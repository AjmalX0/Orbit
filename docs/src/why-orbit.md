# Why ORBIT

ORBIT is for teams that want a business process to be a definition they can
read, review, check and test as a whole, across services written in several
languages. This page describes the problems it is built for, when another
tool is a better fit, and how it compares with code-first durable execution
engines such as Temporal.

## The idea: the process is a typed contract

In ORBIT, a workflow is a declaration, not a program: which tasks run, in
what order, under which conditions, with which retries and deadlines, how
each side effect is undone, which human decisions it waits for, and what
happens when it fails, is cancelled or times out. The compiler checks that
declaration against the contracts of the services that do the work. That
gives guarantees a process written as code does not have by default:

- **Every service call matches its contract**, in every language. A field
  renamed in a worker's contract breaks the build, not production.
- **Every declared side effect has an undo action.** A task that declares
  `compensated_by` must be compensated wherever it is used
  (`orbit check --strict`).
- **Every human decision is typed** and every wait can be required to have a
  deadline.
- **Every path is testable** with `test` blocks that run in milliseconds,
  with time simulated.
- **A change is reviewable**: `orbit compat` reports breaking changes and the
  version bump they need, and running executions keep the version they
  started with.

## What it is built for

### Business processes you have to be able to explain

Onboarding and KYC, lending and underwriting, claims, refunds, orders,
procurement. Processes where an auditor, a compliance officer or a product
owner asks "what happens if...?" and the answer should be in one file, not
spread across services. See
[Typed business process contracts](guides/business-processes.md).

### Orchestration across services in several languages

One workflow can call workers in Python, TypeScript, Go, Rust, Java and C#,
and the compiler checks every call against its contract, whatever language
implements it. Teams keep their services in the language they chose; the
process does not belong to any one of them. See
[Polyglot workflows](guides/polyglot.md).

### Human-in-the-loop work

Approvals, reviews and exception handling, with typed decisions, deadlines,
escalation and a record of who decided what. See
[Human approvals](guides/approvals.md).

### Sagas

Multi-step operations that must be undone in reverse when a later step
fails: reserve, charge, ship. Compensation is declared per step and checked by
the compiler; outcome handlers run after it. See
[Orders with compensation](guides/saga.md).

### AI agents that must not run away

Durable tool calls with typed results, bounded fan-out, budgets, guardrail
checks and human review. See [AI agents with guardrails](guides/ai-agents.md).

### Small teams that want durability without a platform team

One binary. SQLite for development and small deployments, PostgreSQL for
production, several servers when you need failover or more throughput.
No separate history, matching and frontend services to run.

## When to use something else

- **The logic is genuinely algorithmic.** Loops of unknown length, complex
  data structures, dynamic decisions about what to call next: these are
  natural in code and awkward in a declaration. A code-first engine fits
  better, or keep that logic in a worker and orchestrate around it.
- **You need a managed service today.** ORBIT has no hosted offering; you run
  it yourself.
- **You need multi-region active-active execution**, or Temporal-specific
  features such as Nexus, update handlers or worker build-ID versioning.
- **Data pipelines and batch jobs** with large datasets, where tools such as
  Airflow, Dagster or Spark are built for scheduling and data movement.

## Compared with Temporal

Both record workflow history durably, recover by replaying it, deliver work
to workers at least once, and support several languages. The difference is
where the process lives: in Temporal it is code in one SDK language; in ORBIT
it is a typed declaration checked against contracts in every language. The
[FAQ](faq.md#how-does-orbit-compare-with-temporal) has a feature-by-feature
table. In short:

| You want | Better fit |
|----------|-----------|
| The process reviewed and checked as a whole, with compile-time guarantees | ORBIT |
| One definition orchestrating services in several languages | ORBIT |
| Arbitrary control flow in a general-purpose language | Temporal |
| A large ecosystem, a managed cloud and years of production use | Temporal |
| A single binary you can run on a laptop or a small VM | ORBIT |

## Maturity

ORBIT is young. The runtime is tested end to end in six languages, with crash
recovery, TLS, failover and several active servers, and the
[performance](runtime/performance.md) page shows measured numbers and what
bounds them. It does not yet have years of production use. Read the
[delivery guarantees](runtime/guarantees.md) before you rely on it, and
[report what you find](https://github.com/AjmalX0/Orbit/issues).

## See also

- [Core concepts](getting-started/concepts.md)
- [Quick start](getting-started/quickstart.md)
- [FAQ and troubleshooting](faq.md)
