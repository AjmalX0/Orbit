# Releasing

Releases are cut by pushing a version tag. `.github/workflows/release.yml`
builds and checks everything on every push, and publishes only for tags.

1. Update the version everywhere (`0.1.0` → `X.Y.Z`): `Cargo.toml`
   (`[workspace.package]` and the `version = "…"` on internal dependencies),
   `sdks/python/pyproject.toml`, `sdks/typescript/package.json`,
   `sdks/java/pom.xml`, `sdks/csharp/OrbitLang.Sdk/OrbitLang.Sdk.csproj`,
   `editors/vscode/package.json`, the SDK version constants
   (`SDK_VERSION` / `Version` / `sdk` strings) and `CHANGELOG.md`.
2. Run `./scripts/check.sh`.
3. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`.

The tag produces:

| Artifact | Where | Needs |
|---|---|---|
| `orbit` binaries (Linux x86_64/arm64, macOS x86_64/arm64, Windows x86_64) + `SHA256SUMS` | GitHub Release | nothing |
| Container image `ghcr.io/<owner>/orbit:X.Y.Z` and `:latest` | GitHub Packages | nothing |
| Go module tag `sdks/go/vX.Y.Z` | the repository | nothing |
| Crates `orbit-syntax`, `orbit-ir`, `orbit-compiler`, `orbit-runtime`, `orbit-worker`, `orbit-lang` | crates.io | secret `CARGO_REGISTRY_TOKEN` |
| `orbit-lang-sdk` | PyPI | trusted publisher for this workflow (environment `pypi`), or secret `PYPI_API_TOKEN` |
| `@orbit-lang/sdk` | npm | secret `NPM_TOKEN` (the `@orbit-lang` scope must exist) |
| `OrbitLang.Sdk` | NuGet | secret `NUGET_API_KEY` |
| `io.github.ajmalx0:orbit-sdk` | Maven Central | secrets `MAVEN_CENTRAL_USERNAME`, `MAVEN_CENTRAL_PASSWORD`, `MAVEN_GPG_PRIVATE_KEY`, `MAVEN_GPG_PASSPHRASE` (namespace `io.github.ajmalx0` verified on central.sonatype.com) |
| VS Code extension `orbit-lang.orbit-lang` | Marketplace / Open VSX | secrets `VSCE_PAT` (publisher `orbit-lang`) / `OVSX_PAT` |

A registry without credentials is skipped, not failed, so you can enable them
one at a time.
