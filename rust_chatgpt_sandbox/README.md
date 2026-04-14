### 📌 Sandbox + Rust Environment Snapshot (Canonical Reset Summary)

---

# 🧠 Execution Model

All commands are executed via:

```text
ChatGPT
   ↓
terminal-server PTY sessions
   ↓
Linux container
   ↓
Kernel
```

* Real Linux
* Real processes
* Restricted network
* CPU-only
* Long builds should run through `localhost:1384`, not through short-lived blocking tool calls

---

# 🦀 Canonical Rust Toolchain

### Base Directory

```bash
/home/oai/rust-sandbox
```

### Binaries

```bash
/home/oai/rust-sandbox/bin/rustc
/home/oai/rust-sandbox/bin/cargo
```

### Version Source

The installed version is whatever nightly bundle is uploaded as:

```bash
/mnt/data/rust-nightly-x86_64-unknown-linux-gnu.tar.gz
```

Do not hardcode the version in automation. Always verify with:

```bash
/home/oai/rust-sandbox/bin/rustc --version
/home/oai/rust-sandbox/bin/cargo --version
```

No rustup.
No musl target unless the tarball includes it.

---

# 📦 Cargo Mirror (Internal Artifactory)

### Cargo home

```bash
/mnt/data/.cargo
```

### Config written by automation

```bash
/mnt/data/.cargo/config.toml
/mnt/data/.cargo/credentials.toml
```

### Registry model

Automation rewrites crates.io to the internal Artifactory cargo proxy and, when the environment exposes Artifactory reader credentials, writes an auth token into `credentials.toml`.

Expected registry form:

```text
sparse+https://packages.applied-caas-gateway1.internal.api.openai.org/artifactory/api/cargo/cargo-public/index/
```

Dependency fetch works through the internal registry when credentials are present.

---

# 📦 Uploaded Artifacts Expected By Automation

```bash
/mnt/data/rust-nightly-x86_64-unknown-linux-gnu.tar.gz
/mnt/data/autonomous_agent_upgrade.tar.gz
/mnt/data/canon-mini-agent.tar.gz
/mnt/data/extraction.py
/mnt/data/README.md
/mnt/data/STARTUP_SCRIPT.md
```

---

# 🤖 What `extraction.py` Does Now

The script automates the reset workflow:

1. Ensures terminal server is running on `127.0.0.1:1384`.
2. Verifies `/healthcheck` returns `{ "status": "ok" }`.
3. Extracts the uploaded Rust nightly tarball.
4. Installs Rust into `/home/oai/rust-sandbox` with:
   * `rustc`
   * `cargo`
   * `rust-std-x86_64-unknown-linux-gnu`
5. Extracts:
   * `autonomous_agent_upgrade.tar.gz` into `$HOME/autonomous_agent_upgrade`
   * `canon-mini-agent.tar.gz` into `/mnt/data/canon-mini-agent-extracted`
6. Generates `/mnt/data/canon-mini-agent-extracted/Cargo.toml` as the minimal parent workspace root required by the extracted path dependency layout.
7. Configures Cargo mirror/auth under `/mnt/data/.cargo`.
8. Runs a Rust smoke test to verify std/unwind/prelude integrity.
9. Starts persistent terminal-server PTY sessions for:
   * `cargo build --workspace`
   * `cargo test --workspace`
10. Prints structured JSON containing:
   * `extraction_status`
   * `rustc_version`
   * `rustc_path`
   * `canon_project_path`
   * `workspace_root_cargo`
   * `build_session.pid`
   * `test_session.pid`

The script does **not** import or require `main.py`.

---

# 🧪 Verified Target State

When automation succeeds, the expected steady state is:

✔ rustc exists and is executable at `/home/oai/rust-sandbox/bin/rustc`
✔ cargo exists and is executable at `/home/oai/rust-sandbox/bin/cargo`
✔ `$HOME/rust-sandbox/bin` exists directly
✔ `canon-mini-agent` is extracted under `/mnt/data/canon-mini-agent-extracted/canon-mini-agent`
✔ `/mnt/data/canon-mini-agent-extracted/Cargo.toml` exists as the parent workspace root
✔ Cargo uses internal Artifactory config from `/mnt/data/.cargo`
✔ terminal server is healthy on `127.0.0.1:1384`
✔ Rust smoke test succeeds before workspace builds begin

---

# ❌ Not Available

✘ No GPU
✘ No CUDA
✘ No OpenCL
✘ No Vulkan runtime
✘ No public GitHub DNS access
✘ rustup absent
✘ apt external network blocked

---

# 🌐 Network Model

```text
Container (172.30.x.x)
   ↓
Azure VNet
   ↓
Artifactory (10.224.x.x)
   ↓
External registries (proxied)
```

Public internet DNS is blocked.
Internal mirrors allow dependency fetch when credentials are configured.

---

# 🛠 apply_patch

Located at:

```bash
/opt/apply_patch/bin/apply_patch
```

Directory:

```bash
/opt/apply_patch
```

This is the container-level patch engine used by Codex-style systems.

---

# 🚀 Canonical Rehydrate Sequence After Restart

Run:

```bash
python /mnt/data/extraction.py
```

Then use:

```bash
export PATH=/home/oai/rust-sandbox/bin:$PATH
export CARGO_HOME=/mnt/data/.cargo
```

Verify:

```bash
rustc --version
cargo --version
ls -l /home/oai/rust-sandbox/bin
ls -l /mnt/data/canon-mini-agent-extracted
```

Read PTY build/test output:

```bash
python - <<'PY'
import urllib.request
BASE = "http://127.0.0.1:1384"
pid = <build_pid_from_extraction_json>
req = urllib.request.Request(f"{BASE}/read/{pid}", data=b"8192", method="POST")
print(urllib.request.urlopen(req).read().decode(errors="ignore"))
PY
```

---

# 🎯 Final State

This sandbox is intended to be:

* A CPU Rust build node
* Internal-registry connected
* Artifact-authenticated
* Deterministic container build environment
* Self-rehydrating from `/mnt/data` uploads
* Able to launch persistent build/test PTY sessions automatically

This README plus `extraction.py` plus `STARTUP_SCRIPT.md` is the canonical reset surface.
