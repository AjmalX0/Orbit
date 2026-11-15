// Package orbit is the Go worker SDK for the ORBIT orchestration runtime.
//
// A worker announces the tasks it implements, long-polls the runtime for work,
// runs the handler and reports the result. It uses only the standard library.
//
//	w := orbit.NewWorker(orbit.Options{WorkerID: "inventory-go"})
//	w.Task("inventory.reserve", "1.0.0", func(ctx context.Context, t *orbit.Task) (any, error) {
//		var in struct{ OrderID string `json:"order_id"` }
//		if err := t.Decode(&in); err != nil {
//			return nil, orbit.Permanent("BAD_INPUT", err.Error())
//		}
//		return map[string]any{"reservation_id": "res-" + in.OrderID}, nil
//	})
//	log.Fatal(w.Run(context.Background()))
//
// Delivery is at-least-once: a task can be delivered again after a crash
// (Task.Redelivery). Use Task.IdempotencyKey, which is stable across retries
// and redeliveries, to make side effects idempotent.
package orbit

import (
	"bytes"
	"context"
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"math/rand"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"
)

// Version of this SDK.
const Version = "0.1.0"

const defaultServer = "http://127.0.0.1:7233"

// TaskError is a structured task failure. Return it (or wrap it) from a handler.
type TaskError struct {
	Code      string
	Message   string
	Retryable bool
	Details   any
}

func (e *TaskError) Error() string { return e.Code + ": " + e.Message }

// Retryable returns an error that the runtime may retry according to the step's policy.
func Retryable(code, message string) *TaskError {
	return &TaskError{Code: code, Message: message, Retryable: true}
}

// Permanent returns an error that fails the step immediately, regardless of its retry policy.
func Permanent(code, message string) *TaskError {
	return &TaskError{Code: code, Message: message, Retryable: false}
}

// APIError is an error response from the runtime, or a connection failure (Status 0).
type APIError struct {
	Status  int
	Code    string
	Message string
}

func (e *APIError) Error() string { return fmt.Sprintf("%s (%d): %s", e.Code, e.Status, e.Message) }

func (e *APIError) connection() bool { return e.Status == 0 }

// Task is one delivery of a task to this worker (protocol ExecuteTaskRequest).
type Task struct {
	Token           string          `json:"token"`
	ExecutionID     string          `json:"execution_id"`
	Workflow        string          `json:"workflow"`
	WorkflowVersion string          `json:"workflow_version"`
	StepID          string          `json:"step_id"`
	TaskName        string          `json:"task_name"`
	ContractVersion string          `json:"contract_version"`
	Input           json.RawMessage `json:"input"`
	IdempotencyKey  string          `json:"idempotency_key"`
	Attempt         int             `json:"attempt"`
	Redelivery      bool            `json:"redelivery"`
	DeadlineMs      *int64          `json:"deadline_ms,omitempty"`
	TimeoutMs       *int64          `json:"timeout_ms,omitempty"`
}

// Decode unmarshals the task input into v.
func (t *Task) Decode(v any) error { return json.Unmarshal(t.Input, v) }

// Handler implements a task. Its context is cancelled when the runtime asks the
// attempt to stop (timeout or workflow cancellation) or the worker shuts down.
type Handler func(ctx context.Context, t *Task) (any, error)

// Options configures a Worker. Zero values use sensible defaults and the
// ORBIT_SERVER, ORBIT_WORKER_TOKEN / ORBIT_TOKEN and ORBIT_CA_CERT environment variables.
type Options struct {
	WorkerID   string
	Server     string
	Token      string
	CACertFile string
	// ClientCertFile and ClientKeyFile (PEM) enable mutual TLS: the certificate
	// identifies this worker to the runtime.
	ClientCertFile string
	ClientKeyFile  string
	Concurrency    int
	Runtime        string
	PollWait       time.Duration
	Logger         *log.Logger
}

type registered struct {
	version string
	handler Handler
}

// Worker runs task handlers for the ORBIT runtime.
type Worker struct {
	opts   Options
	http   *http.Client
	tasks  map[string]registered
	logger *log.Logger

	mu             sync.Mutex
	session        string
	heartbeatEvery time.Duration
	active         map[string]context.CancelFunc
}

