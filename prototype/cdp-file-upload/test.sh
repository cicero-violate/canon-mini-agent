## TESTEST

python3 /workspace/ai_sandbox/canon-mini-agent/prototype/cdp-file-upload/upload_via_cdp.py \
  --file test.sh \
  --open-sources-flow \
  --scope sources \
  --confirm-loaded

python3 /workspace/ai_sandbox/canon-mini-agent/prototype/cdp-file-upload/upload_via_cdp.py \
  --file test.sh \
  --open-sources-flow \
  --scope sources \
  --confirm-loaded

python3 /workspace/ai_sandbox/canon-mini-agent/prototype/cdp-file-upload/upload_via_cdp.py \
  --file test.sh \
  --open-sources-flow \
  --scope sources \
  --force-upload \
  --confirm-loaded

python3 /workspace/ai_sandbox/canon-mini-agent/prototype/cdp-file-upload/upload_via_cdp.py \
  --build-tar \
  --tar-script /workspace/ai_sandbox/canon-mini-agent/tar.sh \
  --tar-output canon-mini-agent.tar.gz \
  --open-sources-flow \
  --scope sources \
  --force-upload \
  --confirm-loaded

python3 /workspace/ai_sandbox/canon-mini-agent/prototype/cdp-file-upload/upload_via_cdp.py \
  --build-tar \
  --tar-script /workspace/ai_sandbox/canon-mini-agent/tar.sh \
  --tar-output canon-mini-agent.tar.gz \
  --open-sources-flow \
  --scope sources \
  --force-upload \
  --confirm-loaded \
  --confirm-timeout-sec 120 \
  --confirm-settle-sec 3

