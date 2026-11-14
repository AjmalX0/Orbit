package orbit

import (
	"context"
	"encoding/json"
	"io"
	"log"
	"net/http"
	"net/http/httptest"
	"sync"
	"testing"
	"time"
)

// fakeRuntime is a scripted ORBIT runtime implementing the worker protocol.
type fakeRuntime struct {
	mu            sync.Mutex
	tasks         []map[string]any
	registrations int
	forgetOnce    bool // answer the next poll with UNKNOWN_SESSION
	failComplete  int  // answer this many /complete calls with 503
	completed     map[string]json.RawMessage
	failed        map[string]map[string]any
	cancel        []string
	authHeader    string
}

func newFake() *fakeRuntime {
	return &fakeRuntime{completed: map[string]json.RawMessage{}, failed: map[string]map[string]any{}}
}

func (f *fakeRuntime) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	body, _ := io.ReadAll(r.Body)
	var req map[string]json.RawMessage
	_ = json.Unmarshal(body, &req)
	f.mu.Lock()
	defer f.mu.Unlock()
	f.authHeader = r.Header.Get("authorization")
	reply := func(status int, v any) {
		w.Header().Set("content-type", "application/json")
		w.WriteHeader(status)
		_ = json.NewEncoder(w).Encode(v)
	}
	switch r.URL.Path {
	case "/v1/workers/register":
		f.registrations++
		reply(200, map[string]any{"accepted": true, "worker_session_id": "s", "heartbeat_interval_ms": 500})
	case "/v1/workers/poll":
		if f.forgetOnce {
			f.forgetOnce = false
			reply(404, map[string]any{"error": map[string]string{"code": "UNKNOWN_SESSION", "message": "gone"}})
			return
		}
		if len(f.tasks) == 0 {
			f.mu.Unlock()
			time.Sleep(20 * time.Millisecond)
			f.mu.Lock()
			w.WriteHeader(204)
			return
		}
		t := f.tasks[0]
		f.tasks = f.tasks[1:]
		reply(200, t)
	case "/v1/workers/heartbeat":
		reply(200, map[string]any{"accepted": true, "cancel": f.cancel})
	case "/v1/tasks/complete":
		if f.failComplete > 0 {
			f.failComplete--
			reply(503, map[string]any{"error": map[string]string{"code": "UNAVAILABLE", "message": "try later"}})
			return
		}
		var tok string
		_ = json.Unmarshal(req["token"], &tok)
		f.completed[tok] = req["output"]
		reply(200, map[string]any{"accepted": true})
	case "/v1/tasks/fail":
		var m map[string]any
		_ = json.Unmarshal(body, &m)
		f.failed[m["token"].(string)] = m
		reply(200, map[string]any{"accepted": true})
	default:
		reply(200, map[string]any{"accepted": true})
	}
}

func task(token, name string, input any) map[string]any {
	raw, _ := json.Marshal(input)
	return map[string]any{"token": token, "execution_id": "e", "step_id": token, "task_name": name,
		"contract_version": "1.0.0", "input": json.RawMessage(raw), "idempotency_key": "e/" + token, "attempt": 1}
}

func waitFor(t *testing.T, cond func() bool) {
	t.Helper()
	deadline := time.Now().Add(10 * time.Second)
	for !cond() {
		if time.Now().After(deadline) {
			t.Fatal("condition not reached")
		}
		time.Sleep(10 * time.Millisecond)
	}
}

func TestWorkerProtocol(t *testing.T) {
	f := newFake()
	f.forgetOnce = true
	f.failComplete = 2
	f.tasks = []map[string]any{
		task("ok", "echo", map[string]any{"n": 1}),
		task("biz", "fail", nil),
		task("boom", "panic", nil),
		task("slow", "slow", nil),
	}
	srv := httptest.NewServer(f)
	defer srv.Close()

	w := NewWorker(Options{Server: srv.URL, Token: "wt", WorkerID: "go-test", Concurrency: 2,
		PollWait: 100 * time.Millisecond, Logger: log.New(io.Discard, "", 0)})
	w.Task("echo", "1.0.0", func(ctx context.Context, tk *Task) (any, error) {
		var in struct{ N int }
		if err := tk.Decode(&in); err != nil {
			return nil, err
		}
		return map[string]int{"n": in.N + 1}, nil
	})
	w.Task("fail", "1.0.0", func(context.Context, *Task) (any, error) {
		return nil, Permanent("CARD_DECLINED", "no funds")
	})
	w.Task("panic", "1.0.0", func(context.Context, *Task) (any, error) { panic("kaboom") })
	w.Task("slow", "1.0.0", func(ctx context.Context, tk *Task) (any, error) {
		f.mu.Lock()
		f.cancel = []string{tk.Token}
		f.mu.Unlock()
		<-ctx.Done() // released by the heartbeat's cancel list
		return nil, Permanent("CANCELLED", "stopped")
	})

	ctx, stop := context.WithCancel(context.Background())
	done := make(chan error)
	go func() { done <- w.Run(ctx) }()

	waitFor(t, func() bool { f.mu.Lock(); defer f.mu.Unlock(); return len(f.completed) == 1 && len(f.failed) == 3 })
	stop()
	if err := <-done; err != nil {
		t.Fatal(err)
	}

	f.mu.Lock()
	defer f.mu.Unlock()
	if string(f.completed["ok"]) != `{"n":2}` {
		t.Errorf("output = %s", f.completed["ok"])
	}
	if f.registrations < 2 {
		t.Errorf("expected re-registration after UNKNOWN_SESSION, got %d registrations", f.registrations)
	}
	if got := f.failed["biz"]; got["error_code"] != "CARD_DECLINED" || got["retryable"] != false {
		t.Errorf("business error mapped wrongly: %v", got)
	}
	if got := f.failed["boom"]; got["error_code"] != "PANIC" || got["retryable"] != true {
		t.Errorf("panic mapped wrongly: %v", got)
	}
	if got := f.failed["slow"]; got["error_code"] != "CANCELLED" {
		t.Errorf("cancellation not delivered: %v", got)
	}
	if f.authHeader != "Bearer wt" {
		t.Errorf("auth header = %q", f.authHeader)
	}
}
