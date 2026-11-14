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

Run:  PYTHONPATH=<orbit>/sdks/python python workers/python/worker.py
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
// Run:  node workers/typescript/worker.ts   (Node.js 22.18+ runs TypeScript directly)
import { Worker } from "../../../sdks/typescript/src/index.ts";

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
        "# {name}\n\nAn ORBIT project.\n\n```bash\norbit check workflows/hello.orbit\norbit server &\nPYTHONPATH=$ORBIT_HOME/sdks/python python workers/python/worker.py &\nnode workers/typescript/worker.ts &\norbit run workflows/hello.orbit --input '{{\"name\": \"Ada\"}}'\n```\n\nThe TypeScript worker imports the SDK by relative path; adjust it if the project lives\noutside the ORBIT repository (or install `@orbit/sdk`).\n"
    );
    let files: [(&str, String); 5] = [
        (
            "workers/typescript/package.json",
            "{ \"private\": true, \"type\": \"module\" }\n".to_string(),
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
