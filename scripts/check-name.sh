#!/usr/bin/env bash
# Fails if the app name appears anywhere outside its single definition in Cargo.toml.
# Prose docs (*.md) are exempt; they describe the product by name.
set -euo pipefail
cd "$(dirname "$0")/.."
name=$(sed -n 's/^name = "\(.*\)"/\1/p' Cargo.toml | head -1)
hits=$(rg -n -w --fixed-strings "$name" \
  -g '!Cargo.toml' -g '!Cargo.lock' -g '!*.md' -g '!target/**' . || true)
if [[ -n "$hits" ]]; then
  echo "Hard-coded app name \"$name\" found; derive it from src/app.rs instead:" >&2
  echo "$hits" >&2
  exit 1
fi
echo "ok: \"$name\" defined only in Cargo.toml"
