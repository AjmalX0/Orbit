# Project layout

This page describes what `orbit init` generates, how to organize a project as
it grows, how imports work, how to change workflows and contracts safely, and
how to check ORBIT sources in continuous integration.

## What `orbit init` generates

```bash
orbit init my-project
```

`orbit init` creates the directory and seven files. It refuses to write into a
directory that already exists and is not empty.

```text
my-project/
├── .gitignore
├── README.md
├── workflows/
│   └── hello.orbit
└── workers/
    ├── python/
    │   ├── requirements.txt
    │   └── worker.py
    └── typescript/
        ├── package.json
        └── worker.ts
```

| File | Contents |
|---|---|
| `workflows/hello.orbit` | Two task contracts, `greeting.compose` and `greeting.decorate` (both version `1.0.0`), and the workflow `Hello@1.0.0`, which composes a greeting in Python and decorates it in TypeScript. |
| `workers/python/worker.py` | A Python worker with id `hello-python` that implements `greeting.compose`. |
| `workers/python/requirements.txt` | `orbit-lang-sdk>=0.1,<0.2` |
| `workers/typescript/worker.ts` | A TypeScript worker with id `hello-typescript` that implements `greeting.decorate`. Node.js 22.18 or later runs it directly. |
| `workers/typescript/package.json` | A private ES module package that depends on `orbit-lang` `^0.1.0`. |
| `.gitignore` | Ignores `node_modules/`, `__pycache__/`, the SQLite database files (`*.db`, `*.db-wal`, `*.db-shm`) and `build/` (the default output directory of `orbit compile`). |
| `README.md` | The commands to install dependencies, check the workflow, start the server and workers, and run the workflow. |

The [quick start](quickstart.md) walks through running the generated project.

## Recommended layout

As a project grows, keep contracts, workflows and workers apart. Contracts are
shared by workflows and by the teams that write workers, so they deserve their
own files:

```text
my-project/
├── contracts/
│   ├── orders.orbit           schemas and tasks for one domain
│   └── payments.orbit
├── workflows/
│   ├── order_processing.orbit imports the contracts it uses
│   └── fulfilment.orbit
├── workers/
│   ├── python/                one directory per worker program
│   └── typescript/
└── .github/workflows/orbit.yml
```

Guidelines:

- **Group contracts by domain** (`payments.*`, `inventory.*`), and name tasks
  with a dotted prefix. Per-worker credentials grant access by pattern, such as
  `payments.*`, so a consistent prefix keeps those rules short.
- **Keep one workflow per file** when workflows are independent. Put a parent
  and the child workflows it starts in the same compilation, either in one
  file or through imports.
- **Keep workers next to the contracts they implement**, or in the
  repositories of the services that own them. Workers only need the contract
  names and versions; they do not read `.orbit` files.

The `examples/` directory of the ORBIT repository follows this pattern on a
small scale: `examples/contracts.orbit` holds shared contracts, and
`examples/order_processing.orbit` and `examples/fulfilment.orbit` import it.

## Imports

`import` brings another file's declarations into the compilation. The path is
resolved relative to the importing file:

```orbit
// workflows/order_processing.orbit
import "../contracts/orders.orbit"
import "../contracts/payments.orbit"

workflow OrderProcessing {
    version "1.0.0"
    // ...
}
```

The rules:

- Every file is loaded once, even when several files import it, so two
  workflows can import the same contracts file.
- All declarations share one namespace. Declaring the same schema, task or
  workflow twice is an error (`E0200`).
- Declarations can appear in any order within and across files.
- A file that cannot be read is reported as `E0206`.

Pass the workflow files to the CLI; imported files are found automatically:

```bash
orbit check workflows/*.orbit
```

```text
ok Bye@1.0.0 (1 steps)
ok Hello@1.0.0 (1 steps)
```

A file that contains only contracts checks as `ok no workflows (contracts only)`.
See [Files and imports](../language/files.md).

## Versioning workflow changes

Definitions are immutable per `name@version`, and the runtime enforces it.
Deploying changed content under a version that is already registered fails:

```text
error: DEFINITION_CONFLICT (409): Hello@1.0.0 is already registered with a different definition (sha256:505930b4d4668ff2f8cb95e04519d5292ef2fbf0630d575349aaf92c0e2b42be); workflow definitions are immutable, bump the version
```

