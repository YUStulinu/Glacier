// Package scheduler runs Glacier backups on a schedule and keeps their history.
//
// Each job backs up some paths to one repository every `every` (e.g. "24h"); after a successful
// backup it applies the retention policy (forget --keep-last/--keep-daily --prune). A failed run
// is retried after `retry`. History survives restarts in a JSON state file.
package scheduler

import (
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

type Job struct {
	Name         string            `json:"name"`
	Repo         string            `json:"repo"`
	Paths        []string          `json:"paths"`
	Excludes     []string          `json:"excludes,omitempty"`
	Every        string            `json:"every"`
	Retry        string            `json:"retry,omitempty"`
	KeepLast     int               `json:"keep_last,omitempty"`
	KeepDaily    int               `json:"keep_daily,omitempty"`
	PasswordFile string            `json:"password_file"`
	Env          map[string]string `json:"env,omitempty"`
}

type Run struct {
	Job      string          `json:"job"`
	Start    time.Time       `json:"start"`
	End      time.Time       `json:"end"`
	OK       bool            `json:"ok"`
	Error    string          `json:"error,omitempty"`
	Snapshot string          `json:"snapshot,omitempty"`
	Summary  json.RawMessage `json:"summary,omitempty"`
	Pruned   json.RawMessage `json:"pruned,omitempty"`
}

type JobStatus struct {
	Job
	Running bool      `json:"running"`
	LastRun *Run      `json:"last_run,omitempty"`
	NextRun time.Time `json:"next_run"`
}

// Runner executes the glacier binary; replaced in tests.
type Runner func(ctx context.Context, env []string, args ...string) ([]byte, error)

func ExecRunner(binary string) Runner {
	return func(ctx context.Context, env []string, args ...string) ([]byte, error) {
		cmd := exec.CommandContext(ctx, binary, args...)
		cmd.Env = append(os.Environ(), env...)
		out, err := cmd.Output()
		if ee, ok := err.(*exec.ExitError); ok {
			return out, fmt.Errorf("%v: %s", err, strings.TrimSpace(string(ee.Stderr)))
		}
		return out, err
	}
}

type Scheduler struct {
	Jobs      []Job
	Run       Runner
	StateFile string
	Now       func() time.Time

	mu      sync.Mutex
	history []Run
	running map[string]bool
	wake    chan struct{}
}

const maxHistory = 500

func New(jobs []Job, run Runner, stateFile string) (*Scheduler, error) {
	for _, j := range jobs {
		if _, err := time.ParseDuration(j.Every); err != nil {
			return nil, fmt.Errorf("job %s: every: %v", j.Name, err)
		}
		if j.Retry != "" {
			if _, err := time.ParseDuration(j.Retry); err != nil {
				return nil, fmt.Errorf("job %s: retry: %v", j.Name, err)
			}
		}
		if j.Name == "" || j.Repo == "" || len(j.Paths) == 0 || j.PasswordFile == "" {
			return nil, fmt.Errorf("job %q needs name, repo, paths and password_file", j.Name)
		}
	}
	s := &Scheduler{Jobs: jobs, Run: run, StateFile: stateFile, Now: time.Now, running: map[string]bool{}, wake: make(chan struct{}, 1)}
	if b, err := os.ReadFile(stateFile); err == nil {
		json.Unmarshal(b, &s.history)
	}
	return s, nil
}

func (s *Scheduler) lastRun(job string) *Run {
	for i := len(s.history) - 1; i >= 0; i-- {
		if s.history[i].Job == job {
			r := s.history[i]
			return &r
		}
	}
	return nil
}

// NextRun: immediately for a job that never ran; `every` after the last success; `retry` (default
// 10 minutes) after a failure.
func (s *Scheduler) NextRun(j Job) time.Time {
	last := s.lastRun(j.Name)
	if last == nil {
		return time.Time{}
	}
	every, _ := time.ParseDuration(j.Every)
	if last.OK {
		return last.Start.Add(every)
	}
	retry := 10 * time.Minute
	if j.Retry != "" {
		retry, _ = time.ParseDuration(j.Retry)
	}
	return last.End.Add(retry)
}

func (s *Scheduler) Status() []JobStatus {
	s.mu.Lock()
	defer s.mu.Unlock()
	out := make([]JobStatus, 0, len(s.Jobs))
	for _, j := range s.Jobs {
		out = append(out, JobStatus{Job: j, Running: s.running[j.Name], LastRun: s.lastRun(j.Name), NextRun: s.NextRun(j)})
	}
	return out
}

func (s *Scheduler) History(job string) []Run {
	s.mu.Lock()
	defer s.mu.Unlock()
	var out []Run
	for i := len(s.history) - 1; i >= 0; i-- {
		if job == "" || s.history[i].Job == job {
			out = append(out, s.history[i])
		}
	}
	return out
}

func (s *Scheduler) Find(name string) (Job, bool) {
	for _, j := range s.Jobs {
		if j.Name == name {
			return j, true
		}
	}
	return Job{}, false
}

// Env is what the glacier process needs for this job.
func (j Job) EnvList() []string {
	env := []string{}
	keys := make([]string, 0, len(j.Env))
	for k := range j.Env {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	for _, k := range keys {
		env = append(env, k+"="+j.Env[k])
	}
	return env
}

// RunJob performs one backup (and retention) synchronously and records it.
func (s *Scheduler) RunJob(ctx context.Context, j Job) Run {
	s.mu.Lock()
	if s.running[j.Name] {
		s.mu.Unlock()
		return Run{Job: j.Name, Error: "already running"}
	}
	s.running[j.Name] = true
	s.mu.Unlock()
	r := Run{Job: j.Name, Start: s.Now()}
	args := []string{"backup", "--repo", j.Repo, "--password-file", j.PasswordFile, "--json"}
	for _, e := range j.Excludes {
		args = append(args, "--exclude", e)
	}
	args = append(args, j.Paths...)
	out, err := s.Run(ctx, j.EnvList(), args...)
	if err == nil {
		var res struct {
			ID      string          `json:"id"`
			Summary json.RawMessage `json:"summary"`
		}
		if err = json.Unmarshal(out, &res); err == nil {
			r.Snapshot, r.Summary = res.ID, res.Summary
		}
	}
	if err == nil && (j.KeepLast > 0 || j.KeepDaily > 0) {
		fargs := []string{"forget", "--repo", j.Repo, "--password-file", j.PasswordFile, "--prune", "--json"}
		if j.KeepLast > 0 {
			fargs = append(fargs, "--keep-last", strconv.Itoa(j.KeepLast))
		}
		if j.KeepDaily > 0 {
			fargs = append(fargs, "--keep-daily", strconv.Itoa(j.KeepDaily))
		}
		var pout []byte
		if pout, err = s.Run(ctx, j.EnvList(), fargs...); err == nil {
			r.Pruned = json.RawMessage(strings.TrimSpace(string(pout)))
		} else {
			err = fmt.Errorf("retention: %v", err)
		}
	}
	r.End = s.Now()
	r.OK = err == nil
	if err != nil {
		r.Error = err.Error()
	}
	s.mu.Lock()
	s.running[j.Name] = false
	s.history = append(s.history, r)
	if len(s.history) > maxHistory {
		s.history = s.history[len(s.history)-maxHistory:]
	}
	b, _ := json.MarshalIndent(s.history, "", " ")
	s.mu.Unlock()
	if s.StateFile != "" {
		tmp := s.StateFile + ".tmp"
		if os.WriteFile(tmp, b, 0o644) == nil {
			os.Rename(tmp, s.StateFile)
		}
	}
	return r
}

// Due lists jobs whose next run time has come and that are not running.
func (s *Scheduler) Due() []Job {
	s.mu.Lock()
	defer s.mu.Unlock()
	now := s.Now()
	var out []Job
	for _, j := range s.Jobs {
		if !s.running[j.Name] && !s.NextRun(j).After(now) {
			out = append(out, j)
		}
	}
	return out
}

// Trigger asks the loop to check for due jobs now.
func (s *Scheduler) Trigger() {
	select {
	case s.wake <- struct{}{}:
	default:
	}
}

// Loop starts due jobs every `tick` (and when triggered) until ctx ends.
func (s *Scheduler) Loop(ctx context.Context, tick time.Duration) {
	t := time.NewTicker(tick)
	defer t.Stop()
	for {
		for _, j := range s.Due() {
			go s.RunJob(ctx, j)
		}
		select {
		case <-ctx.Done():
			return
		case <-t.C:
		case <-s.wake:
		}
	}
}
