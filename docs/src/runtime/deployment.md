# Deployment

The ORBIT runtime is one self-contained binary with no runtime dependencies apart from its database. You can run it from the container image `ghcr.io/ajmalx0/orbit`, from a release binary under a service manager such as systemd, or from a binary you build yourself. This page covers each option, a Docker Compose setup with PostgreSQL, sizing, and upgrades. Configure security before you expose the runtime; see [Security](security.md).

## Container image

The image is published to the GitHub Container Registry for every release:

| Tag | Meaning |
|---|---|
| `ghcr.io/ajmalx0/orbit:v0.1.0` | A specific release. Use this in production. |
| `ghcr.io/ajmalx0/orbit:latest` | The most recent release. |

The image is built for `linux/amd64` from the repository's `Dockerfile`:

| Property | Value |
|---|---|
| Base | `debian:bookworm-slim` with `ca-certificates` |
| Binary | `/usr/local/bin/orbit` (release build, stripped) |
| User | `orbit`, UID `10001`, home `/data` |
| Working directory and volume | `/data` |
| Entrypoint | `orbit` |
| Default command | `server` |
| Environment | `ORBIT_DB=/data/orbit.db`, `ORBIT_ADDR=0.0.0.0:7233` |
| Exposed ports | `7233` (HTTP API), `7234` (gRPC, when you set `ORBIT_GRPC_ADDR`) |

Because the image listens on `0.0.0.0`, the server refuses to start without a token:

```console
$ docker run --rm ghcr.io/ajmalx0/orbit:v0.1.0
error: refusing to listen on 0.0.0.0:7233 without authentication; pass --token (or ORBIT_TOKEN), or --insecure for trusted networks
```

### docker run

```bash
docker volume create orbit-data
docker run -d --name orbit --restart unless-stopped \
  --stop-signal SIGINT --stop-timeout 75 \
  -p 7233:7233 \
  -e ORBIT_TOKEN="$ORBIT_TOKEN" \
  -e ORBIT_WORKER_TOKEN="$ORBIT_WORKER_TOKEN" \
  -v orbit-data:/data \
  ghcr.io/ajmalx0/orbit:v0.1.0 server --log-format json
```

- `--stop-signal SIGINT` makes `docker stop` shut the server down gracefully with every version. Version 0.1.0 does not handle `SIGTERM`, the signal `docker stop` sends by default, and in a container the server is process 1, which ignores signals it has no handler for; later versions handle `SIGTERM` and their image sets `STOPSIGNAL SIGINT`. `--stop-timeout 75` leaves time for open worker long polls (up to 60 seconds) to finish. Killing the server loses no committed state; the graceful path only avoids interrupting requests.
- Arguments after the image name replace the default command, so start them with `server`.
- The entrypoint is `orbit`, so the same image runs any CLI command: `docker run --rm ghcr.io/ajmalx0/orbit:v0.1.0 --version`.

Check it from the host:

```bash
curl -s http://127.0.0.1:7233/v1/health
orbit ps --server http://127.0.0.1:7233 --token "$ORBIT_TOKEN"
```

To use TLS, mount the certificates and the workers file read-only:

```bash
docker run -d --name orbit --restart unless-stopped \
  --stop-signal SIGINT --stop-timeout 75 \
  -p 7233:7233 \
  -e ORBIT_TOKEN="$ORBIT_TOKEN" \
  -e ORBIT_TLS_CERT=/etc/orbit/tls/server.pem \
  -e ORBIT_TLS_KEY=/etc/orbit/tls/server.key \
  -e ORBIT_WORKERS_FILE=/etc/orbit/workers.toml \
  -v orbit-data:/data \
  -v /etc/orbit:/etc/orbit:ro \
  ghcr.io/ajmalx0/orbit:v0.1.0
```

The files must be readable by UID 10001.

### Docker Compose with PostgreSQL

