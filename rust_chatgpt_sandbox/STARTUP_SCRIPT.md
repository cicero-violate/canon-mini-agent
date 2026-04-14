The user will ask you to run this.

0. Read `/mnt/data/README.md`.

1. Run the extraction script located at:
   `/mnt/data/extraction.py`

2. After extraction completes, verify:

   * Run `rustc --version`
   * Confirm `$HOME/rust-sandbox/bin` exists
   * Confirm `rustc` inside that directory is executable
   * Confirm `$HOME/canon-mini-agent-extracted/Cargo.toml` exists
   * Confirm `$HOME/canon-mini-agent-extracted/canon-mini-agent` exists

3. Print a structured JSON status report containing:

   * `extraction_status`
   * `rustc_version`
   * `rustc_path`
   * `canon_project_path`
   * `workspace_root_cargo`
   * `build_session.pid`
   * `test_session.pid`

4. Use terminal server only for long-running commands.

5. All mutable state must live under `/home/oai/*`.

   * Use `/mnt/data/*` only as the uploaded artifact source.
   * Extract the writable canon workspace into:
     * `/home/oai/canon-mini-agent-extracted`
   * Use Cargo state under:
     * `/home/oai/.cargo`
   * Persist background build/test logs under:
     * `/home/oai/build.log`
     * `/home/oai/test.log`

Terminal-server contract:
1. Ensure terminal server is running on `localhost:1384` (start `/opt/terminal-server/scripts/start-server.sh` if needed).
2. Verify availability via `GET /healthcheck` → expect `{ "status": "ok" }`.
3. To start a process, `POST /open` with JSON: `{ "cmd": [...], "env": {}, "cwd": "/abs/path", "user": "" }`.
4. Store returned PID; this identifies the PTY session.
5. To read output, `POST /read/{pid}` with body = byte count (for example `4096`).
6. To send input, `POST /write/{pid}` with raw bytes payload.
7. Loop: read → write → read to simulate interactive terminal behavior.
8. To terminate session, `POST /kill/{pid}`.
9. Set `Authorization` header if `BEARER_TOKEN` is configured.
10. Use this model to run long builds in the background without blocking short-lived execution.
11. For background build/test, prefer `bash -lc` with `tee` so output is both streamed and persisted.

Reference snippet:

```python
import json
import time
import urllib.request
from pathlib import Path

BASE = "http://127.0.0.1:1384"
HOME = Path.home()
CANON_DIR = HOME / "canon-mini-agent-extracted"
RUST_BIN = HOME / "rust-sandbox" / "bin"
CARGO_HOME = HOME / ".cargo"
BUILD_LOG = HOME / "build.log"
TEST_LOG = HOME / "test.log"


def post_json(path, data):
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(data).encode(),
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.read()


def post_raw(path, data_bytes):
    req = urllib.request.Request(BASE + path, data=data_bytes, method="POST")
    with urllib.request.urlopen(req, timeout=10) as r:
        return r.read()


env = {
    "PATH": f"{RUST_BIN}:/usr/bin:/bin",
    "CARGO_HOME": str(CARGO_HOME),
}

build_pid = int(post_json("/open", {
    "cmd": [
        "/bin/bash",
        "-lc",
        f"cd {CANON_DIR} && cargo build --workspace 2>&1 | tee {BUILD_LOG}",
    ],
    "env": env,
    "cwd": str(CANON_DIR),
    "user": "",
}).decode())

test_pid = int(post_json("/open", {
    "cmd": [
        "/bin/bash",
        "-lc",
        f"cd {CANON_DIR} && cargo test --workspace 2>&1 | tee {TEST_LOG}",
    ],
    "env": env,
    "cwd": str(CANON_DIR),
    "user": "",
}).decode())

time.sleep(2)

build_out = post_raw(f"/read/{build_pid}", b"4096").decode(errors="ignore")
test_out = post_raw(f"/read/{test_pid}", b"4096").decode(errors="ignore")

result = {
    "build_pid": build_pid,
    "test_pid": test_pid,
    "build_output_sample": build_out[:800],
    "test_output_sample": test_out[:800],
    "canon_workspace_root": str(CANON_DIR / "Cargo.toml"),
    "canon_project_path": str(CANON_DIR / "canon-mini-agent"),
    "build_log": str(BUILD_LOG),
    "test_log": str(TEST_LOG),
}

print(json.dumps(result, indent=2))
```

Do not import or require `main.py`.
Extraction must also generate the parent workspace root `Cargo.toml` so the extracted path dependency layout builds without manual fixes.