// NewWorker creates a worker. Register tasks with Task before calling Run.
func NewWorker(opts Options) *Worker {
	if opts.Server == "" {
		opts.Server = os.Getenv("ORBIT_SERVER")
	}
	if opts.Server == "" {
		opts.Server = defaultServer
	}
	opts.Server = strings.TrimRight(opts.Server, "/")
	if opts.Token == "" {
		opts.Token = os.Getenv("ORBIT_WORKER_TOKEN")
	}
	if opts.Token == "" {
		opts.Token = os.Getenv("ORBIT_TOKEN")
	}
	if opts.CACertFile == "" {
		opts.CACertFile = os.Getenv("ORBIT_CA_CERT")
	}
	if opts.ClientCertFile == "" {
		opts.ClientCertFile = os.Getenv("ORBIT_CLIENT_CERT")
	}
	if opts.ClientKeyFile == "" {
		opts.ClientKeyFile = os.Getenv("ORBIT_CLIENT_KEY")
	}
	if opts.WorkerID == "" {
		host, _ := os.Hostname()
		opts.WorkerID = fmt.Sprintf("go-%s-%d", host, os.Getpid())
	}
	if opts.Concurrency <= 0 {
		opts.Concurrency = 4
	}
	if opts.Runtime == "" {
		opts.Runtime = "go"
	}
	if opts.PollWait <= 0 {
		opts.PollWait = 20 * time.Second
	}
	logger := opts.Logger
	if logger == nil {
		logger = log.New(os.Stderr, "[orbit] ", log.LstdFlags)
	}
	transport := http.DefaultTransport.(*http.Transport).Clone()
	if opts.CACertFile != "" || opts.ClientCertFile != "" {
		transport.TLSClientConfig = &tls.Config{MinVersion: tls.VersionTLS12}
	}
	if opts.ClientCertFile != "" {
		pair, err := tls.LoadX509KeyPair(opts.ClientCertFile, opts.ClientKeyFile)
		if err != nil {
			logger.Printf("cannot load client certificate %s: %v", opts.ClientCertFile, err)
		} else {
			transport.TLSClientConfig.Certificates = []tls.Certificate{pair}
		}
	}
	if opts.CACertFile != "" {
		pool, err := x509.SystemCertPool()
		if err != nil || pool == nil {
			pool = x509.NewCertPool()
		}
		if pem, err := os.ReadFile(opts.CACertFile); err == nil {
			pool.AppendCertsFromPEM(pem)
		} else {
			logger.Printf("cannot read CA certificate %s: %v", opts.CACertFile, err)
		}
		transport.TLSClientConfig.RootCAs = pool
	}
	return &Worker{
		opts:           opts,
		http:           &http.Client{Transport: transport, Timeout: opts.PollWait + 15*time.Second},
		tasks:          map[string]registered{},
		logger:         logger,
		heartbeatEvery: 5 * time.Second,
		active:         map[string]context.CancelFunc{},
	}
}

// Task registers handler as the implementation of name@version.
func (w *Worker) Task(name, version string, handler Handler) *Worker {
	if _, dup := w.tasks[name]; dup {
		panic("orbit: task " + name + " registered twice")
	}
	w.tasks[name] = registered{version: version, handler: handler}
	return w
}

