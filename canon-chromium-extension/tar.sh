#!/bin/bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$SCRIPT_DIR"

tar -czvf canon-chromium-extension.tar.gz $(find . -type f -name "*.js")
