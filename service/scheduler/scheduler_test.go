package scheduler

import (
	"context"
	"errors"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

type fakeClock struct{ t time.Time }

func (c *fakeClock) now() time.Time { return c.t }

func job() Job {
	return Job{Name: "docs", Repo: "/r", Paths: []string{"/home/docs"}, Excludes: []string{"node_modules"}, Every: "24h", Retry: "1h", KeepLast: 3, PasswordFile: "/pw", Env: map[string]string{"B": "2", "A": "1"}}
}

func TestScheduleAndHistory(t *testing.T) {
	var calls [][]string
	fail := false
	run := func(ctx context.Context, env []string, args ...string) ([]byte, error) {
		calls = append(calls, append(append([]string{}, env...), args...))
		if fail {
			return nil, errors.New("disk full")
		}
		if args[0] == "backup" {
			return []byte(`{"id":"abc123","summary":{"files":3}}`), nil
		}
		return []byte(`{"forgotten":[]}`), nil
	}
	state := filepath.Join(t.TempDir(), "state.json")
	s, err := New([]Job{job()}, run, state)
	if err != nil {
		t.Fatal(err)
	}
	clk := &fakeClock{time.Date(2026, 11, 10, 12, 0, 0, 0, time.UTC)}
	s.Now = clk.now
	if len(s.Due()) != 1 {
		t.Fatal("a job that never ran should be due")
	}
	r := s.RunJob(context.Background(), job())
	if !r.OK || r.Snapshot != "abc123" || string(r.Summary) != `{"files":3}` {
		t.Fatalf("%+v", r)
	}
	got := strings.Join(calls[0], " ")
	if got != "A=1 B=2 backup --repo /r --password-file /pw --json --exclude node_modules /home/docs" {
		t.Fatal(got)
	}
	if !strings.Contains(strings.Join(calls[1], " "), "forget --repo /r --password-file /pw --prune --json --keep-last 3") {
		t.Fatal(calls[1])
	}
	if len(s.Due()) != 0 {
		t.Fatal("due again right after a success")
	}
	clk.t = clk.t.Add(24 * time.Hour)
	if len(s.Due()) != 1 {
		t.Fatal("not due after 24 h")
	}
	// A failure is retried after an hour, not a day.
	fail = true
	r = s.RunJob(context.Background(), job())
	if r.OK || !strings.Contains(r.Error, "disk full") {
		t.Fatalf("%+v", r)
	}
	clk.t = clk.t.Add(59 * time.Minute)
	if len(s.Due()) != 0 {
		t.Fatal("retried too early")
	}
	clk.t = clk.t.Add(2 * time.Minute)
	if len(s.Due()) != 1 {
		t.Fatal("not retried")
	}
	// History survives a restart.
	s2, _ := New([]Job{job()}, run, state)
	if h := s2.History("docs"); len(h) != 2 || h[0].OK || !h[1].OK {
		t.Fatalf("%+v", h)
	}
}

func TestValidation(t *testing.T) {
	bad := job()
	bad.Every = "daily"
	if _, err := New([]Job{bad}, nil, ""); err == nil {
		t.Fatal("bad duration accepted")
	}
	bad = job()
	bad.PasswordFile = ""
	if _, err := New([]Job{bad}, nil, ""); err == nil {
		t.Fatal("missing password file accepted")
	}
}
