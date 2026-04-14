### 📌 Sandbox + Rust Environment Snapshot (Canonical Reset Summary)

---

# 🧠 Execution Model

All commands are executed via:

```text
ChatGPT
   ↓
python_user_visible tool
   ↓
subprocess
   ↓
Linux container
   ↓
Kernel
```

* Real Linux
* Real processes
* Restricted network
* 60s timeout per synchronous Python call
* Background processes allowed
* CPU-only

---

# 🦀 Canonical Rust Toolchain

### Base Directory

```bash
/mnt/data/rust-sandbox
```

### Binaries

```bash
/mnt/data/rust-sandbox/bin/rustc
/mnt/data/rust-sandbox/bin/cargo
```

### Compatibility Link

```bash
$HOME/rust-sandbox -> /mnt/data/rust-sandbox
```

### Version Source

The installed version is whatever nightly bundle is uploaded as:

```bash
/mnt/data/rust-nightly-x86_64-unknown-linux-gnu.tar.gz
```

Do not hardcode the version in automation. Always verify with:

```bash
/mnt/data/rust-sandbox/bin/rustc --version
/mnt/data/rust-sandbox/bin/cargo --version
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
```

---

# 🤖 What `extraction.py` Does Now

The script automates the full reset workflow:

1. Ensures terminal server is running on `127.0.0.1:1384`.
2. Verifies `/healthcheck` returns `{ "status": "ok" }`.
3. Extracts the uploaded Rust nightly tarball.
4. Installs Rust into `/mnt/data/rust-sandbox` with:
   * `rustc`
   * `cargo`
   * `rust-std-x86_64-unknown-linux-gnu`
5. Creates compatibility symlink:
   * `$HOME/rust-sandbox -> /mnt/data/rust-sandbox`
6. Extracts:
   * `autonomous_agent_upgrade.tar.gz` into `$HOME`
   * `canon-mini-agent.tar.gz` into `/mnt/data/canon-mini-agent-extracted`
7. Configures Cargo mirror/auth under `/mnt/data/.cargo`.
8. Imports `$HOME/autonomous_agent_upgrade/main.py`.
9. Runs an optional `cargo build --workspace` probe in:

```bash
/mnt/data/canon-mini-agent-extracted/canon-mini-agent
```

10. Prints structured JSON containing at minimum:
    * `extraction_status`
    * `rustc_version`
    * `rustc_path`
    * `main_py_path`
    * `main_loaded`

It also includes extra diagnostics for terminal server, cargo config, extraction paths, and build probe results.

---

# 🧪 Verified Target State

When automation succeeds, the expected steady state is:

✔ rustc exists and is executable at `/mnt/data/rust-sandbox/bin/rustc`
✔ cargo exists and is executable at `/mnt/data/rust-sandbox/bin/cargo`
✔ `$HOME/rust-sandbox/bin` is available through the symlink
✔ `autonomous_agent_upgrade/main.py` is extracted under `$HOME`
✔ `canon-mini-agent` is extracted under `/mnt/data/canon-mini-agent-extracted/canon-mini-agent`
✔ Cargo uses internal Artifactory config from `/mnt/data/.cargo`
✔ terminal server is healthy on `127.0.0.1:1384`

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
export PATH=/mnt/data/rust-sandbox/bin:$PATH
export CARGO_HOME=/mnt/data/.cargo
```

Verify:

```bash
rustc --version
cargo --version
ls -l /mnt/data/rust-sandbox/bin
```

Build:

```bash
cd /mnt/data/canon-mini-agent-extracted/canon-mini-agent
cargo build --workspace
```

---

# 🎯 Final State

This sandbox is intended to be:

* A CPU Rust build node
* Internal-registry connected
* Artifact-authenticated
* Deterministic container build environment
* Self-rehydrating from `/mnt/data` uploads

This README plus `extraction.py` is the canonical reset surface.
