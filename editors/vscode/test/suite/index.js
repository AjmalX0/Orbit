// Runs inside VS Code's extension host.
const assert = require("node:assert/strict");
const path = require("node:path");
const vscode = require("vscode");

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function eventually(what, fn, timeoutMs = 30000) {
  const deadline = Date.now() + timeoutMs;
  let last;
  while (Date.now() < deadline) {
    try {
      const v = await fn();
      if (v) return v;
    } catch (e) {
      last = e;
    }
    await sleep(250);
  }
  throw new Error(`timed out waiting for ${what}${last ? `: ${last.message}` : ""}`);
}

exports.run = async function run() {
  const ext = vscode.extensions.getExtension("orbit-lang.orbit-lang");
  assert.ok(ext, "extension is installed");
  await vscode.workspace.getConfiguration("orbit").update("serverPath", process.env.ORBIT_BIN, vscode.ConfigurationTarget.Global);
  await ext.activate();
  await vscode.commands.executeCommand("orbit.restartServer");

  const uri = vscode.Uri.file(path.resolve(__dirname, "..", "fixture", "demo.orbit"));
  const doc = await vscode.workspace.openTextDocument(uri);
  assert.equal(doc.languageId, "orbit", "the .orbit extension maps to the orbit language");
  await vscode.window.showTextDocument(doc);

  // Live diagnostics from `orbit lsp`.
  const diags = await eventually("diagnostics", () => {
    const d = vscode.languages.getDiagnostics(uri);
    return d.length > 0 ? d : undefined;
  });
  assert.equal(diags.length, 1);
  assert.equal(String(diags[0].code), "E0402");
  assert.match(diags[0].message, /did you mean `score`\?/);
  assert.equal(diags[0].range.start.line, 6);

  // Hover over the task name shows its contract.
  const text = doc.getText();
  const at = doc.positionAt(text.indexOf('"risk.analyze"', text.indexOf("step risk")) + 3);
  const hovers = await eventually("hover", async () => {
    const h = await vscode.commands.executeCommand("vscode.executeHoverProvider", uri, at);
    return h && h.length ? h : undefined;
  });
  const hoverText = hovers.flatMap((h) => h.contents.map((c) => (typeof c === "string" ? c : c.value))).join("\n");
  assert.match(hoverText, /risk\.analyze@1\.0\.0/);

  // Go to definition of the `risk` reference.
  const ref = doc.positionAt(text.indexOf("risk.scor") + 1);
  const defs = await vscode.commands.executeCommand("vscode.executeDefinitionProvider", uri, ref);
  assert.equal(defs[0].range.start.line, 5);

  // Formatting through the language server.
  const edits = await vscode.commands.executeCommand("vscode.executeFormatDocumentProvider", uri, { tabSize: 4, insertSpaces: true });
  assert.ok(Array.isArray(edits), "formatting provider answered");

  // Commands are registered.
  const commands = await vscode.commands.getCommands(true);
  for (const c of ["orbit.run", "orbit.deploy", "orbit.check", "orbit.graph", "orbit.restartServer"]) {
    assert.ok(commands.includes(c), `${c} is registered`);
  }
  console.log("ORBIT extension integration test passed");
};
