#!/usr/bin/env bash
# Runs every check CI runs, skipping toolchains that are not installed.
# Usage: ./scripts/check.sh            (from the repository root)
set -euo pipefail
cd "$(dirname "$0")/.."
step() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }

step "Rust: format, lint, test"
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test

step "ORBIT sources are formatted"
cargo run -q -- fmt --check examples/*.orbit

step "ORBIT example tests"
cargo run -q -- test examples/*.test.orbit
cargo run -q -- check --strict examples/onboarding.orbit examples/expense.orbit examples/agent.orbit examples/approval.orbit examples/checkout.orbit
cargo run -q -- check --policy examples/policies/kyc.toml examples/onboarding.orbit
cargo run -q -- check --policy examples/policies/fintech.toml examples/approval.orbit

step "Python SDK"
PYTHONPATH=sdks/python python3 -m unittest discover -s sdks/python/tests

if have node; then
  step "TypeScript SDK"
  (cd sdks/typescript && npx -y -p typescript@5.9.3 tsc -p tsconfig.json --noEmit && npm test)
fi
if have go; then
  step "Go SDK"
  test -z "$(gofmt -l sdks/go examples/workers/polyglot/go)"
  (cd sdks/go && go vet ./... && go test -race ./...)
fi
if have mvn; then
  step "Java SDK"
  (cd sdks/java && mvn -B -q verify)
fi
if have dotnet; then
  step "C# SDK"
  (cd sdks/csharp && dotnet test --nologo -v q)
fi

step "End-to-end (real runtime, polyglot workers, crash recovery, TLS, identities)"
cargo build -q
python3 -I tests/e2e/test_e2e.py

step "All checks passed"
