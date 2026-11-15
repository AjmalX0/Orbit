# Formatter

`orbit fmt` formats `.orbit` files in a single, consistent style. It is conservative: it normalizes indentation, spacing and blank lines, but keeps your line breaks and every comment, so it never reflows code or moves a comment. You can run it on save in your editor, and check it in CI with `--check`.

## Usage

```bash
orbit fmt workflows/*.orbit            # rewrite files in place
orbit fmt --check workflows/*.orbit    # report files that would change; write nothing
```

| Option | Meaning |
|---|---|
| `[FILES]...` | Files to format. At least one is required; directories are not expanded, so use your shell's globbing. |
| `--check` | Do not write. Print `would reformat <file>` for each file that is not formatted, and exit with status `1` if there is one. |

Files that are already formatted are not touched and produce no output. Changed files are reported on stderr:

```console
$ orbit fmt --check messy.orbit
would reformat messy.orbit
$ orbit fmt messy.orbit
formatted messy.orbit
$ orbit fmt --check messy.orbit
$ echo $?
0
```

| Exit code | Meaning |
|---|---|
| `0` | Every file is formatted, or was rewritten. |
| `1` | `--check` found an unformatted file; a file could not be read, written or formatted; or no files were given. |

## What it changes

Before:

```text
// Greeting workflow
workflow Hello{
version   "1.0.0"
  input{ name:String , times:Int=-1 }



step greet{ worker "greeting.compose"   // the task
 with {name:input.name,n:input.times*2}
}
}
```

After `orbit fmt`:

```orbit
// Greeting workflow
workflow Hello {
    version "1.0.0"
    input { name: String, times: Int = -1 }

    step greet { worker "greeting.compose" // the task
        with { name: input.name, n: input.times * 2 }
    }
}
```

The rules:

- **Indentation**: four spaces per open `{`, `(` or `[`. A line that continues an expression (it starts with, or follows, an infix operator, or starts with `then` or `else`) gets one extra level.
- **Spacing**: one space around infix operators and `=`, after `:` and `,`, and between a name and `{`; none inside `()` and `[]`, before `:`, `,`, `.` or `?`, after `!` or a unary minus, between a function name and `(`, and inside generic types (`List<Map<String, Int>>`). `{}` stays empty.
- **Blank lines**: at most one in a row, none right after an opening brace or right before a closing one.
- **Trailing whitespace** is removed, and the file ends with exactly one newline.
- **Line breaks** are kept where you wrote them. The formatter never joins or splits lines.
- **Comments** are kept, with their text unchanged. A comment at the end of a line is separated by one space; a comment on its own line is indented like the code. Multi-line block comments are kept verbatim.
- **Line endings**: a file with Windows line endings (`\r\n`) keeps them.

## Safety

Before returning, the formatter re-lexes its own output and checks that the token sequence, including the line-break information the grammar depends on, and the comments are unchanged. If the check fails, the file is left unchanged and `fmt` reports:

```text
error: <file>: formatter safety check failed (<detail>); the file was left unchanged
```

The formatter needs a file that lexes. A file with lexical errors (an unterminated string, an unknown character) is refused:

```console
$ orbit fmt lexbad.orbit
error: lexbad.orbit: cannot format a file with lexical errors: unterminated string literal
```

Syntax and type errors do not prevent formatting, because the formatter works on tokens, not on the syntax tree. Formatting never changes a workflow's `definition_hash`: the hash excludes source locations.

## Editors

The language server (`orbit lsp`) implements document formatting with the same formatter. The VS Code extension makes ORBIT the default formatter for `.orbit` files and turns on format-on-save. See [Editor support](editors.md).

## CI

Fail a build when files are not formatted:

```yaml
# .github/workflows/ci.yml
- name: Check formatting
  run: orbit fmt --check workflows/*.orbit
```

Combine it with `orbit check --format json` to report diagnostics as structured data.

## See also

- [CLI: orbit fmt](../reference/cli.md#orbit-fmt)
- [Editor support](editors.md)
- [Grammar](../language/grammar.md)
- [Project layout](../getting-started/project.md)
