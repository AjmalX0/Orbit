#!/usr/bin/env sh
# Sets every package to one version: scripts/bump-version.sh 0.2.0
# Then: ./scripts/check.sh, commit, and Actions -> release -> Run workflow.
set -eu
new="${1:?usage: scripts/bump-version.sh X.Y.Z}"
echo "$new" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+$' || { echo "not a version: $new" >&2; exit 1; }
cd "$(dirname "$0")/.."
old=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
[ "$old" != "$new" ] || { echo "already $new"; exit 0; }
files="Cargo.toml
crates/orbit-cli/src/scaffold.rs
editors/vscode/package.json
sdks/python/pyproject.toml
sdks/python/orbit_sdk/__init__.py
sdks/python/orbit_sdk/worker.py
sdks/typescript/package.json
sdks/typescript/src/index.ts
sdks/go/orbit.go
sdks/java/pom.xml
sdks/java/README.md
sdks/java/src/main/java/io/github/ajmalx0/orbit/Worker.java
sdks/java/src/test/java/io/github/ajmalx0/orbit/WorkerTest.java
sdks/csharp/OrbitLang.Sdk/OrbitLang.Sdk.csproj
sdks/csharp/OrbitLang.Sdk/Worker.cs
sdks/csharp/OrbitLang.Sdk.Tests/WorkerTests.cs
CITATION.cff"
esc=$(echo "$old" | sed 's/\./\\./g')
for f in $files; do
  [ -f "$f" ] || continue
  sed "s/$esc/$new/g" "$f" > "$f.tmp" && mv "$f.tmp" "$f"
done
sed "s/^date-released: .*/date-released: $(date +%F)/" CITATION.cff > CITATION.cff.tmp && mv CITATION.cff.tmp CITATION.cff
cargo update --workspace --quiet
echo "version $old -> $new. Add a CHANGELOG.md entry, run ./scripts/check.sh, commit, then release."
