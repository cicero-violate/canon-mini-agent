The user will ask you to run this
1. Run the extraction script located at:
   `/mnt/data/extraction.py`

2. After extraction completes, verify:

   * Run `rustc --version`
   * Confirm `$HOME/rust-sandbox/bin` exists
   * Confirm `rustc` inside that directory is executable

3. Locate `main.py` from the extracted contents.

   * Provide its absolute path.
   * Load it into the Python/Jupyter kernel using import execution.

4. Print a structured JSON status report containing:

   * extraction_status
   * rustc_version
   * rustc_path
   * main_py_path
   * main_loaded (true/false)

{ "name": "imagemagick", "path": "/opt/imagemagick", "is_dir": true },
{ "name": "apply_patch", "path": "/opt/apply_patch", "is_dir": true },
{ "name": "python-tool", "path": "/opt/python-tool", "is_dir": true },
{ "name": "pyvenv-python-tool", "path": "/opt/pyvenv-python-tool", "is_dir": true },
{ "name": "pyvenv", "path": "/opt/pyvenv", "is_dir": true },
{ "name": "entrypoint", "path": "/opt/entrypoint", "is_dir": true },
{ "name": "granola-cli", "path": "/opt/granola-cli", "is_dir": true },
{ "name": "novnc", "path": "/opt/novnc", "is_dir": true },
{ "name": "nvm", "path": "/opt/nvm", "is_dir": true },
{ "name": "terminal-server", "path": "/opt/terminal-server", "is_dir": true }

1. Ensure terminal server is running on `localhost:1384` (start `/opt/terminal-server/scripts/start-server.sh` if needed).
2. Verify availability via `GET /healthcheck` → expect `{ "status": "ok" }`.
3. To start a process, `POST /open` with JSON: `{ "cmd": [...], "env": {}, "cwd": "/abs/path", "user": "" }`.
4. Store returned PID; this identifies the PTY session.
5. To read output, `POST /read/{pid}` with body = byte count (e.g., `4096`).
6. To send input, `POST /write/{pid}` with raw bytes payload.
7. Loop: read → write → read to simulate interactive terminal behavior.
8. To terminate session, `POST /kill/{pid}`.
9. Set `Authorization` header if `BEARER_TOKEN` is configured.
10. Use this model to run long builds (e.g., `cargo build`) without blocking Python execution.

import json
import time
import urllib.request

BASE = "http://127.0.0.1:1384"
CANON_DIR = "/home/oai/canon"
RUST_BIN = "/home/oai/rust-sandbox/bin"
CARGO_HOME = "/mnt/data/.cargo"

def post_json(path, data):
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(data).encode(),
        headers={"Content-Type": "application/json"},
        method="POST"
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.read()

def post_raw(path, data_bytes):
    req = urllib.request.Request(
        BASE + path,
        data=data_bytes,
        method="POST"
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.read()

env = {
    "PATH": f"{RUST_BIN}:/usr/bin:/bin",
    "CARGO_HOME": CARGO_HOME
}

result = {}

# Open cargo build
build_pid = int(post_json("/open", {
    "cmd": [f"{RUST_BIN}/cargo", "build", "--workspace"],
    "env": env,
    "cwd": CANON_DIR,
    "user": ""
}).decode())

# Open cargo test
test_pid = int(post_json("/open", {
    "cmd": [f"{RUST_BIN}/cargo", "test", "--workspace"],
    "env": env,
    "cwd": CANON_DIR,
    "user": ""
}).decode())

# Open interactive bash using absolute path
bash_pid = int(post_json("/open", {
    "cmd": ["/bin/bash"],
    "env": env,
    "cwd": CANON_DIR,
    "user": ""
}).decode())

time.sleep(2)

build_out = post_raw(f"/read/{build_pid}", b"4096").decode(errors="ignore")
test_out = post_raw(f"/read/{test_pid}", b"4096").decode(errors="ignore")

# Interact with bash
post_raw(f"/write/{bash_pid}", b"echo MULTI_TERMINAL_OK\n")
time.sleep(1)
bash_out = post_raw(f"/read/{bash_pid}", b"2048").decode(errors="ignore")

result["build_pid"] = build_pid
result["test_pid"] = test_pid
result["bash_pid"] = bash_pid
result["build_output_sample"] = build_out[:800]
result["test_output_sample"] = test_out[:800]
result["bash_output_sample"] = bash_out[:800]

print(json.dumps(result, indent=2))

Extract also canon-mini-agent tar
