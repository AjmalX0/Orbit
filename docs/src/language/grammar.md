# Grammar

This page is the formal reference for ORBIT syntax. It reproduces the EBNF
grammar accepted by the `orbit-syntax` parser, walks through it section by
section, and describes the lexical rules: tokens, comments, contextual
keywords and the line-break rule. Semantic restrictions that the compiler
enforces after parsing are summarized at the end, with their diagnostic codes.

## EBNF

The grammar uses ISO EBNF: `{ x }` means zero or more, `[ x ]` means optional,
and `,` separates the items of a sequence.

```ebnf
{{#include ../../../specification/grammar.ebnf}}
```

The source of this grammar is `specification/grammar.ebnf` in the repository.

## Walkthrough

### Programs and declarations

A program is a sequence of declarations: `workflow`, `schema`, `task`,
`import` and `test`, in any order. Separators (`,` or `;`) between declarations are
allowed and ignored. Anything else at the top level is `E0104`. See
[Files and imports](files.md).

### Records and types

`record_body` is shared by schemas, inline record types and the workflow
`input` and `output` blocks. Each `field` is `name: type`, optionally followed
by `= expression` for a constant default.

A `type` is a base type followed by any number of `?`. Base types are
`List<T>`, `Map<K, V>` (where `K` must be `String`), an inline record, or an
identifier naming a primitive or a schema. See [Types](types.md).

### Task contracts

A `task` declaration has a string name and up to five members: `version`,
`description`, `input`, `output` and `compensated_by`. See
[Task contracts](tasks.md).

### Workflows and steps

A workflow has an identifier name and members: `version`, `description`,
`timeout`, `input`, `output`, any number of `step`, `signal` and `query`
declarations, `return`, and `on` handlers. A `signal` has a name and a record body, like
`input`. A `query` has a name, `=` and an expression; the parser also accepts
`:` or nothing in place of `=`, but `=` is the documented form. See
[Signals and queries](signals.md). An `on` member is an outcome handler:
`on failure`, `on cancel` or `on timeout`, followed by a block of `step`
declarations. Any other word after `on` is rejected with `E0105`. See
[Outcome handlers](handlers.md).

