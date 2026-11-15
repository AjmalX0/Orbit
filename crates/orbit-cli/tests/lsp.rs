//! Drives `orbit lsp` over stdio like an editor would.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

struct Lsp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Lsp {
    fn start() -> Lsp {
        let mut child = Command::new(env!("CARGO_BIN_EXE_orbit"))
            .arg("lsp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Lsp {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn send(&mut self, msg: Value) {
        let body = msg.to_string();
        write!(self.stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv(&mut self) -> Value {
        let mut len = 0;
        loop {
            let mut line = String::new();
            self.stdout.read_line(&mut line).unwrap();
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length: ") {
                len = v.parse().unwrap();
            }
        }
        let mut buf = vec![0; len];
        self.stdout.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let m = self.recv();
            if m["id"] == id {
                return m["result"].clone();
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn diagnostics(&mut self) -> Vec<Value> {
        loop {
            let m = self.recv();
            if m["method"] == "textDocument/publishDiagnostics" {
                return m["params"]["diagnostics"].as_array().unwrap().clone();
            }
        }
    }
}

const DOC: &str = r#"task "risk.analyze" { version "1.0.0" input { amount: Float } output { score: Float } }
workflow W {
    version "1.0.0"
    input { amount: Float }
    step risk { worker "risk.analyze" with { amount: input.amount } }
    step again { worker "risk.analyze" with { amount: risk.score } }
}
"#;

/// LSP position of the `nth` occurrence of `needle` (plus `delta` columns).
fn pos(text: &str, needle: &str, nth: usize, delta: usize) -> Value {
    let at = text.match_indices(needle).nth(nth).unwrap().0 + delta;
    let line = text[..at].matches('\n').count();
    let col = at - text[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    json!({ "line": line, "character": col })
}

#[test]
fn language_server_features() {
    let dir = std::env::temp_dir().join(format!("orbit-lsp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("w.orbit");
    std::fs::write(&file, DOC).unwrap();
    let uri = format!("file://{}", file.display());
    let mut lsp = Lsp::start();

    let init = lsp.request("initialize", json!({ "capabilities": {} }));
    assert_eq!(init["serverInfo"]["name"], "orbit-lsp");
    assert_eq!(init["capabilities"]["hoverProvider"], true);
    lsp.notify("initialized", json!({}));

    // A clean document: no diagnostics.
    lsp.notify(
        "textDocument/didOpen",
        json!({ "textDocument": { "uri": uri, "languageId": "orbit", "version": 1, "text": DOC } }),
    );
    assert_eq!(lsp.diagnostics(), Vec::<Value>::new());

    // An unsaved edit with a typo: located diagnostic with code and help.
    let broken = DOC.replace("risk.score", "risk.scor");
    lsp.notify("textDocument/didChange", json!({ "textDocument": { "uri": uri, "version": 2 }, "contentChanges": [{ "text": broken }] }));
    let diags = lsp.diagnostics();
    assert_eq!(diags.len(), 1, "{diags:?}");
    assert_eq!(diags[0]["code"], "E0402");
    assert!(diags[0]["message"]
        .as_str()
        .unwrap()
        .contains("did you mean `score`?"));
    assert_eq!(diags[0]["range"]["start"], pos(&broken, "scor ", 0, 0));
    lsp.notify("textDocument/didChange", json!({ "textDocument": { "uri": uri, "version": 3 }, "contentChanges": [{ "text": DOC }] }));
    assert!(lsp.diagnostics().is_empty());

    // Hover: the task contract, a step, and a field path.
    let h = lsp.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": pos(DOC, "\"risk.analyze\"", 1, 3) }),
    );
    let text = h["contents"]["value"].as_str().unwrap();
    assert!(
        text.contains("risk.analyze@1.0.0") && text.contains("score: Float"),
        "{text}"
    );
    let h = lsp.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": pos(DOC, "risk.score", 0, 6) }),
    );
    assert_eq!(h["contents"]["value"], "`risk.score`: `Float`");
    let h = lsp.request(
        "textDocument/hover",
        json!({ "textDocument": { "uri": uri }, "position": pos(DOC, "step again", 0, 7) }),
    );
    assert!(
        h["contents"]["value"]
            .as_str()
            .unwrap()
            .contains("**step** `again`"),
        "{h}"
    );

    // Go to definition: the `risk` reference jumps to the step declaration.
    let d = lsp.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": pos(DOC, "risk.score", 0, 1) }),
    );
    assert_eq!(d["range"]["start"], pos(DOC, "risk {", 0, 0));
    let d = lsp.request(
        "textDocument/definition",
        json!({ "textDocument": { "uri": uri }, "position": pos(DOC, "\"risk.analyze\"", 2, 2) }),
    );
    assert_eq!(d["range"]["start"], pos(DOC, "\"risk.analyze\"", 0, 0));

    // Completion after `input.` lists the workflow input fields.
    let c = lsp.request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": pos(DOC, "input.amount", 0, 6) }),
    );
    let labels: Vec<&str> = c
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, vec!["amount"]);
    let c = lsp.request(
        "textDocument/completion",
        json!({ "textDocument": { "uri": uri }, "position": pos(DOC, "risk.score", 0, 0) }),
    );
    let labels: Vec<&str> = c
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["label"].as_str().unwrap())
        .collect();
    assert!(
        labels.contains(&"risk") && labels.contains(&"coalesce") && labels.contains(&"retry"),
        "{labels:?}"
    );

    // Outline and formatting.
    let s = lsp.request(
        "textDocument/documentSymbol",
        json!({ "textDocument": { "uri": uri } }),
    );
    let names: Vec<&str> = s
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["risk.analyze", "W"]);
    assert_eq!(s[1]["children"].as_array().unwrap().len(), 2);
    let messy = DOC.replace("    version", "version");
    lsp.notify("textDocument/didChange", json!({ "textDocument": { "uri": uri, "version": 4 }, "contentChanges": [{ "text": messy }] }));
    lsp.diagnostics();
    let edits = lsp.request(
        "textDocument/formatting",
        json!({ "textDocument": { "uri": uri }, "options": {} }),
    );
    assert_eq!(edits[0]["newText"], DOC);

    assert_eq!(lsp.request("shutdown", Value::Null), Value::Null);
    lsp.notify("exit", Value::Null);
    assert!(lsp.child.wait().unwrap().success());
}
