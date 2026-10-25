#!/usr/bin/env sh
# Install the latest nexusweave release:
#   curl -fsSL https://raw.githubusercontent.com/AxiomOperator/NexusWeave/main/scripts/install.sh | sh
# GITHUB_TOKEN / GH_TOKEN is sent when set (higher API rate limit; required for a private fork).
# Options (environment): NEXUSWEAVE_VERSION=v0.2.0, NEXUSWEAVE_INSTALL_DIR=~/.local/bin, NEXUSWEAVE_REPO=owner/name
set -eu

REPO="${NEXUSWEAVE_REPO:-AxiomOperator/NexusWeave}"
API="${NEXUSWEAVE_API:-https://api.github.com}"   # override for testing against a mock
DIR="${NEXUSWEAVE_INSTALL_DIR:-$HOME/.local/bin}"
TOKEN="${GITHUB_TOKEN:-${GH_TOKEN:-}}"

err() { echo "nexusweave install: $*" >&2; exit 1; }
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

# the token goes to curl through a config on stdin, not argv (visible to other users via `ps`)
api() {
  if [ -n "$TOKEN" ]; then
    printf 'header = "Authorization: Bearer %s"\n' "$(printf '%s' "$TOKEN" | tr -d '"\\\n')" \
      | curl -fsSL --config - -H "Accept: $1" "$2"
  else curl -fsSL -H "Accept: $1" "$2"; fi
}

# accept 0.2.0 as well as v0.2.0
if [ -n "${NEXUSWEAVE_VERSION:-}" ]; then rel="tags/v${NEXUSWEAVE_VERSION#v}"; else rel=latest; fi
hint=""; [ -n "$TOKEN" ] || hint=" (private repository? export GITHUB_TOKEN)"
json=$(api application/vnd.github+json "$API/repos/$REPO/releases/$rel") \
  || err "cannot read release $rel of $REPO$hint"

# asset API URL for a file name (the API URL, unlike browser_download_url, works for private
# repos with a token); each asset object lists its .../releases/assets/<id> url before its name
asset_url() {
  printf '%s' "$json" | tr ',{' '\n\n' | awk -v want="$1" '
    /"url": *"[^"]*\/releases\/assets\// { u = $0; sub(/.*"url": *"/, "", u); sub(/".*/, "", u) }
    /"name": *"/ { n = $0; sub(/.*"name": *"/, "", n); sub(/".*/, "", n); if (n == want && u != "") { print u; exit } }'
}
sums=$(asset_url SHA256SUMS); [ -n "$sums" ] || err "release has no SHA256SUMS"
tmp=$(mktemp -d); trap 'rm -rf "$tmp"' EXIT
api application/octet-stream "$sums" > "$tmp/SHA256SUMS" || err "downloading SHA256SUMS failed"

# download, verify and unpack one build into $tmp/<target>/; fails if the release lacks it
fetch() {
  a="nexusweave-$1.tar.gz"
  u=$(asset_url "$a"); [ -n "$u" ] || return 1
  api application/octet-stream "$u" > "$tmp/$a" || err "downloading $a failed"
  want=$(grep " \*\{0,1\}$a\$" "$tmp/SHA256SUMS" | cut -d' ' -f1)
  if command -v sha256sum >/dev/null 2>&1; then got=$(sha256sum "$tmp/$a" | cut -d' ' -f1)
  else got=$(shasum -a 256 "$tmp/$a" | cut -d' ' -f1); fi
  [ -n "$want" ] && [ "$want" = "$got" ] || err "checksum mismatch for $a"
  mkdir -p "$tmp/$1" && tar -xzf "$tmp/$a" -C "$tmp/$1"
}

fetch "$target" || err "no build for $target in this release"
bin="$tmp/$target/nexusweave"
# the gnu build needs glibc 2.35+; on older systems use the static musl build when there is one.
# A binary that runs but has the loader warn (an optional newer symbol version) counts as broken:
# the warning would print on every run.
runs() {
  v=$("$1" --version 2>&1) && [ "$(printf '%s\n' "$v" | wc -l)" -eq 1 ] || return 1
  case "$v" in "nexusweave "*) return 0 ;; *) return 1 ;; esac
}
if [ "$os" = unknown-linux-gnu ] && ! runs "$bin"; then
  musl="$arch-unknown-linux-musl"
  fetch "$musl" || err "$target doesn't run here (glibc older than 2.35?) and this release has no $musl build"
  bin="$tmp/$musl/nexusweave"
  echo "note: the glibc build doesn't run on this system; installing the static $musl build"
fi
mkdir -p "$DIR"
mv "$bin" "$DIR/nexusweave"
chmod 755 "$DIR/nexusweave"
echo "installed $("$DIR/nexusweave" --version) to $DIR/nexusweave"
case ":$PATH:" in *":$DIR:"*) ;; *) echo "note: add $DIR to your PATH" ;; esac
echo "next: nexusweave install   # register with your coding agents (add --hooks for the Claude Code hint)"
