#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

cargo test -p tabor --test web_e2e browser_native_clipboard_shortcuts_smoke -- --exact --nocapture
cargo test -p tabor --test web_e2e browser_clipboard_api_smoke -- --exact --nocapture
