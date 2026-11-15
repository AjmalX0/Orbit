# ORBIT for Visual Studio Code

Language support for [ORBIT](https://github.com/AjmalX0/Orbit), the polyglot
orchestration language.

- **Diagnostics as you type** from the real ORBIT compiler, with stable error
  codes and "did you mean" suggestions. Unsaved changes and imported files are
  taken into account.
- **Hover:** task contracts, step output types, field types along
  `step.field.sub` paths, schemas, child workflows, built-in functions and keywords.
- **Go to definition** for steps, schemas, tasks and child workflows, across
  imports.
- **Completion** for `input.` and `<step>.` fields, task names inside
  `worker "…"`, keywords, types and built-in functions.
- **Outline** of workflows, steps, schemas and tasks.
- **Formatting** with `orbit fmt`. It preserves comments and line breaks, and is
  on by default when saving.
- **Commands:** *ORBIT: Run Workflow* (also the ▶ button in the editor title),
  *Deploy*, *Check*, *Show Step Graph* (Mermaid), *Restart Language Server*.

## Requirements

The `orbit` binary, version 0.1 or later:

```bash
curl -fsSL https://raw.githubusercontent.com/AjmalX0/Orbit/main/install.sh | sh
```

or `cargo install orbit-lang`. If `orbit` is not on your `PATH`, set
`orbit.serverPath`.

## Settings

| Setting | Default | |
|---|---|---|
| `orbit.serverPath` | `orbit` | path to the `orbit` binary |
| `orbit.runtimeUrl` | `http://127.0.0.1:7233` | runtime used by Run and Deploy |
| `orbit.trace.server` | `off` | log language-server traffic |
