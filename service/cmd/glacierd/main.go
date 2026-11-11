// Command glacierd runs scheduled Glacier backups and serves the web interface.
//
//	glacierd -config glacierd.json
//
// Config:
//
//	{
//	  "glacier": "../core/target/release/glacier",
//	  "listen": "127.0.0.1:8260",
//	  "web": "../web/dist",
//	  "state": "glacierd-state.json",
//	  "jobs": [{"name": "projects", "repo": "D:/backups/projects", "paths": ["C:/work"],
//	            "excludes": ["node_modules", "target"], "every": "24h", "keep_last": 7,
//	            "keep_daily": 30, "password_file": "C:/secrets/glacier.txt"}]
//	}
//
// For an S3 destination, set "repo" to "s3:http://host:port/bucket/prefix" and put
// AWS_ACCESS_KEY_ID / AWS_SECRET_ACCESS_KEY in the job's "env".
//
// API:
//
//	GET  /api/jobs                 jobs with last and next run
//	POST /api/jobs/<name>/run      start a backup now
//	GET  /api/runs[?job=]          run history, newest first
//	*    /api/repo/<name>/...      the job repository's browse/restore API (`glacier serve`)
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"log"
	"net"
	"net/http"
	"net/http/httputil"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"github.com/YUStulinu/Glacier/service/scheduler"
)

type Config struct {
	Glacier string          `json:"glacier"`
	Listen  string          `json:"listen"`
	Web     string          `json:"web"`
	State   string          `json:"state"`
	Jobs    []scheduler.Job `json:"jobs"`
}

type App struct {
	cfg   Config
	sched *scheduler.Scheduler
	mu    sync.Mutex
	// One `glacier serve` per job, started on first use.
	browsers map[string]*httputil.ReverseProxy
}

func freePort() (int, error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return 0, err
	}
	defer l.Close()
	return l.Addr().(*net.TCPAddr).Port, nil
}

func (a *App) browser(j scheduler.Job) (*httputil.ReverseProxy, error) {
	a.mu.Lock()
	defer a.mu.Unlock()
	if p, ok := a.browsers[j.Name]; ok {
		return p, nil
	}
	port, err := freePort()
	if err != nil {
		return nil, err
	}
	addr := fmt.Sprintf("127.0.0.1:%d", port)
	cmd := exec.Command(a.cfg.Glacier, "serve", "--repo", j.Repo, "--password-file", j.PasswordFile, "--addr", addr)
	cmd.Env = append(os.Environ(), j.EnvList()...)
	cmd.Stderr = os.Stderr
	if err := cmd.Start(); err != nil {
		return nil, err
	}
	go cmd.Wait()
	for i := 0; i < 100; i++ {
		if c, err := net.Dial("tcp", addr); err == nil {
			c.Close()
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	u, _ := url.Parse("http://" + addr)
	p := httputil.NewSingleHostReverseProxy(u)
	a.browsers[j.Name] = p
	return p, nil
}

func writeJSON(w http.ResponseWriter, code int, v any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(code)
	json.NewEncoder(w).Encode(v)
}

func (a *App) Routes() http.Handler {
	mux := http.NewServeMux()
	mux.HandleFunc("GET /api/jobs", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, 200, a.sched.Status())
	})
	mux.HandleFunc("POST /api/jobs/{name}/run", func(w http.ResponseWriter, r *http.Request) {
		j, ok := a.sched.Find(r.PathValue("name"))
		if !ok {
			writeJSON(w, 404, map[string]string{"error": "no such job"})
			return
		}
		go a.sched.RunJob(context.Background(), j)
		writeJSON(w, 202, map[string]string{"status": "started"})
	})
	mux.HandleFunc("GET /api/runs", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, 200, a.sched.History(r.URL.Query().Get("job")))
	})
	mux.HandleFunc("/api/repo/{name}/", func(w http.ResponseWriter, r *http.Request) {
		j, ok := a.sched.Find(r.PathValue("name"))
		if !ok {
			writeJSON(w, 404, map[string]string{"error": "no such job"})
			return
		}
		p, err := a.browser(j)
		if err != nil {
			writeJSON(w, 500, map[string]string{"error": err.Error()})
			return
		}
		r.URL.Path = "/api/" + strings.TrimPrefix(r.URL.Path, "/api/repo/"+j.Name+"/")
		p.ServeHTTP(w, r)
	})
	if a.cfg.Web != "" {
		fs := http.FileServer(http.Dir(a.cfg.Web))
		mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
			if _, err := os.Stat(filepath.Join(a.cfg.Web, filepath.FromSlash(r.URL.Path))); err != nil {
				r.URL.Path = "/"
			}
			fs.ServeHTTP(w, r)
		})
	}
	return mux
}

func main() {
	cfgPath := flag.String("config", "glacierd.json", "configuration file")
	flag.Parse()
	b, err := os.ReadFile(*cfgPath)
	if err != nil {
		log.Fatal(err)
	}
	var cfg Config
	if err := json.Unmarshal(b, &cfg); err != nil {
		log.Fatalf("%s: %v", *cfgPath, err)
	}
	if cfg.Listen == "" {
		cfg.Listen = "127.0.0.1:8260"
	}
	if cfg.State == "" {
		cfg.State = "glacierd-state.json"
	}
	s, err := scheduler.New(cfg.Jobs, scheduler.ExecRunner(cfg.Glacier), cfg.State)
	if err != nil {
		log.Fatal(err)
	}
	app := &App{cfg: cfg, sched: s, browsers: map[string]*httputil.ReverseProxy{}}
	go s.Loop(context.Background(), 30*time.Second)
	log.Printf("glacierd on http://%s, %d jobs", cfg.Listen, len(cfg.Jobs))
	log.Fatal(http.ListenAndServe(cfg.Listen, app.Routes()))
}
