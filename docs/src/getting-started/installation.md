# Installation

ORBIT ships as one binary, `orbit`, that contains the compiler, the runtime
server, the formatter, the language server and the client commands. You also
install a worker SDK for each language your workers use. This page covers
every installation method for the CLI and runtime, the six SDKs, the VS Code
extension, upgrading and uninstalling.

The current release is **v0.1.0**.

## Choose an installation method

| Platform | Recommended method | Alternatives |
|---|---|---|
| Linux x86_64 (glibc) | [Install script](#install-script-linux) | Manual download, `cargo install`, Docker, from source |
| Linux arm64 (glibc) | [Install script](#install-script-linux) | Manual download, `cargo install`, Docker, from source |
| macOS (Intel or Apple silicon) | [`cargo install orbit-lang`](#cargo-install-any-platform) | Docker, from source |
| Windows x86_64 | [Manual download](#windows) | `cargo install`, Docker |
| Containers and servers | [Docker image](#docker) | Any of the above |

The prebuilt Linux binaries link against glibc. On musl-based distributions
such as Alpine, use `cargo install` or the Docker image.

## Install script (Linux)

The install script downloads the release archive for your architecture,
verifies its SHA-256 checksum against the release's `SHA256SUMS` file, and
installs the binary:

```bash
curl -fsSL https://github.com/AjmalX0/Orbit/releases/latest/download/install.sh | sh
```

```text
orbit: downloading orbit-x86_64-unknown-linux-gnu.tar.gz (latest)
orbit: installed orbit 0.1.0 to /home/you/.local/bin/orbit
```

If the install directory is not on your `PATH`, the script prints
`orbit: add <dir> to your PATH`. Add it in your shell profile, for example:

```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
```

The script reads these environment variables:

| Variable | Default | Purpose |
|---|---|---|
| `ORBIT_VERSION` | `latest` | Release tag to install, for example `v0.1.0`. |
| `ORBIT_INSTALL_DIR` | `~/.local/bin`, or `/usr/local/bin` when run as root | Directory the binary is installed into. |
| `ORBIT_DOWNLOAD_URL` | GitHub Releases | Base URL of a mirror that serves the release assets and `SHA256SUMS`. Overrides `ORBIT_VERSION`. |

Pass them to `sh`, not to `curl`:

```bash
curl -fsSL https://github.com/AjmalX0/Orbit/releases/latest/download/install.sh \
  | ORBIT_VERSION=v0.1.0 ORBIT_INSTALL_DIR=/opt/orbit/bin sh
```

If the checksum does not match, the script prints
`orbit: checksum mismatch for <asset>`, exits with status 1 and installs
nothing.

### Manual download (Linux)

To install without piping a script into `sh`, download the archive and the
checksum file from [GitHub Releases](https://github.com/AjmalX0/Orbit/releases/latest)
yourself:

```bash
curl -fsSLO https://github.com/AjmalX0/Orbit/releases/latest/download/orbit-x86_64-unknown-linux-gnu.tar.gz
curl -fsSLO https://github.com/AjmalX0/Orbit/releases/latest/download/SHA256SUMS
sha256sum --check --ignore-missing SHA256SUMS
tar -xzf orbit-x86_64-unknown-linux-gnu.tar.gz
install -m 0755 orbit ~/.local/bin/orbit
```

For arm64, use `orbit-aarch64-unknown-linux-gnu.tar.gz`. The archive contains
the single `orbit` binary.

## Windows

Download `orbit-x86_64-pc-windows-msvc.zip` from
[GitHub Releases](https://github.com/AjmalX0/Orbit/releases/latest), verify it,
and put `orbit.exe` on your `PATH`. In PowerShell:

```powershell
$base = "https://github.com/AjmalX0/Orbit/releases/latest/download"
Invoke-WebRequest "$base/orbit-x86_64-pc-windows-msvc.zip" -OutFile orbit.zip
Invoke-WebRequest "$base/SHA256SUMS" -OutFile SHA256SUMS

# Compare this hash with the orbit-x86_64-pc-windows-msvc.zip line in SHA256SUMS.
(Get-FileHash orbit.zip -Algorithm SHA256).Hash.ToLower()
Select-String "orbit-x86_64-pc-windows-msvc.zip" SHA256SUMS

$dir = "$env:LOCALAPPDATA\Programs\orbit"
Expand-Archive orbit.zip -DestinationPath $dir -Force
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
[Environment]::SetEnvironmentVariable("Path", "$userPath;$dir", "User")
```

Open a new terminal so that the updated `PATH` takes effect. The install
script is a POSIX shell script and does not run in PowerShell or `cmd.exe`.

## cargo install (any platform)

`orbit-lang` is the crate that provides the `orbit` binary. It builds on Linux,
macOS and Windows with Rust 1.89 or later:

```bash
cargo install orbit-lang --locked
```

The build compiles the TLS stack from source, so it needs a C compiler and
CMake in addition to Rust (on macOS, the Xcode command line tools and
`brew install cmake`; on Debian and Ubuntu, `apt install build-essential cmake`).
Cargo installs the binary into `~/.cargo/bin`, which `rustup` adds to your
`PATH`.

This is the recommended method on macOS: v0.1.0 has no prebuilt macOS binary.

## Docker

The runtime image is published to the GitHub Container Registry:

| Tag | Contents |
|---|---|
| `ghcr.io/ajmalx0/orbit:v0.1.0` | the v0.1.0 release |
| `ghcr.io/ajmalx0/orbit:latest` | the most recent release |

The image runs `orbit server` as an unprivileged user, listens on
`0.0.0.0:7233`, and stores its SQLite database at `/data/orbit.db` (a volume).
Because it listens on all interfaces, the server refuses to start without a
token. Set `ORBIT_TOKEN`:

```bash
docker run -d --name orbit \
  -p 7233:7233 \
  -e ORBIT_TOKEN=change-me \
  -v orbit-data:/data \
  ghcr.io/ajmalx0/orbit:v0.1.0
```

Without a token, the container exits immediately with:

```text
error: refusing to listen on 0.0.0.0:7233 without authentication; pass --token (or ORBIT_TOKEN), or --insecure for trusted networks
```

Use a long random token in practice; `orbit token` prints one. Clients and
workers on the host then pass the same token:

```bash
export ORBIT_TOKEN=change-me
orbit ps
```

The image's entry point is `orbit`, so arguments after the image name replace
the default `server` command. For example, to print the version, or to start
the server with the gRPC worker protocol (port 7234 is exposed for it) and the
HTTP adapter enabled:

```bash
docker run --rm ghcr.io/ajmalx0/orbit:v0.1.0 --version

docker run -d -p 7233:7233 -p 7234:7234 -e ORBIT_TOKEN=change-me -v orbit-data:/data \
  ghcr.io/ajmalx0/orbit:v0.1.0 server --grpc-addr 0.0.0.0:7234 --enable-http-adapter
```

To build the image yourself from a clone, run `docker build -t orbit .` in the
repository root. [Deployment](../runtime/deployment.md) covers PostgreSQL,
TLS and production settings.

## From source

Build from a clone when you want unreleased changes or plan to contribute.
You need Rust 1.89 or later, a C compiler and CMake:

```bash
git clone https://github.com/AjmalX0/Orbit.git
cd Orbit
cargo install --path crates/orbit-cli --locked
```

Alternatively, `cargo build --release` leaves the binary at
`target/release/orbit` without installing it. [Contributing](../contributing.md)
describes the full development setup.

## Verify the installation

Check the version:

```bash
orbit --version
```

```text
orbit 0.1.0
```

Then start the runtime and query its health endpoint from a second terminal:

```bash
orbit server
```

```bash
curl -s http://127.0.0.1:7233/v1/health
```

```json
{"status":"ok","api_version":"orbit-api/1","runtime_version":"0.2.0","ir_versions":["orbit-ir/1","orbit-ir/2"],"active_executions":0,"worker_sessions":0,"role":"single","node_id":null,"term":null,"leader":null}
```

By default `orbit server` listens on `127.0.0.1:7233` and creates `orbit.db`
(plus `orbit.db-wal` and `orbit.db-shm`) in the current directory. Stop it
with Ctrl-C.

## Install a worker SDK

Workers use an SDK to register with the runtime, receive tasks and report
results. Install the SDK for each language you write workers in:

| Language | Package | Install | Requirements |
|---|---|---|---|
| Python | `orbit-lang-sdk` on PyPI | `pip install orbit-lang-sdk` | Python 3.9+, no dependencies |
| TypeScript and JavaScript | `orbit-lang` on npm | `npm install orbit-lang` | Node.js 18+ (22.18+ to run `.ts` files directly) |
| Go | `github.com/AjmalX0/Orbit/sdks/go` | `go get github.com/AjmalX0/Orbit/sdks/go@v0.1.0` | Go 1.21+, no dependencies |
| Rust | `orbit-worker` on crates.io | `cargo add orbit-worker` | Rust 1.89+ |
| Java | `io.github.ajmalx0:orbit-sdk` on Maven Central | see below | Java 17+ |
| C# and .NET | `OrbitLang.Sdk` on NuGet | `dotnet add package OrbitLang.Sdk` | .NET 8+ |

The Python package is installed as `orbit-lang-sdk` and imported as
`orbit_sdk`:

```python
from orbit_sdk import Worker
```

For Java with Maven:

```xml
<dependency>
  <groupId>io.github.ajmalx0</groupId>
  <artifactId>orbit-sdk</artifactId>
  <version>0.1.0</version>
</dependency>
```

With Gradle:

```kotlin
implementation("io.github.ajmalx0:orbit-sdk:0.1.0")
```

All SDKs read the same environment variables: `ORBIT_SERVER` (default
`http://127.0.0.1:7233`), `ORBIT_WORKER_TOKEN` or `ORBIT_TOKEN`,
`ORBIT_CA_CERT`, and `ORBIT_CLIENT_CERT` / `ORBIT_CLIENT_KEY`. Each language
has its own page under [Workers and SDKs](../sdks/overview.md).

## Install the VS Code extension

The extension provides syntax highlighting and, through `orbit lsp`,
diagnostics as you type, hover, go to definition, completion, outline and
formatting. Download `orbit-lang.vsix` from
[GitHub Releases](https://github.com/AjmalX0/Orbit/releases/latest) and
install it:

```bash
curl -fsSLO https://github.com/AjmalX0/Orbit/releases/latest/download/orbit-lang.vsix
code --install-extension orbit-lang.vsix
```

The extension runs the `orbit` binary from your `PATH`. If it is installed
elsewhere, set `orbit.serverPath` in your VS Code settings. The
`orbit.runtimeUrl` setting (default `http://127.0.0.1:7233`) selects the
runtime used by the extension's Run and Deploy commands. See
[Editor support](../tooling/editors.md).

## Upgrade

| Method | Upgrade command |
|---|---|
| Install script | Run the script again. It replaces the binary in `ORBIT_INSTALL_DIR`. |
| Manual download | Download the new archive and replace the binary. |
| `cargo install` | `cargo install orbit-lang --locked` (Cargo replaces an older version). |
| Docker | `docker pull ghcr.io/ajmalx0/orbit:<new tag>`, then recreate the container with the same volume. |
| From source | `git pull`, then `cargo install --path crates/orbit-cli --locked`. |
| SDKs | `pip install -U orbit-lang-sdk`, `npm install orbit-lang@latest`, `go get github.com/AjmalX0/Orbit/sdks/go@latest`, `cargo update -p orbit-worker`, or bump the version in `pom.xml` / the `.csproj`. |
| VS Code extension | Install the new `orbit-lang.vsix` with `code --install-extension`. |

Stop the server before replacing its binary. On restart, the runtime reloads
every definition and resumes every execution that was still running.
Registered workflow definitions and execution history live in the database,
not in the binary, so upgrading does not change them. The runtime reports the
IR versions it accepts in `/v1/health` (`ir_versions`).

## Uninstall

Remove the binary that matches how you installed it:

```bash
rm ~/.local/bin/orbit          # install script as a regular user
sudo rm /usr/local/bin/orbit   # install script as root
cargo uninstall orbit-lang     # cargo install
```

On Windows, delete `%LOCALAPPDATA%\Programs\orbit` and remove it from your
user `PATH`.

For Docker, remove the container and, if you no longer need the data, the
volume:

```bash
docker rm -f orbit
docker volume rm orbit-data
docker image rm ghcr.io/ajmalx0/orbit:v0.1.0
```

`orbit server` keeps its state in the database you pointed it at, by default
`orbit.db`, `orbit.db-wal` and `orbit.db-shm` in the directory where you
started it. Delete those files to remove all definitions and history.

Remove the VS Code extension with:

```bash
code --uninstall-extension orbit-lang.orbit-lang
```

## Next steps

- [Quick start](quickstart.md): create a project and run your first workflow.
- [Running the server](../runtime/server.md): server options and storage.
- [How workers work](../sdks/overview.md): the worker model and the SDKs.
- [CLI reference](../reference/cli.md): every `orbit` command.
