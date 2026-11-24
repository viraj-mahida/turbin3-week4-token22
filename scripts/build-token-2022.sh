#!/usr/bin/env bash
# Rebuilds tests/fixtures/spl_token_2022.so from source at the same commit the
# Rust client crates in Cargo.toml are pinned to.
set -euo pipefail

REV=1d91cfca9395a24adcdc453b38cb776d9624ff8e
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="${TMPDIR:-/tmp}/token-2022-$REV"

if [ ! -d "$WORK" ]; then
  git clone https://github.com/solana-program/token-2022 "$WORK"
fi
git -C "$WORK" checkout -q "$REV"

(cd "$WORK" && cargo build-sbf --manifest-path program/Cargo.toml)
cp "$WORK/target/deploy/spl_token_2022.so" "$ROOT/tests/fixtures/spl_token_2022.so"
shasum -a 256 "$ROOT/tests/fixtures/spl_token_2022.so"
