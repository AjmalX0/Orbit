# Java worker for `examples/java_hello.orbit`

Implements `greeting.java` with the [Java SDK](../../../sdks/java). From the
repository root:

```bash
(cd sdks/java && mvn -q install)              # puts orbit-sdk 0.1.0 in ~/.m2
orbit server &                                # or: cargo run -p orbit-cli -- server
(cd examples/workers/java && mvn -q compile exec:java)
orbit run examples/java_hello.orbit --input '{"name": "Ada", "excited": true}'
# {"message": "Hello, Ada, from Java!", "letters": 3}
```

Set `ORBIT_SERVER` to reach a runtime elsewhere than `http://127.0.0.1:7233`.
