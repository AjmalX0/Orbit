# Editor support

ORBIT ships a language server, `orbit lsp`, built into the `orbit` binary. It reports diagnostics from the real compiler as you type, and provides hover, go to definition, completion, an outline and formatting. The VS Code extension packages it with syntax highlighting and commands to run, deploy and graph workflows. Any other editor with an LSP client can use `orbit lsp` over stdio.

## Language server features

| Feature | LSP method | What it does |
|---|---|---|
| Diagnostics | `textDocument/publishDiagnostics` | Runs the compiler on every open, change and save, with unsaved buffers taking precedence over files on disk, imports included. Diagnostics carry the stable code (for example `E0300`) and `source: "orbit"`. See [Compiler diagnostics](../reference/diagnostics.md). |
| Hover | `textDocument/hover` | Task contracts, step output types, field types along `step.field.sub` paths, schemas, child workflows, built-in functions and keywords. |
| Go to definition | `textDocument/definition` | Steps, schemas, tasks and child workflows, across imports. |
| Completion | `textDocument/completion` | Fields after `input.` and `<step>.`, task names inside `worker "…"`, keywords, types and built-in functions. Triggered by `.` and `"`. |
| Outline | `textDocument/documentSymbol` | Workflows with their steps, schemas and tasks. |
| Formatting | `textDocument/formatting` | The same formatter as `orbit fmt`. See [Formatter](fmt.md). |

The server uses full document synchronization, identifies itself as `orbit-lsp`, and reads and writes JSON-RPC messages with `Content-Length` headers on stdin and stdout. It takes no command-line options and no initialization options.

## Visual Studio Code

### Install

