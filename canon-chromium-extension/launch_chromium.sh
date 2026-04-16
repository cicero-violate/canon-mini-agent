#!/bin/bash
# File: chromium-intel-claude_dev.sh
# GPU Selection := Intel iGPU
export DRI_PRIME=0
export __GLX_VENDOR_LIBRARY_NAME=mesa
unset __NV_PRIME_RENDER_OFFLOAD
unset __VK_LAYER_NV_optimus

export CDPPROXY_HTTP_SSE_TEST=1

# Vulkan ICD := Explicit Intel selection
export VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/intel_icd.x86_64.json:/usr/share/vulkan/icd.d/intel_hasvk_icd.x86_64.json

# Extension dev directory
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
EXT_DIR="$SCRIPT_DIR"
PROFILE_DIR="/workspace/ai_sandbox/canon-mini-agent/chrome-dev-profile"

# Ensure reload signal exists
touch "$EXT_DIR/reload.signal"

# --- Pure Bash watcher ---
watcher() {
  while true; do
    for f in "$EXT_DIR"/*; do
      # check if any file changed
      [ "$f" -nt "$EXT_DIR/reload.signal" ] && touch "$EXT_DIR/reload.signal"
    done
    sleep 1
  done
}
watcher &
WATCHER_PID=$!

# Cleanup on exit
cleanup() {
    echo "Killing watcher and exiting..."
    kill $WATCHER_PID 2>/dev/null
    wait $WATCHER_PID 2>/dev/null
    exit 0
}
trap cleanup SIGINT SIGTERM EXIT

# Chromium Flags
FLAGS=(
    --remote-debugging-port=9221
    --remote-debugging-address=127.0.0.1
    --remote-allow-origins=*
    --load-extension="$EXT_DIR"
    --user-data-dir="$PROFILE_DIR"
)

# Launch Chromium
/usr/bin/chromium "${FLAGS[@]}" "$@"