func (w *Worker) call(ctx context.Context, path string, body any, out any) (int, error) {
	payload, err := json.Marshal(body)
	if err != nil {
		return 0, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, w.opts.Server+path, bytes.NewReader(payload))
	if err != nil {
		return 0, err
	}
	req.Header.Set("content-type", "application/json")
	if w.opts.Token != "" {
		req.Header.Set("authorization", "Bearer "+w.opts.Token)
	}
	resp, err := w.http.Do(req)
	if err != nil {
		return 0, &APIError{Status: 0, Code: "CONNECTION", Message: fmt.Sprintf("cannot reach ORBIT runtime at %s: %v", w.opts.Server, err)}
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode >= 400 {
		var e struct {
			Error struct{ Code, Message string } `json:"error"`
		}
		_ = json.Unmarshal(raw, &e)
		if e.Error.Code == "" {
			e.Error.Code, e.Error.Message = "HTTP_ERROR", string(raw)
		}
		return resp.StatusCode, &APIError{Status: resp.StatusCode, Code: e.Error.Code, Message: e.Error.Message}
	}
	if out != nil && len(raw) > 0 {
		if err := json.Unmarshal(raw, out); err != nil {
			return resp.StatusCode, err
		}
	}
	return resp.StatusCode, nil
}

func (w *Worker) register(ctx context.Context) (string, error) {
	type contract struct {
		TaskName        string `json:"task_name"`
		ContractVersion string `json:"contract_version"`
	}
	contracts := make([]contract, 0, len(w.tasks))
	for name, t := range w.tasks {
		contracts = append(contracts, contract{name, t.version})
	}
	var r struct {
		Session   string   `json:"worker_session_id"`
		Heartbeat int64    `json:"heartbeat_interval_ms"`
		Warnings  []string `json:"warnings"`
	}
	_, err := w.call(ctx, "/v1/workers/register", map[string]any{
		"worker_id": w.opts.WorkerID, "runtime": w.opts.Runtime, "sdk": "orbit-go/" + Version, "contracts": contracts,
	}, &r)
	if err != nil {
		return "", err
	}
	w.mu.Lock()
	w.session = r.Session
	if r.Heartbeat > 0 {
		w.heartbeatEvery = time.Duration(max64(r.Heartbeat, 500)) * time.Millisecond
	}
	w.mu.Unlock()
	for _, warning := range r.Warnings {
		w.logger.Printf("runtime: %s", warning)
	}
	w.logger.Printf("worker %s registered (%d tasks)", w.opts.WorkerID, len(w.tasks))
	return r.Session, nil
}

// withSession runs fn with the current session, re-registering once if the
// runtime no longer knows it (for example after a restart).
func (w *Worker) withSession(ctx context.Context, fn func(session string) (int, error)) (int, error) {
	w.mu.Lock()
	session := w.session
	w.mu.Unlock()
	if session == "" {
		var err error
		if session, err = w.register(ctx); err != nil {
			return 0, err
		}
	}
	status, err := fn(session)
	var apiErr *APIError
	if errors.As(err, &apiErr) && apiErr.Status == 404 && apiErr.Code == "UNKNOWN_SESSION" {
		if session, err = w.register(ctx); err != nil {
			return 0, err
		}
		return fn(session)
	}
	return status, err
}

func sleepCtx(ctx context.Context, d time.Duration) bool {
	t := time.NewTimer(d)
	defer t.Stop()
	select {
	case <-ctx.Done():
		return false
	case <-t.C:
		return true
	}
}

func max64(a, b int64) int64 {
	if a > b {
		return a
	}
	return b
}

// retrying calls fn until it succeeds, fails with a non-retryable API error,
// or stop is cancelled. Connection errors and 5xx responses are retried.
func (w *Worker) retrying(stop context.Context, fn func() error) error {
	delay := 200 * time.Millisecond
	for {
		err := fn()
		var apiErr *APIError
		if err == nil || !errors.As(err, &apiErr) || (!apiErr.connection() && apiErr.Status < 500) {
			return err
		}
		if stop.Err() != nil {
			w.logger.Printf("%v; giving up because the worker is stopping", err)
			return err
		}
		w.logger.Printf("%v; retrying in %v", err, delay)
		sleepCtx(stop, delay)
		delay = time.Duration(float64(min(delay*2, 5*time.Second)) * (0.8 + rand.Float64()*0.4))
	}
}

// Run polls for work until ctx is cancelled, then finishes in-flight tasks and unregisters.
func (w *Worker) Run(ctx context.Context) error {
	if len(w.tasks) == 0 {
		return errors.New("orbit: no tasks registered")
	}
	if err := w.retrying(ctx, func() error { _, err := w.register(ctx); return err }); err != nil {
		return err
	}
	// Results are delivered with a context that outlives ctx, so work finished
	// during shutdown is still reported.
	report := context.WithoutCancel(ctx)
	var wg sync.WaitGroup
	wg.Add(1)
	go func() { defer wg.Done(); w.heartbeatLoop(ctx) }()
	for i := 0; i < w.opts.Concurrency; i++ {
		wg.Add(1)
		go func() { defer wg.Done(); w.pollLoop(ctx, report) }()
	}
	wg.Wait()
	w.mu.Lock()
	session := w.session
	w.mu.Unlock()
	if session != "" {
		_, _ = w.call(report, "/v1/workers/unregister", map[string]string{"worker_session_id": session}, nil)
	}
	w.logger.Printf("worker %s stopped", w.opts.WorkerID)
	return nil
}

func (w *Worker) heartbeatLoop(ctx context.Context) {
	for {
		w.mu.Lock()
		every := w.heartbeatEvery
		w.mu.Unlock()
		if !sleepCtx(ctx, every) {
			return
		}
		var r struct {
			Cancel []string `json:"cancel"`
		}
		_, err := w.withSession(ctx, func(s string) (int, error) {
			return w.call(ctx, "/v1/workers/heartbeat", map[string]string{"worker_session_id": s}, &r)
		})
		if err != nil {
			continue
		}
		w.mu.Lock()
		for _, token := range r.Cancel {
			if cancel, ok := w.active[token]; ok {
				w.logger.Printf("cancellation requested for %s", token[:min(8, len(token))])
				cancel()
			}
		}
		w.mu.Unlock()
	}
}

func (w *Worker) pollLoop(ctx, report context.Context) {
	backoff := 200 * time.Millisecond
	for ctx.Err() == nil {
		var task Task
		status, err := w.withSession(ctx, func(s string) (int, error) {
			return w.call(ctx, "/v1/workers/poll", map[string]any{"worker_session_id": s, "wait_ms": w.opts.PollWait.Milliseconds()}, &task)
		})
		if err != nil {
			if ctx.Err() != nil {
				return
			}
			w.logger.Printf("poll failed: %v; retrying in %v", err, backoff)
			sleepCtx(ctx, backoff)
			backoff = min(backoff*2, 5*time.Second)
			continue
		}
		backoff = 200 * time.Millisecond
		if status == http.StatusNoContent || task.Token == "" {
			continue
		}
		w.execute(ctx, report, &task)
	}
}

func (w *Worker) execute(ctx, report context.Context, task *Task) {
	taskCtx, cancel := context.WithCancel(ctx)
	if task.DeadlineMs != nil {
		var cancelDeadline context.CancelFunc
		taskCtx, cancelDeadline = context.WithDeadline(taskCtx, time.UnixMilli(*task.DeadlineMs))
		defer cancelDeadline()
	}
	w.mu.Lock()
	w.active[task.Token] = cancel
	w.mu.Unlock()
	defer func() {
		w.mu.Lock()
		delete(w.active, task.Token)
		w.mu.Unlock()
		cancel()
	}()

	output, err := w.invoke(taskCtx, task)
	if err == nil {
		if output == nil {
			output = map[string]any{}
		}
		raw, mErr := json.Marshal(output)
		if mErr != nil || !bytes.HasPrefix(bytes.TrimSpace(raw), []byte("{")) {
			err = Permanent("INVALID_RESULT", "tasks must return a JSON object")
		} else {
			w.deliver(report, "/v1/tasks/complete", map[string]any{"token": task.Token, "output": json.RawMessage(raw)})
			return
		}
	}
	var te *TaskError
	if !errors.As(err, &te) {
		te = &TaskError{Code: "UNHANDLED_EXCEPTION", Message: err.Error(), Retryable: true}
	}
	w.deliver(report, "/v1/tasks/fail", map[string]any{
		"token": task.Token, "error_code": te.Code, "error_message": te.Message, "retryable": te.Retryable, "details": te.Details,
	})
}

func (w *Worker) invoke(ctx context.Context, task *Task) (out any, err error) {
	impl, ok := w.tasks[task.TaskName]
	if !ok {
		return nil, Retryable("UNKNOWN_TASK", "worker does not implement "+task.TaskName)
	}
	defer func() {
		if r := recover(); r != nil {
			err = &TaskError{Code: "PANIC", Message: fmt.Sprint(r), Retryable: true}
		}
	}()
	return impl.handler(ctx, task)
}

// deliver reports a result, retrying through outages so that the result
// survives a runtime restart (the runtime reconciles it).
func (w *Worker) deliver(ctx context.Context, path string, body any) {
	var ack struct {
		Accepted bool   `json:"accepted"`
		Reason   string `json:"reason"`
	}
	stop, cancel := context.WithTimeout(ctx, 10*time.Minute)
	defer cancel()
	err := w.retrying(stop, func() error { _, err := w.call(ctx, path, body, &ack); return err })
	if err != nil {
		w.logger.Printf("could not deliver result: %v", err)
	} else if !ack.Accepted {
		w.logger.Printf("runtime ignored result: %s", ack.Reason)
	}
}