Reformatting a file or editing comments does not change the definition hash,
so it never causes a conflict.

### Changing a workflow

1. Edit the workflow and bump its `version`, for example from `1.0.0` to
   `1.1.0`.
2. Run `orbit check`.
3. Deploy it with `orbit deploy`, or let `orbit run` deploy it.

Executions that already started keep running on the definition they started
with. New executions use the version you ask for: `orbit run <file>` starts
the version in the file, and `orbit run <Name> --version <v>` starts a
specific registered version. `orbit run <Name>` without `--version` starts
the highest registered version, compared numerically (`1.10.0` is higher than
`1.9.0`).

### Changing a task contract

Contract versions are independent of workflow versions. A worker receives a
task only when it announced the same task name and contract version that the
step uses.

- **Compatible change** (for example, a new optional output field that no
  workflow reads yet): you can keep the contract version.
- **Incompatible change** (renamed or retyped fields, a new required input):
  declare a new contract version, deploy a worker that implements it, then
  deploy the workflow version that uses it. Keep the worker for the old
  contract version running until no execution needs it.

An SDK worker registers each task name once, so serve two versions of the same
contract from two worker processes with different worker ids. `orbit workers
list` shows which `name@version` each worker serves.

## Using ORBIT in CI

Three commands are useful in a pipeline. All of them work offline; none needs
a running server.

| Command | Fails when |
|---|---|
| `orbit check <files>` | a file has a compile error (exit status 1) |
| `orbit fmt --check <files>` | a file is not formatted (exit status 1, and `would reformat <file>` for each) |
| `orbit compile <files> -o build` | a file has a compile error; otherwise writes `orbit-ir` JSON to `build/` |

`orbit check --format json` prints machine-readable diagnostics with codes,
file, line and column, which you can turn into review annotations:

```json
{
  "diagnostics": [
    {
      "severity": "error",
      "code": "E0405",
      "message": "value may be `null` but `String` is required",
      "location": { "file": "null.orbit", "line": 33, "column": 32 },
      "end_line": 33,
      "end_column": 53,
      "labels": [],
      "notes": [],
      "help": "provide a fallback with `coalesce(value, default)`, or declare the target as optional with `?`"
    }
  ]
}
```

A GitHub Actions job that installs the CLI with the install script and checks
every source file:

```yaml
name: orbit
on: [push, pull_request]
jobs:
  check:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install orbit
        run: |
          curl -fsSL https://github.com/AjmalX0/Orbit/releases/latest/download/install.sh \
            | ORBIT_VERSION=v0.1.0 sh
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
      - run: orbit check workflows/*.orbit
      - run: orbit fmt --check workflows/*.orbit contracts/*.orbit
```

Pin `ORBIT_VERSION` so that a new release does not change your pipeline
unexpectedly.

To deploy from CI, compile once and deploy the IR artifacts. `orbit validate`
checks compiled `.json` files, and `orbit deploy` accepts them as well as
`.orbit` sources:

```bash
orbit compile workflows/*.orbit -o build
orbit validate build/*.json
ORBIT_SERVER=https://orbit.internal:7233 ORBIT_TOKEN="$ORBIT_ADMIN_TOKEN" orbit deploy build/*.json
```

```text
compiled Bye@1.0.0 -> build/bye.orbit.json
compiled Hello@1.0.0 -> build/hello.orbit.json
ok build/bye.orbit.json is valid orbit-ir/1 (Bye@1.0.0, sha256:f8818e42acc5fa28a7e881b49abee1adc6febd65df6dd0d6c48b911b7ef97150)
ok build/hello.orbit.json is valid orbit-ir/1 (Hello@1.0.0, sha256:317903c71c7f843f965f75bde4e69cd36df4a05f16ee8a244236b31383902d65)
deployed Bye@1.0.0 (sha256:f8818e42acc5)
deployed Hello@1.0.0 (sha256:317903c71c7f)
```

Running `orbit deploy` again prints `up to date` for each definition that is
already registered with the same content.

`orbit deploy` exits with status 1 on a `DEFINITION_CONFLICT`, which catches a
workflow that changed without a version bump.

## See also

- [Files and imports](../language/files.md)
- [Versioning](../language/versioning.md)
- [Formatter](../tooling/fmt.md)
- [CLI reference](../reference/cli.md)
- [Compiler diagnostics](../reference/diagnostics.md)
