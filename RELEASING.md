# Releasing

Releases are cut by pushing a version tag, or with **Actions → release → Run workflow**. `.github/workflows/release.yml`
builds and checks everything on every push, and publishes only for tags.

1. `./scripts/bump-version.sh X.Y.Z` sets the version in every package and SDK
   constant; add a `CHANGELOG.md` entry and commit.
2. Run `./scripts/check.sh`.
3. Tag and push: `git tag vX.Y.Z && git push origin vX.Y.Z`. Or, from the browser:
   **Actions → release → Run workflow**, enter `X.Y.Z`; the workflow tags the
   selected commit itself.

The tag produces:

| Artifact | Where | Needs |
|---|---|---|
| `orbit` binaries (Linux x86_64/arm64, Windows x86_64, and macOS x86_64/arm64 when the repository variable `MACOS_BUILDS` is `true`) + `SHA256SUMS`, `install.sh`, the VS Code `.vsix` | GitHub Release | nothing |
| Container image `ghcr.io/<owner>/orbit:vX.Y.Z` and `:latest` (linux/amd64) | GitHub Packages | nothing |
| Go module tag `sdks/go/vX.Y.Z` | the repository | nothing |
| Crates `orbit-syntax`, `orbit-ir`, `orbit-compiler`, `orbit-runtime`, `orbit-worker`, `orbit-lang` | crates.io | secret `CARGO_REGISTRY_TOKEN` |
| `orbit-lang-sdk` | PyPI | secret `PYPI_API_TOKEN`, or trusted publishing (environment `pypi`) with repository variable `PYPI_TRUSTED_PUBLISHING=true` |
| `orbit-lang` | npm | trusted publishing (on npmjs.com: package `orbit-lang` → Settings → Trusted publisher → GitHub Actions, workflow `release.yml`) with repository variable `NPM_TRUSTED_PUBLISHING=true`; or secret `NPM_TOKEN` |
| `OrbitLang.Sdk` | NuGet | secret `NUGET_API_KEY` |
| `io.github.ajmalx0:orbit-sdk` | Maven Central | secrets `MAVEN_CENTRAL_USERNAME`, `MAVEN_CENTRAL_PASSWORD`, `MAVEN_GPG_PRIVATE_KEY`, `MAVEN_GPG_PASSPHRASE` (namespace `io.github.ajmalx0` verified on central.sonatype.com) |
| VS Code extension `orbit-lang.orbit-lang` | Marketplace / Open VSX | secrets `VSCE_PAT` (publisher `orbit-lang`) / `OVSX_PAT` |

A registry without credentials is skipped, not failed, so you can enable them
one at a time.

## Documentation website

`.github/workflows/pages.yml` builds the landing page (`website/`) and the
documentation (`docs/`, an mdBook) on every push and deploys them to GitHub
Pages from `main`. Build locally with `./scripts/build-site.sh` (needs
`cargo install mdbook --version 0.4.52 --locked`); the output is in
`target/site`. In the repository settings, **Pages → Source** must be set to
**GitHub Actions** once.
