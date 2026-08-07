#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

cargo test -p tabor --features signed-web-e2e --test web_e2e web_popup_smoke -- --exact --nocapture
