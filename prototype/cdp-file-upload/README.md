# CDP File Upload Prototype

This prototype uploads a local file into a file input element on an already-open browser tab via Chrome DevTools Protocol (CDP).

## Target

Default URL match:

`https://chatgpt.com/g/g-p-69d5aab6319c8191abe0e3298935c109-canon-mini-agent/project?tab=sources`

Default selector:

`input[type=file]`

## Run

```bash
cd /workspace/ai_sandbox/canon-mini-agent/prototype/cdp-file-upload
python3 upload_via_cdp.py --file /absolute/path/to/file
```

Build tar and upload in one command:

```bash
python3 upload_via_cdp.py \
  --build-tar \
  --tar-script /workspace/ai_sandbox/canon-mini-agent/tar.sh \
  --tar-output canon-mini-agent.tar.gz \
  --open-sources-flow \
  --scope sources \
  --force-upload \
  --confirm-settle-sec 3 \
  --confirm-loaded \
  --message "Review the uploaded source archive."
```

Upload, wait until the source row is fully ready, then send a message:

```bash
python3 upload_via_cdp.py \
  --file /absolute/path/to/file \
  --open-sources-flow \
  --scope sources \
  --confirm-loaded \
  --message "Review the uploaded file."
```

## Useful options

```bash
python3 upload_via_cdp.py \
  --file /absolute/path/to/file \
  --match "chatgpt.com/g/g-p-69d5aab6319c8191abe0e3298935c109-canon-mini-agent/project?tab=sources" \
  --selector "input[type=file]" \
  --cdp "http://127.0.0.1:9222" \
  --confirm-loaded \
  --verbose
```

## Notes

- The tab must already be open and reachable at the CDP endpoint.
- Use `--file` for direct upload, or `--build-tar` to run `tar.sh` then upload its output.
- If selector lookup fails, inspect the page and pass a more specific selector.
- By default, it checks whether the filename already appears in Sources UI and skips upload if found.
- `--confirm-loaded` now waits for a ready state (not just filename visibility), using row-level loading indicators/spinners.
- `--confirm-settle-sec` requires ready state to stay stable before confirmation (helps avoid transient false-ready states).
- Use `--force-upload` to replace an existing same-named source: remove existing entry, verify it's gone, then upload and verify ready.
- Use `--message` or `--message-file` to fill the ChatGPT project composer and send after the source is fully ready. Message sending always waits for the ready check, even if `--confirm-loaded` is omitted.
