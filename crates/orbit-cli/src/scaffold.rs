//! `orbit init`: project scaffolding.

use std::path::Path;

const HELLO: &str = r#"// A two-language hello world: Python composes, TypeScript decorates.

task "greeting.compose" {
    version "1.0.0"
    input { name: String }
    output { message: String }
}

task "greeting.decorate" {
    version "1.0.0"
    input { message: String, excitement: Int }
    output { message: String }
}

workflow Hello {
    version "1.0.0"

    input {
        name: String
        excitement: Int = 3
    }

    output { greeting: String }

    step compose {
        worker "greeting.compose"
        using python
        with { name: input.name }
        retry { attempts: 3, backoff: exponential, initial_delay: 500ms }
    }

    step decorate {
        worker "greeting.decorate"
        using typescript
        with { message: compose.message, excitement: input.excitement }
        timeout 10s
    }

    return { greeting: decorate.message }
}
"#;

const PY_WORKER: &str = r#""""Python worker for the Hello workflow.

Run:  pip install -r workers/python/requirements.txt
      python workers/python/worker.py
"""
from orbit_sdk import Worker

worker = Worker("hello-python")


@worker.task("greeting.compose", version="1.0.0")
def compose(input, ctx):
    ctx.log(f"composing a greeting for {input['name']}")
    return {"message": f"Hello, {input['name']}"}


if __name__ == "__main__":
    worker.run()
"#;

const TS_WORKER: &str = r#"// TypeScript worker for the Hello workflow.
//
// Run:  (cd workers/typescript && npm install) && node workers/typescript/worker.ts
//       Node.js 22.18+ runs TypeScript directly.
import { Worker } from "orbit-lang";

const worker = new Worker({ workerId: "hello-typescript" });

worker.task<{ message: string; excitement: number }, { message: string }>(
  "greeting.decorate",
  { version: "1.0.0" },
  async (input) => ({ message: input.message + "!".repeat(input.excitement) }),
);

await worker.run();
"#;

pub fn init(dir: &Path) -> Result<(), String> {
    if dir.exists()
        && std::fs::read_dir(dir)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false)
    {
        return Err(format!("{} already exists and is not empty", dir.display()));
    }
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "orbit-project".into());
    let readme = format!(
        r#"# {name}

An [ORBIT](https://github.com/AjmalX0/Orbit) project: one workflow, two languages.

```bash
pip install -r workers/python/requirements.txt
(cd workers/typescript && npm install)
orbit check workflows/hello.orbit
orbit server &
python workers/python/worker.py &
node workers/typescript/worker.ts &
orbit run workflows/hello.orbit --input '{{"name": "Ada"}}'
```

Format with `orbit fmt workflows/*.orbit`. The ORBIT VS Code extension gives
diagnostics, hover, go to definition and completion as you type.
"#
    );
    let package_json = r#"{
  "private": true,
  "type": "module",
  "dependencies": {
    "orbit-lang": "^0.5.0"
  }
}
"#;
    let files: [(&str, String); 7] = [
        ("workers/typescript/package.json", package_json.to_string()),
        (
            "workers/python/requirements.txt",
            "orbit-lang-sdk>=0.1,<0.2\n".to_string(),
        ),
        (
            ".gitignore",
            "node_modules/\n__pycache__/\n*.db\n*.db-wal\n*.db-shm\nbuild/\n".to_string(),
        ),
        ("workflows/hello.orbit", HELLO.to_string()),
        ("workers/python/worker.py", PY_WORKER.to_string()),
        ("workers/typescript/worker.ts", TS_WORKER.to_string()),
        ("README.md", readme),
    ];
    for (rel, content) in files {
        let path = dir.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
        std::fs::write(&path, content)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        eprintln!("  created {}", path.display());
    }
    eprintln!(
        "\nNext: cd {} && orbit check workflows/hello.orbit",
        dir.display()
    );
    Ok(())
}
