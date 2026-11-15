// ORBIT for VS Code: starts `orbit lsp` (diagnostics, hover, go to definition,
// completion, outline, formatting) and adds Run / Deploy / Check / Graph commands.
import * as vscode from "vscode";
import { LanguageClient, LanguageClientOptions, ServerOptions } from "vscode-languageclient/node";

let client: LanguageClient | undefined;
let restarting: Promise<void> = Promise.resolve();

/** Restart the language server; restarts run one at a time so only one client is ever live. */
function restart(context: vscode.ExtensionContext): Promise<void> {
  restarting = restarting.then(async () => {
    await stopClient();
    await startClient(context);
  });
  return restarting;
}

function config() {
  const c = vscode.workspace.getConfiguration("orbit");
  return { server: c.get<string>("serverPath", "orbit"), runtime: c.get<string>("runtimeUrl", "http://127.0.0.1:7233") };
}

async function startClient(context: vscode.ExtensionContext): Promise<void> {
  const serverOptions: ServerOptions = { command: config().server, args: ["lsp"] };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [{ scheme: "file", language: "orbit" }],
    synchronize: { fileEvents: vscode.workspace.createFileSystemWatcher("**/*.orbit") },
  };
  client = new LanguageClient("orbit", "ORBIT Language Server", serverOptions, clientOptions);
  try {
    await client.start();
  } catch (e) {
    vscode.window.showErrorMessage(
      `ORBIT: could not start \`${config().server} lsp\` (${(e as Error).message}). Install orbit or set "orbit.serverPath".`,
    );
  }
  context.subscriptions.push({ dispose: () => void stopClient() });
}

/** Stop the language server if it is running. A client that failed to start cannot be stopped. */
async function stopClient(): Promise<void> {
  const c = client;
  client = undefined;
  if (!c) return;
  try {
    if (c.isRunning()) await c.stop();
    else await c.dispose();
  } catch {
    // already stopped or never started
  }
}

/** Run an orbit subcommand for the active file in an integrated terminal. */
function inTerminal(args: (file: string) => string[]): void {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.languageId !== "orbit") {
    vscode.window.showWarningMessage("Open an .orbit file first.");
    return;
  }
  void editor.document.save();
  const quote = (s: string) => (/^[\w./:@=-]+$/.test(s) ? s : `'${s.replace(/'/g, "'\\''")}'`);
  const terminal =
    vscode.window.terminals.find((t) => t.name === "ORBIT") ??
    vscode.window.createTerminal({ name: "ORBIT", env: { ORBIT_SERVER: config().runtime } });
  terminal.show(true);
  terminal.sendText([config().server, ...args(editor.document.uri.fsPath)].map(quote).join(" "));
}

export async function activate(context: vscode.ExtensionContext): Promise<void> {
  restarting = startClient(context);
  await restarting;
  context.subscriptions.push(
    vscode.commands.registerCommand("orbit.run", async () => {
      const input = await vscode.window.showInputBox({
        prompt: "Workflow input (JSON)",
        value: "{}",
        validateInput: (v) => {
          try {
            JSON.parse(v);
            return undefined;
          } catch {
            return "Not valid JSON";
          }
        },
      });
      if (input !== undefined) inTerminal((f) => ["run", f, "--input", input]);
    }),
    vscode.commands.registerCommand("orbit.deploy", () => inTerminal((f) => ["deploy", f])),
    vscode.commands.registerCommand("orbit.check", () => inTerminal((f) => ["check", f])),
    vscode.commands.registerCommand("orbit.graph", async () => {
      const editor = vscode.window.activeTextEditor;
      if (!editor) return;
      const { execFile } = await import("node:child_process");
      execFile(config().server, ["graph", editor.document.uri.fsPath], async (err, stdout, stderr) => {
        if (err) {
          vscode.window.showErrorMessage(`orbit graph failed: ${stderr || err.message}`);
          return;
        }
        const doc = await vscode.workspace.openTextDocument({ language: "mermaid", content: stdout });
        await vscode.window.showTextDocument(doc, vscode.ViewColumn.Beside);
      });
    }),
    vscode.commands.registerCommand("orbit.restartServer", () => restart(context)),
    vscode.workspace.onDidChangeConfiguration((e) => {
      if (e.affectsConfiguration("orbit.serverPath")) void restart(context);
    }),
  );
}

export async function deactivate(): Promise<void> {
  await stopClient();
}
