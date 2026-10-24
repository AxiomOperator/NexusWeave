#!/usr/bin/env sh
# Install the latest xgraph release:
#   curl -fsSL https://raw.githubusercontent.com/AxiomOperator/xGraph/main/scripts/install.sh | sh
# Private repository: export GITHUB_TOKEN (a token with read access) first.
# Options (environment): XGRAPH_VERSION=v0.2.0, XGRAPH_INSTALL_DIR=~/.local/bin, XGRAPH_REPO=owner/name
set -eu

REPO="${XGRAPH_REPO:-AxiomOperator/xGraph}"
DIR="${XGRAPH_INSTALL_DIR:-$HOME/.local/bin}"
TOKEN="${GITHUB_TOKEN:-${GH_TOKEN:-}}"

err() { echo "xgraph install: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "needs $1"; }
need curl; need tar; need uname

case "$(uname -s)" in
  Linux) os=unknown-linux-gnu ;;
  Darwin) os=apple-darwin ;;
  *) err "unsupported OS $(uname -s); on Windows download the .zip from the releases page" ;;
esac
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  arm64|aarch64) arch=aarch64 ;;
  *) err "unsupported architecture $(uname -m)" ;;
esac
target="$arch-$os"
asset="xgraph-$target.tar.gz"

api() {
  if [ -n "$TOKEN" ]; then curl -fsSL -H "Authorization: Bearer $TOKEN" -H "Accept: $1" "$2"
  else curl -fsSL -H "Accept: $1" "$2"; fi
}

# accept 0.2.0 as well as v0.2.0
if [ -n "${XGRAPH_VERSION:-}" ]; then rel="tags/v${XGRAPH_VERSION#v}"; else rel=latest; fi
hint=""; [ -n "$TOKEN" ] || hint=" (private repository? export GITHUB_TOKEN)"
json=$(api application/vnd.github+json "https://api.github.com/repos/$REPO/releases/$rel") \
  || err "cannot read release $rel of $REPO$hint"

# asset API URL for a file name (the API URL, unlike browser_download_url, works for private
# repos with a token); each asset object lists its .../releases/assets/<id> url before its name
asset_url() {
  printf '%s' "$json" | tr ',{' '\n\n' | awk -v want="$1" '
    /"url": *"[^"]*\/releases\/assets\// { u = $0; sub(/.*"url": *"/, "", u); sub(/".*/, "", u) }
    /"name": *"/ { n = $0; sub(/.*"name": *"/, "", n); sub(/".*/, "", n); if (n == want && u != "") { print u; exit } }'
}
url=$(asset_url "$asset"); [ -n "$url" ] || err "no build for $target in this release"
sums=$(asset_url SHA256SUMS); [ -n "$sums" ] || err "release has no SHA256SUMS"

tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
api application/octet-stream "$url" > "$tmp/$asset" || err "downloading $asset failed"
api application/octet-stream "$sums" > "$tmp/SHA256SUMS" || err "downloading SHA256SUMS failed"

want=$(grep " \*\{0,1\}$asset\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp/$asset" | cut -d' ' -f1)
else got=$(shasum -a 256 "$tmp/$asset" | cut -d' ' -f1); fi
[ -n "$want" ] && [ "$want" = "$got" ] || err "checksum mismatch for $asset"

tar -xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$DIR"
mv "$tmp/xgraph" "$DIR/xgraph"
chmod 755 "$DIR/xgraph"
echo "installed $("$DIR/xgraph" --version) to $DIR/xgraph"
case ":$PATH:" in *":$DIR:"*) ;; *) echo "note: add $DIR to your PATH" ;; esac
echo "next: xgraph install   # register with your coding agents (add --hooks for the Claude Code hint)"
