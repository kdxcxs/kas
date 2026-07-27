#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
PROFILE="${1:-smoke}"
shift || true

cd "$ROOT"
cargo build --release \
  -p kas-api \
  -p kas-migrate \
  -p kas-admin \
  -p kas-builtin-driver \
  -p kas-benchmark \
  --bins

exec "$ROOT/target/release/kas-benchmark" "$PROFILE" "$@"
