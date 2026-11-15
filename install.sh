#!/bin/sh
# Install the orbit binary from the latest GitHub release.
#
#   curl -fsSL https://raw.githubusercontent.com/AjmalX0/Orbit/main/install.sh | sh
#
# Environment: ORBIT_VERSION (default: latest), ORBIT_INSTALL_DIR (default:
# ~/.local/bin, or /usr/local/bin when run as root), ORBIT_DOWNLOAD_URL (a
# mirror serving the release assets; overrides the GitHub URL).
set -eu

REPO="AjmalX0/Orbit"
VERSION="${ORBIT_VERSION:-latest}"

os=$(uname -s)
arch=$(uname -m)
case "$os-$arch" in
  Linux-x86_64)  target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Darwin-arm64)  target=aarch64-apple-darwin ;;
  *) echo "orbit: no prebuilt binary for $os/$arch; install with: cargo install orbit-lang" >&2; exit 1 ;;
esac

if [ "$(id -u)" = 0 ]; then default_dir=/usr/local/bin; else default_dir="$HOME/.local/bin"; fi
dir="${ORBIT_INSTALL_DIR:-$default_dir}"

if [ -n "${ORBIT_DOWNLOAD_URL:-}" ]; then
  base="$ORBIT_DOWNLOAD_URL"
elif [ "$VERSION" = latest ]; then
  base="https://github.com/$REPO/releases/latest/download"
else
  base="https://github.com/$REPO/releases/download/$VERSION"
fi
asset="orbit-$target.tar.gz"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "orbit: downloading $asset ($VERSION)"
curl -fsSL "$base/$asset" -o "$tmp/$asset"
curl -fsSL "$base/SHA256SUMS" -o "$tmp/SHA256SUMS"
expected=$(grep " $asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then actual=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
else actual=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1); fi
if [ -z "$expected" ] || [ "$expected" != "$actual" ]; then
  echo "orbit: checksum mismatch for $asset" >&2
  exit 1
fi
tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$dir"
install -m 0755 "$tmp/orbit" "$dir/orbit"
echo "orbit: installed $("$dir/orbit" --version) to $dir/orbit"
case ":$PATH:" in *":$dir:"*) ;; *) echo "orbit: add $dir to your PATH" ;; esac
