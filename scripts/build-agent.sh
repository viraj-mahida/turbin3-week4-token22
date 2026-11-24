#!/usr/bin/env bash
# Builds the remit-agent program and copies it to tests/fixtures/.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
(cd "$ROOT/programs/remit-agent" && cargo build-sbf)
cp "$ROOT/programs/remit-agent/target/deploy/remit_agent.so" "$ROOT/tests/fixtures/remit_agent.so"
shasum -a 256 "$ROOT/tests/fixtures/remit_agent.so"
