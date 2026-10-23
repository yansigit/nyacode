#!/bin/bash

# Set "chatgpt.cliExecutable": "/Users/<USERNAME>/code/codex/scripts/debug-codex.sh" in VSCode settings to always get the 
# latest codex-rs binary when debugging Codex Extension.


set -euo pipefail

REPO_ROOT=$(cd "$(dirname "$0")/.." && pwd)
exec python3 "$REPO_ROOT/scripts/engine_run.py" cargo run --quiet --bin nyacode -- "$@"