```yaml
# compose.yaml
services:
  postgres:
    image: postgres:16
    environment:
      POSTGRES_USER: orbit
      POSTGRES_PASSWORD: ${POSTGRES_PASSWORD}
      POSTGRES_DB: orbit
    volumes:
      - pg-data:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U orbit -d orbit"]
      interval: 5s
      retries: 10

  orbit:
    image: ghcr.io/ajmalx0/orbit:v0.1.0
    restart: unless-stopped
    stop_signal: SIGINT
    stop_grace_period: 75s
    depends_on:
      postgres:
        condition: service_healthy
    environment:
      ORBIT_DB: postgres://orbit:${POSTGRES_PASSWORD}@postgres:5432/orbit?sslmode=disable
      ORBIT_TOKEN: ${ORBIT_TOKEN}
      ORBIT_WORKER_TOKEN: ${ORBIT_WORKER_TOKEN}
      ORBIT_GRPC_ADDR: 0.0.0.0:7234
      OTEL_EXPORTER_OTLP_ENDPOINT: http://otel-collector:4318
      OTEL_SERVICE_NAME: orbit
    command: ["server", "--log-format", "json"]
    ports:
      - "7233:7233"
      - "7234:7234"

  payments-worker:
    build: ./workers/payments
    restart: unless-stopped
    environment:
      ORBIT_SERVER: http://orbit:7233
      ORBIT_WORKER_TOKEN: ${ORBIT_WORKER_TOKEN}
    depends_on: [orbit]

volumes:
  pg-data:
```

Put `POSTGRES_PASSWORD`, `ORBIT_TOKEN` and `ORBIT_WORKER_TOKEN` in an `.env` file next to `compose.yaml`, or in your secret manager. `sslmode=disable` is appropriate here only because PostgreSQL is on the same private Compose network; use `sslmode=require` for a database on another host. Remove the `OTEL_*` lines if you do not run a collector. The image contains no HTTP client such as `curl` or `wget`, so probe `/v1/health` from outside the container (a load balancer, an orchestrator probe or your monitoring system) rather than with a Compose `healthcheck`.

### Kubernetes

Run exactly one replica:

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: orbit
spec:
  replicas: 1
  strategy:
    type: Recreate            # never two servers on one database
  selector:
    matchLabels: { app: orbit }
  template:
    metadata:
      labels: { app: orbit }
    spec:
      securityContext:
        runAsUser: 10001
        fsGroup: 10001
      containers:
        - name: orbit
          image: ghcr.io/ajmalx0/orbit:v0.1.0
          args: ["server", "--log-format", "json"]
          envFrom:
            - secretRef: { name: orbit }   # ORBIT_TOKEN, ORBIT_WORKER_TOKEN, ORBIT_DB
          ports:
            - { containerPort: 7233, name: http }
          readinessProbe:
            httpGet: { path: /v1/health, port: http }
          livenessProbe:
            httpGet: { path: /v1/health, port: http }
            periodSeconds: 10
          lifecycle:
            preStop:
              exec: { command: ["/bin/sh", "-c", "kill -INT 1"] }
          volumeMounts:
            - { name: data, mountPath: /data }
      volumes:
        - name: data
          persistentVolumeClaim: { claimName: orbit-data }
```

The `preStop` hook sends `SIGINT` for a graceful shutdown; without it, the pod is killed after its termination grace period, which is also safe. With PostgreSQL, set `ORBIT_DB` in the secret and drop the volume. With SQLite, use a `ReadWriteOnce` volume on block storage.

## Release binaries

Every GitHub release has prebuilt binaries and a `SHA256SUMS` file:

| Asset | Platform |
|---|---|
| `orbit-x86_64-unknown-linux-gnu.tar.gz` | Linux x86_64 (glibc) |
| `orbit-aarch64-unknown-linux-gnu.tar.gz` | Linux arm64 (glibc) |
| `orbit-x86_64-apple-darwin.tar.gz` | macOS Intel |
| `orbit-aarch64-apple-darwin.tar.gz` | macOS Apple silicon |
| `orbit-x86_64-pc-windows-msvc.zip` | Windows x86_64 |

The install script downloads the right archive for Linux and macOS, verifies its checksum and installs it:

```bash
curl -fsSL https://github.com/AjmalX0/Orbit/releases/latest/download/install.sh | sh
# a specific version, system-wide:
curl -fsSL https://github.com/AjmalX0/Orbit/releases/latest/download/install.sh \
  | sudo env ORBIT_VERSION=v0.1.0 sh
