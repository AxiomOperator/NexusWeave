#!/usr/bin/env sh
# Fetch the ground-truth indexers into bench/.cache/tools (nothing is installed system-wide).
# Versions are pinned so benchmark numbers stay comparable; bump them deliberately and
# regenerate the baseline (bench/accuracy.py --refresh --update-baseline) when you do.
set -eu

NODE_VERSION=22.12.0
GO_VERSION=1.23.4
SCIP_TYPESCRIPT=0.4.0
SCIP_PYTHON=0.6.8
SCIP_GO=v0.2.9
RUST_ANALYZER=2026-10-19

here=$(cd "$(dirname "$0")" && pwd)
tools="$here/.cache/tools"
mkdir -p "$tools"
cd "$tools"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64)  node_os=linux-x64;   go_os=linux-amd64;  ra=rust-analyzer-x86_64-unknown-linux-gnu ;;
  Linux-aarch64) node_os=linux-arm64; go_os=linux-arm64;  ra=rust-analyzer-aarch64-unknown-linux-gnu ;;
  Darwin-arm64)  node_os=darwin-arm64; go_os=darwin-arm64; ra=rust-analyzer-aarch64-apple-darwin ;;
  Darwin-x86_64) node_os=darwin-x64;  go_os=darwin-amd64; ra=rust-analyzer-x86_64-apple-darwin ;;
  *) echo "unsupported platform $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

if [ ! -x node/bin/node ] || [ "$(node/bin/node --version)" != "v$NODE_VERSION" ]; then
  echo "· node $NODE_VERSION"
  rm -rf node
  curl -fsSL "https://nodejs.org/dist/v$NODE_VERSION/node-v$NODE_VERSION-$node_os.tar.xz" | tar xJ
  mv "node-v$NODE_VERSION-$node_os" node
fi

if [ ! -x go/bin/go ] || ! go/bin/go version | grep -q "go$GO_VERSION "; then
  echo "· go $GO_VERSION"
  rm -rf go
  curl -fsSL "https://go.dev/dl/go$GO_VERSION.$go_os.tar.gz" | tar xz
fi

export PATH="$tools/node/bin:$tools/go/bin:$PATH"

echo "· scip-typescript $SCIP_TYPESCRIPT, scip-python $SCIP_PYTHON"
mkdir -p npm
cd npm
[ -f package.json ] || npm init -y >/dev/null
npm install --silent --no-audit --no-fund "@sourcegraph/scip-typescript@$SCIP_TYPESCRIPT" "@sourcegraph/scip-python@$SCIP_PYTHON"
cd ..

echo "· scip-go $SCIP_GO"
GOBIN="$tools/gobin" GOPATH="$tools/gopath" GOTOOLCHAIN=auto go install "github.com/scip-code/scip-go/cmd/scip-go@$SCIP_GO"

if [ ! -x rust-analyzer ] || [ "$(cat rust-analyzer.version 2>/dev/null)" != "$RUST_ANALYZER" ]; then
  echo "· rust-analyzer $RUST_ANALYZER"
  curl -fsSL "https://github.com/rust-lang/rust-analyzer/releases/download/$RUST_ANALYZER/$ra.gz" | gunzip > rust-analyzer
  chmod +x rust-analyzer
  echo "$RUST_ANALYZER" > rust-analyzer.version
fi

echo "tools ready in $tools"
