# ORBIT Go SDK

Worker SDK for the [ORBIT](../../README.md) runtime. Standard library only, Go 1.21+.

```go
import orbit "github.com/AjmalX0/Orbit/sdks/go"

w := orbit.NewWorker(orbit.Options{WorkerID: "inventory-go", Concurrency: 8})
w.Task("inventory.reserve", "1.0.0", func(ctx context.Context, t *orbit.Task) (any, error) {
	var in struct {
		OrderID string `json:"order_id"`
	}
	if err := t.Decode(&in); err != nil {
		return nil, orbit.Permanent("INVALID_INPUT", err.Error())
	}
	// t.IdempotencyKey is stable across retries and redeliveries.
	return map[string]any{"reservation_id": "res-" + in.OrderID}, nil
})
ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
defer stop()
log.Fatal(w.Run(ctx)) // returns after in-flight tasks finish and the worker unregisters
```

* The handler's `ctx` is cancelled when the runtime cancels the attempt
  (timeout or workflow cancellation), when the attempt's deadline passes, or
  when the worker shuts down.
* Return `orbit.Permanent(code, msg)` or `orbit.Retryable(code, msg)` for
  business errors. Other errors are reported as retryable `UNHANDLED_ERROR`,
  and panics as retryable `PANIC`.
* Configuration comes from `Options` or from `ORBIT_SERVER`,
  `ORBIT_WORKER_TOKEN` / `ORBIT_TOKEN` and `ORBIT_CA_CERT`.
* Tests: `go test -race ./...`
