//! Built-in declarations available to every ORBIT program.

use orbit_syntax::{ast::Program, parse, FileId};

pub const SOURCE: &str = r#"
// HTTP integration adapter, executed by the runtime itself (RFC-0001 §3.1).
// Enabled with `orbit server --enable-http-adapter`.
task "orbit.http.request" {
    version "1.0.0"
    input {
        method: String = "GET"
        url: String
        headers: Map<String, String> = {}
        body: JSON = null
    }
    output {
        status: Int
        headers: Map<String, String>
        body: JSON
    }
}
"#;

pub fn program(file: FileId) -> Program {
    let r = parse(file, SOURCE);
    debug_assert!(
        r.diagnostics.is_empty(),
        "prelude must parse: {:?}",
        r.diagnostics
    );
    r.program
}