A step has an identifier name and members. `action` is one of `worker`,
`workflow`, `sleep`, `await` (followed by a signal name) or `compute`
(followed by an expression). The `compensate` block contains step members, with the
restrictions listed under
[Restrictions after parsing](#restrictions-after-parsing). See
[Steps](steps.md).

### Tests

A `test` declaration has a string name and members: `workflow` (an
identifier), `version`, `input`, any number of `mock`, `signal` and `expect`
members. A `mock` chains behaviours with `times N then`. Tests are checked by
the compiler but never compiled into workflow IR. See
[Testing workflows](../tooling/testing.md).

### Retry blocks

`retry { ... }` contains `name: expression` pairs. The grammar accepts any
identifier and expression; the compiler then requires the five known settings
with literal values (`E0410`). See [Retries and timeouts](retries-timeouts.md).

### Expressions

The expression grammar is layered by precedence, from `or_expr` (lowest) to
`postfix` and `primary` (highest). All binary operators are
left-associative. A unary `-` directly before a number literal is folded into
a negative literal. See [Expressions](expressions.md).

`primary` covers literals, function calls (an identifier immediately followed
by `(`), path roots (`input`, `execution`, a step name, a loop variable or a
lambda parameter), object literals, list literals, `if ... then ... else ...`,
and parenthesized expressions.

A function argument can be a lambda, `IDENTIFIER => expression`. The parser
recognizes `name =>` wherever a primary expression can start, so a misplaced
lambda is reported by the compiler (`E0415`) rather than as a syntax error.
The compiler accepts lambdas only as the second argument of `map`, `filter`,
`find`, `any` and `all`. See [Expressions](expressions.md#lambdas).

### Notes on the grammar

The parser differs from the EBNF in a few details:

- List items are separated by commas or line breaks. A semicolon between list
  items is a syntax error (`E0100`), although `sep` in the EBNF includes `;`.
  A trailing comma is allowed.
- Function arguments are separated by commas only, and a trailing comma is
  allowed: `max(1, 2,)`.
- In `after a, b`, the commas are required. `after a b` parses `b` as an
  unknown member (`E0102`).
- `retry`, `compensate` and `for` do not accept the optional `:` that other
  members accept.
- In a lambda, `=>` must be on the same line as the parameter.

## Lexical rules

### Character set

Source files are UTF-8. Identifiers and keywords are ASCII; non-ASCII
characters are allowed only in string literals and comments. Any other
character outside a string or comment is `E0001`.

### Tokens

| Token | Form | Notes |
|-------|------|-------|
| identifier | `[A-Za-z_][A-Za-z0-9_]*` | Case-sensitive. |
| integer | `[0-9][0-9_]*` | Underscores are ignored. Must fit in a signed 64-bit integer (`E0006`). |
| float | `[0-9][0-9_]*.[0-9][0-9_]*` | Digits are required on both sides of `.`. No exponent. |
| duration | an integer or float immediately followed by `ms`, `s`, `m`, `h` or `d` | Normalized to milliseconds, rounded. Any other letters after a number are `E0005`. |
| string | `"..."` on one line | Escapes: `\n`, `\t`, `\r`, `\"`, `\\`, `\u{hex}`. Unknown escapes are `E0004`; an unterminated or multi-line string is `E0002`. |
| punctuation | `{ } ( ) [ ] < > , : ; . ? = ! + - * / %` | |
| operators | `== != <= >= && \|\| =>` | Two-character operators are lexed greedily. `=>` separates a lambda parameter from its body. |
| reserved literals | `true`, `false`, `null` | The only reserved words. |

### Comments

`//` starts a comment that runs to the end of the line. `/*` starts a block
comment that ends at the matching `*/`; block comments nest. An unterminated
block comment is `E0003`. Comments are whitespace to the parser, except that a
block comment spanning lines counts as a line break.

### Whitespace and line breaks

Whitespace and line breaks are insignificant, with one rule: an infix
operator, and a postfix `.`, `[` or `(`, must be on the same line as its left
operand. A line break before such a token ends the expression. That is why

```orbit
workflow Fields {
    version "1.0.0"
    return {
        a: 1
        b: -2
    }
}
```

has two fields, `a: 1` and `b: -2`, rather than one field `a: 1 - 2`. See
[Expressions](expressions.md#line-breaks-and-operators).

### Separators and colons

- `,` and `;` between members, fields and object entries are optional.
- Member keywords that take a value may be followed by an optional `:`.
  `timeout 5s` and `timeout: 5s` are the same, and so are
  `idempotency_key "k"` and `idempotency_key: "k"`.

## Keywords

ORBIT keywords are contextual. The lexer produces an identifier for every
word except `true`, `false` and `null`, and the parser treats a word as a
keyword only in a position where that keyword is expected. Everywhere else
the word is an ordinary identifier, so `input`, `version`, `status`, `step`
or `timeout` can be field names.

| Position | Words with special meaning |
|----------|----------------------------|
| top level | `workflow`, `schema`, `task`, `import`, `test` |
| task body | `version`, `description`, `input`, `output`, `compensated_by` |
| workflow body | `version`, `description`, `timeout`, `input`, `output`, `step`, `signal`, `query`, `return`, `on` |
| step body | `worker`, `workflow`, `sleep`, `await`, `compute`, `using`, `after`, `with`, `timeout`, `queue_timeout`, `retry`, `idempotency_key`, `when`, `for`, `in`, `max_parallel`, `compensate` |
| type | `List`, `Map`, and the primitive type names |
| expression | `if`, `then`, `else`; `input` and `execution` as path roots |
| `retry` values | `fixed`, `exponential` |
| test body | `workflow`, `version`, `input`, `mock`, `returns`, `fails`, `retryable`, `hangs`, `times`, `then`, `signal`, `expect`, `status`, `output`, `error`, `step`, `attempts`, `compensated` |

This workflow is valid, because each word is used outside its keyword
positions:

```orbit
schema step {
    workflow: String
    input: Int
    timeout: Int
}

workflow workflow {
    version "1.0.0"
    input { step: step, for: Int }
    return { input: input.step.workflow, version: input.for }
}
```

A few names are reserved in specific positions because expressions would
otherwise be ambiguous:

| Position | Reserved names | Diagnostic |
|----------|----------------|------------|
| step names | `input`, `execution`, `output`, `if`, `then`, `else` | `E0306` |
| `for` loop variables | `input`, `execution`, and the names of the workflow's steps | `E0413` |
| lambda parameters | `input`, `execution`, the names of the workflow's steps, loop variables and enclosing lambda parameters | `E0413` |
| schema names | the primitive type names, `List`, `Map` | `E0200` |
| task names | names starting with `orbit.` | `E0200` |

An object key that is not a valid identifier, or that is `true`, `false` or
`null`, must be written as a string: `{ "null": 1, "content-type": "x" }`.
Record type fields must be identifiers, so a contract field cannot be named
`true`, `false` or `null`.

## Restrictions after parsing

The grammar accepts some programs that the compiler then rejects:

| Restriction | Code |
|-------------|------|
| A workflow has a `version`. | `E0500` |
| A step has exactly one of `worker`, `workflow`, `sleep`, `await` or `compute`. | `E0301`, `E0106` |
| A step member appears at most once (except `after`). | `E0106` |
| `workflow` steps take no `using`, `retry`, `idempotency_key` or `queue_timeout`. | `E0307` |
| `sleep` steps take only `after` and `when`. | `E0307`, `E0308` |
| `await` steps take only `after`, `when` and `timeout`, and name a declared signal. | `E0307`, `E0308`, `E0315` |
| `compute` steps take only `after`, `when`, `for` and `max_parallel`. | `E0307`, `E0308` |
| Signal and query names are unique within a workflow. | `E0317`, `E0318` |
| `compensated_by` names a declared task. | `E0316` |
| Lambdas appear only as the second argument of `map`, `filter`, `find`, `any` and `all`. | `E0415` |
| `max_parallel` requires `for`. | `E0307` |
| `compensate` calls a `worker` and takes no `when`, `after`, `for` or `max_parallel`. | `E0308`, `E0307` |
| The loop variable is not in scope in `when`. | `E0401` |
| `retry` settings are known names with literal values. | `E0410` |
| Timeouts are greater than zero. `max_parallel` is 1 to 10000. | `E0410` |
| Defaults are constants of the field's type. | `E0401`, `E0400`, `E0205` |
| Map keys are `String`. | `E0204` |
| Schemas are not recursive. | `E0202` |
| The step graph and the workflow call graph are acyclic. | `E0304`, `E0311` |
| `on` is followed by `failure`, `cancel` or `timeout`. | `E0105` |
| A workflow has at most one `on` block of each kind. | `E0319` |
| Handler step names differ from workflow step names, reserved names and `failure`. | `E0302` |
| `after` in a handler names a step of the same block. | `E0303` |
| Handler steps take no `await`, child `workflow`, `for` or `max_parallel`, and no `compensate`. | `E0307`, `E0308` |
| `failure` is read only inside handler steps. | `E0401` |

The full list of codes is in [Compiler diagnostics](../reference/diagnostics.md).

## See also

- [Expressions](expressions.md)
- [Files and imports](files.md)
- [Steps](steps.md)
- [Compiler diagnostics](../reference/diagnostics.md)
- [Formatter](../tooling/fmt.md)
