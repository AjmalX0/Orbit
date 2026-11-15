# Files and imports

This page describes how ORBIT source is organized on disk: what a source file
contains, how `import` combines files into one compilation, how names are
shared between files, and how comments and whitespace work.

## Source files

ORBIT source files are UTF-8 text files with the `.orbit` extension. A file
contains any number of top-level declarations, in any order:

| Declaration | Purpose | Page |
|-------------|---------|------|
| `schema Name { ... }` | A named record type | [Schemas](schemas.md) |
| `task "name" { ... }` | A worker contract | [Task contracts](tasks.md) |
| `workflow Name { ... }` | A workflow definition | [Workflows](workflows.md) |
| `import "path.orbit"` | Include another file in the compilation | [Imports](#imports) |
| `test "name" { ... }` | A test of a workflow, run by `orbit test` | [Testing workflows](../tooling/testing.md) |

Declaration order does not matter. A workflow can use a schema or task that is
declared further down the same file, or in a file that is imported later.

Test declarations are type-checked with the rest of the compilation, so
`orbit check` reports their errors, but they never become part of the compiled
output: `orbit compile`, `orbit deploy` and `orbit run` ignore them. A common
convention is to keep a workflow's tests in a separate `<name>.test.orbit` file
that imports the workflow file.

Anything else at the top level is a syntax error, `E0104`:

```orbit
workflow Example {
    version "1.0.0"
}

step orphan { sleep 1s }
```

```text
error[E0104]: expected a declaration, found `step`
 --> example.orbit:5:1
  |
5 | step orphan { sleep 1s }
  | ^^^^
  = note: top-level declarations are `workflow`, `schema`, `task`, `import` and `test`

error: compilation failed: 1 error(s), 0 warning(s)
```

## Imports

`import` takes a string literal with a path. The path is resolved relative to
the directory of the file that contains the `import`, not relative to the
current working directory.

```orbit
// workflows/orders.orbit
import "../contracts/payments.orbit"
import "shared.orbit"            // workflows/shared.orbit
```

The compiler loads files as follows:

- Every file is loaded once, even if several files import it. Files are
  identified by their canonical path, so `a/../b.orbit` and `b.orbit` are the
  same file.
- Import cycles are allowed. If `a.orbit` imports `b.orbit` and `b.orbit`
  imports `a.orbit`, each file is still loaded once.
- An import that cannot be read is reported as `E0206` at the `import`
  declaration:

  ```text
  error[E0206]: cannot import `missing.orbit`: No such file or directory (os error 2)
   --> main.orbit:1:1
    |
  1 | import "missing.orbit"
    | ^^^^^^^^^^^^^^^^^^^^^^
  ```

There is no module system and no selective import: an `import` brings every
declaration of the imported file, and of the files it imports, into the
compilation.

## One namespace

All files in a compilation share a single namespace for each kind of
declaration:

- schema names,
- task names (strings such as `"payments.charge"`),
- workflow names.

Declaring the same schema, task or workflow twice, in the same file or in
different files, is an error (`E0200`), with a label pointing at the first
declaration. The three kinds have separate namespaces, so a schema and a
workflow may share a name.

The built-in prelude is part of every compilation. It declares the task
`orbit.http.request` (see [HTTP adapter](http-adapter.md)). Task names that
start with `orbit.` are reserved for built-in tasks, and declaring one is
`E0200`.

## Compiling several files

`orbit check`, `orbit compile` and `orbit deploy` accept one or more files;
`orbit run` and `orbit graph` take one file. The files you name, plus
everything they import, form one compilation:

```bash
orbit check workflows/orders.orbit
orbit check contracts.orbit workflows/*.orbit
```

Workflows declared in imported files are compiled too. If `main.orbit`
imports `lib.orbit` and both declare workflows, `orbit check main.orbit`
reports both:

```text
ok Lib@1.0.0 (0 steps)
ok Main@1.0.0 (1 steps)
```

A common layout keeps task contracts and shared schemas in one file that every
workflow file imports. See [Project layout](../getting-started/project.md).

## Comments

ORBIT has two kinds of comments:

- Line comments start with `//` and run to the end of the line.
- Block comments start with `/*` and end with `*/`. Block comments nest, so
  you can comment out a region that already contains a block comment.

```orbit
/* Contracts for the payments service.
   /* Nested comments are allowed. */
*/
task "payments.authorize" {
    version "1.0.0"                       // the contract version
    input { order_id: String, amount: Float }
    output { authorization_id: String }
}
```

An unterminated block comment is `E0003`. Comments are kept by
[`orbit fmt`](../tooling/fmt.md).

## Whitespace, newlines and separators

Spaces, tabs and newlines are insignificant, with one exception: an infix
operator must be on the same line as its left operand. This rule lets you
write one field per line without commas. It is described in
[Expressions](expressions.md#line-breaks-and-operators).

Commas and semicolons between members, fields and object entries are
optional. These three declarations are equivalent:

```orbit
schema Point { x: Int, y: Int }
schema Point2 { x: Int; y: Int }
schema Point3 {
    x: Int
    y: Int
}
```

List literals are the exception: items are separated by commas or by line
breaks, and a semicolon is not accepted between list items.

## Keywords are contextual

Apart from `true`, `false` and `null`, ORBIT has no reserved words. A word such
as `step`, `input`, `version` or `timeout` acts as a keyword only where the
grammar expects that keyword. Everywhere else it is an ordinary identifier, so
it can be a field name or even a declaration name. See
[Grammar](grammar.md#keywords) for the full list and the few names that are
reserved in specific positions.

## See also

- [Project layout](../getting-started/project.md)
- [Schemas](schemas.md)
- [Task contracts](tasks.md)
- [Testing workflows](../tooling/testing.md)
- [Grammar](grammar.md)
- [Compiler diagnostics](../reference/diagnostics.md)
