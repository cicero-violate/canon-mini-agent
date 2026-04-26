git add .
git commit -m "uploading to chatgpt projects"
git push origin main

python3 /workspace/ai_sandbox/canon-mini-agent/prototype/cdp-file-upload/upload_via_cdp.py \
  --build-tar \
  --tar-script /workspace/ai_sandbox/canon-mini-agent/tar.sh \
  --tar-output canon-mini-agent.tar.gz \
  --open-target-if-missing \
  --target-url "https://chatgpt.com/g/g-p-69d5aab6319c8191abe0e3298935c109-canon-mini-agent/project?tab=sources" \
  --target-wait-timeout-sec 45 \
  --open-sources-flow \
  --scope sources \
  --force-upload \
  --confirm-loaded \
  --confirm-timeout-sec 120 \
  --confirm-settle-sec 3
