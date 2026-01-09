#!/usr/bin/env bash

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

MANIFEST="ra/Cargo.toml"

if ! command -v cargo >/dev/null 2>&1; then
  echo "error: cargo not found" >&2
  exit 1
fi

if ! command -v git >/dev/null 2>&1; then
  echo "error: git not found" >&2
  exit 1
fi

if [[ ! -f "$MANIFEST" ]]; then
  echo "error: expected $MANIFEST to exist (run from repo root)" >&2
  exit 1
fi

BRANCH="$(git rev-parse --abbrev-ref HEAD)"
echo "git branch: $BRANCH"

if [[ -n "$(git status --porcelain=v1)" ]]; then
  echo "note: working tree is dirty (this is normal if you just bumped the version)." >&2
  echo "      make sure you only have the intended release changes, then commit after checks pass." >&2
  git status --porcelain=v1 >&2
fi

echo "==> cargo fmt"
(cd ra && cargo fmt)

echo "==> cargo test"
(cd ra && cargo test -q)

echo "==> cargo clippy"
(cd ra && cargo clippy --all-targets -- -D warnings)

echo "==> cargo build --release"
cargo build --release --manifest-path "$MANIFEST"

echo "==> optional: update lockfile"
echo "    run: cargo generate-lockfile --manifest-path $MANIFEST"
echo "    then commit the resulting Cargo.lock changes (if any)."

echo "ok: release prep checks passed"