The extension is attached to every GitHub release as `orbit-lang.vsix`. Download it from the [releases page](https://github.com/AjmalX0/Orbit/releases) and install it:

```bash
curl -fsSLO https://github.com/AjmalX0/Orbit/releases/latest/download/orbit-lang.vsix
code --install-extension orbit-lang.vsix
```

You can also use **Extensions: Install from VSIX...** in the Command Palette.

The extension needs the `orbit` binary, version 0.1 or later (see [Installation](../getting-started/installation.md)). If `orbit` is not on your `PATH`, set `orbit.serverPath`. It works with VS Code 1.82 or later, and activates when you open an `.orbit` file or a folder that contains one.

### Features

- Syntax highlighting for `.orbit` files, with bracket matching, auto-closing pairs and comment toggling.
- Everything the language server provides, listed above.
- Format on save, on by default for ORBIT files.
- Commands and a run button in the editor title bar.

The extension sets these defaults for the `[orbit]` language, which you can override in your settings:

```json
"[orbit]": {
  "editor.defaultFormatter": "orbit-lang.orbit-lang",
  "editor.formatOnSave": true,
  "editor.tabSize": 4
}
```

### Commands

| Command | ID | What it does |
|---|---|---|
| ORBIT: Run Workflow | `orbit.run` | Saves the file, asks for the workflow input as JSON (default `{}`), and runs `orbit run <file> --input <json>` in an integrated terminal named `ORBIT`. Also available as the run button in the editor title bar. |
| ORBIT: Deploy Workflows in This File | `orbit.deploy` | Runs `orbit deploy <file>` in the terminal. |
| ORBIT: Check File | `orbit.check` | Runs `orbit check <file>` in the terminal. |
| ORBIT: Show Step Graph (Mermaid) | `orbit.graph` | Runs `orbit graph <file>` and opens the Mermaid source beside the editor. Use a Mermaid preview extension to render it. |
| ORBIT: Restart Language Server | `orbit.restartServer` | Stops and starts `orbit lsp`. |

The terminal is created with `ORBIT_SERVER` set to `orbit.runtimeUrl`. To use a runtime with authentication, export `ORBIT_TOKEN` (and `ORBIT_CA_CERT` for a private CA) in the environment VS Code starts from, or type the command in the terminal yourself with `--token`.

### Settings

| Setting | Type | Default | Meaning |
|---|---|---|---|
| `orbit.serverPath` | string | `orbit` | Path to the `orbit` binary, used for `orbit lsp` and the commands. Changing it restarts the language server. |
| `orbit.runtimeUrl` | string | `http://127.0.0.1:7233` | Runtime used by the Run and Deploy commands, passed as `ORBIT_SERVER`. |
| `orbit.trace.server` | `off`, `messages` or `verbose` | `off` | Log the communication with the language server in the **ORBIT Language Server** output channel. |

### Troubleshooting

If the extension cannot start the server, it shows ``ORBIT: could not start `orbit lsp` (...). Install orbit or set "orbit.serverPath".`` Check that `orbit --version` works in a terminal, set `orbit.serverPath` to an absolute path if needed, and run **ORBIT: Restart Language Server**. Set `orbit.trace.server` to `verbose` to see every message.

## Other editors

Configure your editor's LSP client to start `orbit lsp` for files with the `.orbit` extension, communicating over stdio. No arguments, initialization options or workspace settings are needed. These examples assume `orbit` is on your `PATH`.

Syntax highlighting is not part of the language server. The VS Code extension contains a TextMate grammar (`editors/vscode/syntaxes/orbit.tmLanguage.json`) that editors with TextMate support can reuse. Elsewhere, highlighting as a C-like language gives a reasonable approximation: ORBIT uses `//` and `/* */` comments, double-quoted strings and braces.

### Neovim

With Neovim 0.11 or later and the built-in client:

```lua
-- init.lua
vim.filetype.add({ extension = { orbit = "orbit" } })

vim.lsp.config("orbit", {
  cmd = { "orbit", "lsp" },
  filetypes = { "orbit" },
  root_markers = { ".git" },
})
vim.lsp.enable("orbit")

-- Format on save.
vim.api.nvim_create_autocmd("BufWritePre", {
  pattern = "*.orbit",
  callback = function() vim.lsp.buf.format({ async = false }) end,
})
```

With Neovim 0.10, start the client from an autocommand instead:

```lua
vim.filetype.add({ extension = { orbit = "orbit" } })

vim.api.nvim_create_autocmd("FileType", {
  pattern = "orbit",
  callback = function(args)
    vim.lsp.start({
      name = "orbit",
      cmd = { "orbit", "lsp" },
      root_dir = vim.fs.root(args.buf, { ".git" }) or vim.fn.getcwd(),
    })
  end,
})
```

### Helix

```toml
# ~/.config/helix/languages.toml
[language-server.orbit]
command = "orbit"
args = ["lsp"]

[[language]]
name = "orbit"
scope = "source.orbit"
file-types = ["orbit"]
roots = [".git"]
comment-token = "//"
block-comment-tokens = { start = "/*", end = "*/" }
indent = { tab-width = 4, unit = "    " }
language-servers = ["orbit"]
auto-format = true
```

Run `hx --health orbit` to check that Helix finds the server.

### Other clients

For Emacs (`eglot`), Sublime Text (LSP package), Zed, Kate and other clients, register a server with:

| Setting | Value |
|---|---|
| Command | `orbit lsp` |
| Transport | stdio |
| File pattern | `*.orbit` |
| Language id | `orbit` |
| Root marker | your repository root, for example `.git` |

For example, with `eglot`:

```elisp
(define-derived-mode orbit-mode prog-mode "ORBIT"
  (setq-local comment-start "// "))
(add-to-list 'auto-mode-alist '("\\.orbit\\'" . orbit-mode))
(with-eval-after-load 'eglot
  (add-to-list 'eglot-server-programs '(orbit-mode "orbit" "lsp")))
```

## See also

- [Formatter](fmt.md)
- [Workflow graphs](graph.md)
- [Compiler diagnostics](../reference/diagnostics.md)
- [CLI: orbit lsp](../reference/cli.md#orbit-lsp)
- [Installation](../getting-started/installation.md)
