# Security policy

## Reporting a vulnerability

Please **do not** open a public issue. Report privately through GitHub:
*Security → Report a vulnerability* on this repository (private vulnerability
reporting). Include steps to reproduce, the affected version and the impact.
You should get an acknowledgement within a few days.

## Supported versions

| Version | Supported |
|---|---|
| 0.1.x | yes |

## Scope and hardening

The runtime's security model (credentials, per-worker identities, TLS and
mutual TLS, input validation, redaction) is described in
[Security](docs/src/runtime/security.md). In production:

- set `--token`, give workers per-worker identities (`--workers-file`), and
  serve TLS (`--tls-cert`/`--tls-key`, optionally `--tls-client-ca`);
- keep the HTTP adapter disabled unless you need it;
- use `sslmode=require` for PostgreSQL.
