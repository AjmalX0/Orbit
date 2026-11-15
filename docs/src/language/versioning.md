# Versioning

ORBIT versions three things independently: workflow definitions, task
contracts and the compiled IR format. A registered workflow version never
changes, running executions stay on the exact definition they started with,
and workers only receive tasks for contract versions they implement. This page
explains each kind of version, what changes a definition's identity, and how
to roll out changes safely.

## Workflow versions

Every workflow declares a version (`E0500` if missing):

```orbit
workflow Invoice {
    version "2.1.0"
    input { customer_id: String }
    step pause { sleep 1s }
}
```

The version is a string. The compiler warns (`W0201`) when it is not
`MAJOR.MINOR.PATCH`; pre-release and build suffixes such as `2.1.0-rc.1` or
`2.1.0+build.5` are accepted.

### Immutability

A workflow definition is identified by `name@version`. When you register a
definition (`orbit deploy`, `orbit run <file>`, or `POST /v1/workflows`), the
runtime compares its definition hash with any definition already registered
under the same name and version:

| Situation | Result |
|-----------|--------|
| No definition with that `name@version` | Registered (HTTP 201). |
| Same `name@version`, same hash | No change (HTTP 200, `up to date`). |
| Same `name@version`, different hash | Rejected with `DEFINITION_CONFLICT` (HTTP 409). |

```text
error: DEFINITION_CONFLICT (409): Invoice@2.1.0 is already registered with a different definition (sha256:…); workflow definitions are immutable, bump the version
```

To change a workflow, bump its version and register the new definition. Both
versions stay registered.

### The definition hash

The definition hash is a SHA-256 hash (`sha256:<hex>`) of the compiled
definition's semantic content. It covers everything that affects execution:
input and output types, steps, expressions, policies, and the contracts the
workflow uses, including the definition hashes of child workflows. It
excludes source locations, so these changes keep the hash:

- reformatting, including `orbit fmt`,
- editing comments,
- moving top-level declarations between files or reordering them.

These changes produce a new hash, and therefore need a new version:

- any change to the workflow's input, output, steps or `return`,
- any change to a task contract the workflow uses, including its output type
  or contract version,
- any change to a child workflow it starts (because the child's hash changes),
- a change to a schema the workflow uses, including renaming it,
- reordering steps that do not depend on each other, because such steps are
  stored in source order.

Changing a shared contract or a child workflow therefore requires bumping
every workflow that uses it.

### Executions are pinned

An execution records the definition hash it started with and runs that
definition to the end, even after newer versions are registered. Retries,
redeliveries, crash recovery and compensations all use the pinned definition.

When you start an execution without naming a version (`orbit run Name`, or a
start request without `version`), the runtime picks the registered version
with the highest numeric `MAJOR.MINOR.PATCH`. Pass `--version` (or `version`
in the API) to start a specific one.

### Child workflow versions

A `workflow "Name"` step is compiled against the child definition in the same
compilation and pins that definition's hash. The parent always starts exactly
that child definition, even if a newer version of the child is registered
later. Registering a parent whose child definition is not registered fails
with `CHILD_NOT_REGISTERED`; `orbit deploy` registers children first. See
[Child workflows](child-workflows.md).

## Contract versions

A task contract has its own version, `"1.0.0"` if omitted:

```orbit
task "payments.charge" {
    version "2.0.0"
    input { order_id: String, amount_cents: Int, currency: String }
    output { transaction_id: String }
}
```

Workers announce the task names and contract versions they implement. The
runtime gives a task only to workers that announced the same name and the
same version, so a workflow compiled against `payments.charge@2.0.0` never
reaches a worker that only implements `1.0.0`.

Use the contract version to roll out incompatible changes:

1. Deploy workers that implement both the old and the new contract version.
2. Change the contract, bump its version, and bump the versions of the
   workflows that use it. Register them.
3. Let executions on the old workflow versions finish.
4. Remove the old contract version from the workers.

Compatible additions, such as a new optional output field, can keep the
contract version. Workers may also return fields that the contract does not
declare; the runtime ignores them during validation. The workflows that use
the contract still get a new definition hash, so they still need a version
bump.

## IR version

Every compiled definition records its IR format in `ir_version`. The compiler
chooses the lowest version that can express the workflow:

| IR version | Used for |
|---|---|
| `orbit-ir/1` | Every workflow that uses none of the features below. |
| `orbit-ir/2` | A workflow that declares a `signal` or a `query`, has an `await` or `compute` step, or uses a lambda in any expression (`with`, `when`, `for`, `idempotency_key`, `compensate`, `return`). |

A workflow that does not use these features compiles exactly as before: it
keeps `orbit-ir/1` and the same definition hash, so upgrading the compiler
never forces a version bump or causes a `DEFINITION_CONFLICT`. Using one of
the features changes the definition and needs a new workflow version, like
any other change. `compensated_by` on a contract is not an `orbit-ir/2`
feature, but it is part of the contract recorded in the definition, so adding
it changes the hash of every workflow that calls the task.

```console
$ orbit compile --stdout examples/order_processing.orbit | grep ir_version
  "ir_version": "orbit-ir/1",
$ orbit compile --stdout examples/approval.orbit | grep ir_version
  "ir_version": "orbit-ir/2",
```

The runtime accepts every IR version it supports and rejects any other with
`400 INVALID_IR` (`` unsupported IR version `orbit-ir/3`; this runtime supports `orbit-ir/1`, `orbit-ir/2` ``),
instead of misreading it. It also rejects an `orbit-ir/1` definition that uses
an `orbit-ir/2` feature. `GET /v1/health` lists the supported versions in
`ir_versions`:

```json
{"status":"ok","api_version":"orbit-api/1","runtime_version":"0.2.0","ir_versions":["orbit-ir/1","orbit-ir/2"],"active_executions":0,"worker_sessions":2}
```

A runtime that only lists `orbit-ir/1` (ORBIT 0.1) cannot register
workflows that use signals, queries, `await`, `compute` or lambdas. Upgrade
the runtime before you deploy them; definitions already registered keep
working after the upgrade.

## Versioning checklist

- Bump the workflow version for every change that is not formatting or
  comments.
- Bump the contract version when old workers cannot handle the new contract.
- After changing a contract or a child workflow, bump every workflow that uses
  it.
- Use `MAJOR.MINOR.PATCH` versions, so that "latest" selection works as
  expected.
- Treat signals and queries like the input and output: senders and readers
  depend on them. Run [`orbit compat`](../tooling/compat.md) to see which
  changes break them.
- Before deploying a workflow that compiles to `orbit-ir/2`, check that the
  runtime lists it in `GET /v1/health`.

## See also

- [Workflows](workflows.md)
- [Task contracts](tasks.md#changing-a-contract)
- [Child workflows](child-workflows.md)
- [Signals and queries](signals.md)
- [Compatibility checks](../tooling/compat.md)
- [Executions and history](../runtime/executions.md)
- [CLI](../reference/cli.md)