```

It installs to `~/.local/bin`, or `/usr/local/bin` as root; set `ORBIT_INSTALL_DIR` to change that, and `ORBIT_DOWNLOAD_URL` to use a mirror. To install by hand:

```bash
VERSION=v0.1.0
TARGET=x86_64-unknown-linux-gnu
curl -fsSLO https://github.com/AjmalX0/Orbit/releases/download/$VERSION/orbit-$TARGET.tar.gz
curl -fsSLO https://github.com/AjmalX0/Orbit/releases/download/$VERSION/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf orbit-$TARGET.tar.gz
sudo install -m 0755 orbit /usr/local/bin/orbit
```

You can also build from source with `cargo install orbit-lang` (Rust 1.89 or later).

## systemd

Create a system user and directories:

```bash
sudo useradd --system --home /var/lib/orbit --shell /usr/sbin/nologin orbit
sudo install -d -o orbit -g orbit -m 0750 /var/lib/orbit
sudo install -d -m 0750 /etc/orbit
```

Put the secrets in an environment file readable only by root:

```bash
# /etc/orbit/orbit.env  (mode 0600)
ORBIT_TOKEN=...
ORBIT_WORKER_TOKEN=...
ORBIT_DB=/var/lib/orbit/orbit.db
ORBIT_ADDR=0.0.0.0:7233
```

```ini
# /etc/systemd/system/orbit.service
[Unit]
Description=ORBIT workflow runtime
After=network-online.target
Wants=network-online.target

[Service]
User=orbit
Group=orbit
EnvironmentFile=/etc/orbit/orbit.env
ExecStart=/usr/local/bin/orbit server --log-format json
KillSignal=SIGINT
TimeoutStopSec=75
Restart=always
RestartSec=2
WorkingDirectory=/var/lib/orbit
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/var/lib/orbit
LimitNOFILE=65536

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now orbit
journalctl -u orbit -f
```

`KillSignal=SIGINT` gives a graceful shutdown. `TimeoutStopSec=75` leaves time for open worker long polls, which can last up to 60 seconds. With PostgreSQL, add `After=postgresql.service` when the database runs on the same host, and remove `ReadWritePaths`.

## Sizing

- **CPU.** The engine processes state changes one at a time, so it uses about one core under load. Two cores leave room for TLS, HTTP and the background tick.
- **Memory.** Every active (non-terminal) execution is held in memory with its full state; finished executions are not. Memory grows with the number of active executions and the size of their inputs and outputs. Start with 512 MiB and watch `orbit_active_executions`.
- **Storage.** History is never deleted. Each step attempt writes about three events, each the size of its JSON payload; inputs and outputs dominate. Keep large payloads (files, documents) out of workflows and pass references instead.
- **Disk latency.** SQLite commits with `fsync` for every batch of events. Use local SSD storage. For PostgreSQL, latency to the database bounds throughput, so keep it close.
- **Startup time.** Recovery replays the history of every active execution before the server listens. Very long-running executions with very long histories make startup slower.
- **Workers.** Scale workers horizontally; the runtime does not need more instances for more workers. Each worker holds one long-poll request open most of the time.
- **Session timeout.** The default 30 seconds means a crashed worker's tasks are redelivered after about 30 seconds. Lower it for faster failover, at the cost of more heartbeat traffic (heartbeats are sent every sixth of the timeout).

## Upgrades

1. Read the [changelog](../changelog.md) for the new version.
2. Back up the database. See [Storage](storage.md#backups).
3. Stop the server gracefully (`SIGINT`), replace the binary or image tag, and start it. Workers lose their sessions, register again and retry result delivery; executions continue from their history.

Things that stay stable within a major version:

- The HTTP API is versioned by path (`/v1`) and reports `api_version` `orbit-api/1`; new fields may appear in responses.
- The worker protocol is `orbit.worker.v1`. SDKs that speak the same protocol version can connect to any runtime that serves it.
- Compiled definitions are `orbit-ir/1`, or `orbit-ir/2` for workflows that use signals, queries, `await` or `compute` steps, or lambdas. `/v1/health` lists the IR versions a runtime accepts in `ir_versions`; upgrade the runtime before you deploy `orbit-ir/2` definitions to it. See [Versioning](../language/versioning.md#ir-version).
- The database schema is versioned in the `schema_version` table (version `1` in this release) and created on first start.

Downgrading to an older runtime against a database written by a newer one is not supported. Workflow definitions are immutable, so upgrading the runtime never changes the behavior of a running execution's definition; deploy new workflow versions separately from runtime upgrades.

## See also

- [Running the server](server.md)
- [Security](security.md)
- [Storage](storage.md)
- [Observability](observability.md)
- [Installation](../getting-started/installation.md)
