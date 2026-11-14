# Polyglot workers for `examples/polyglot.orbit`

One workflow, four languages. Start the runtime, then each worker in its own
terminal (from the repository root):

```bash
orbit server
```

```bash
PYTHONPATH=sdks/python python3 examples/workers/polyglot/text_worker.py
```

```bash
node examples/workers/polyglot/text_worker.ts
```

```bash
(cd examples/workers/polyglot/go && go run .)
```

```bash
cargo run -p orbit-worker --example text_worker
```

```bash
orbit run examples/polyglot.orbit --input '{"text": "The quick brown fox jumps over the lazy dog. The dog sleeps."}'
```
