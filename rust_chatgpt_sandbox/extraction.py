#!/usr/bin/env python3
import base64
import json
import os
import shutil
import time
import urllib.request
from pathlib import Path

BASE = "http://127.0.0.1:1384"
MNT = Path("/mnt/data")
HOME = Path.home()

RUST_TAR = MNT / "rust-nightly-x86_64-unknown-linux-gnu.tar.gz"
AUTONOMOUS_TAR = MNT / "autonomous_agent_upgrade.tar.gz"
CANON_TAR = MNT / "canon-mini-agent.tar.gz"

RUST_SANDBOX = HOME / "rust-sandbox"
RUST_BIN = RUST_SANDBOX / "bin"
RUSTC_PATH = RUST_BIN / "rustc"
CARGO_PATH = RUST_BIN / "cargo"

CARGO_HOME = HOME / ".cargo"
CARGO_CONFIG = CARGO_HOME / "config.toml"
CARGO_CREDENTIALS = CARGO_HOME / "credentials.toml"

AUTONOMOUS_DIR = HOME / "autonomous_agent_upgrade"
CANON_ROOT = HOME / "canon-mini-agent-extracted"
CANON_DIR = CANON_ROOT / "canon-mini-agent"
WORKSPACE_ROOT_CARGO = CANON_ROOT / "Cargo.toml"
BUILD_LOG = HOME / "build.log"
TEST_LOG = HOME / "test.log"

DEFAULT_REGISTRY_URL = (
    "sparse+https://packages.applied-caas-gateway1.internal.api.openai.org/"
    "artifactory/api/cargo/cargo-public/index/"
)


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


def shell_quote(value):
    return str(value).replace("'", "'\"'\"'")


def ensure_terminal_server():
    health = {"ok": False, "response": None, "error": None}
    try:
        with urllib.request.urlopen(BASE + "/healthcheck", timeout=5) as r:
            body = r.read().decode(errors="ignore")
            normalized = body.replace(" ", "")
            health["ok"] = r.status == 200 and '"status":"ok"' in normalized
            health["response"] = body
            if health["ok"]:
                return health
    except Exception as e:
        health["error"] = str(e)

    start_script = Path("/opt/terminal-server/scripts/start-server.sh")
    if start_script.exists():
        os.system(f"bash {start_script} >/tmp/terminal-server.log 2>&1 &")
        time.sleep(2)

    try:
        with urllib.request.urlopen(BASE + "/healthcheck", timeout=5) as r:
            body = r.read().decode(errors="ignore")
            normalized = body.replace(" ", "")
            health["ok"] = r.status == 200 and '"status":"ok"' in normalized
            health["response"] = body
            health["error"] = None
    except Exception as e:
        health["error"] = str(e)
    return health


def terminal_open(cmd, cwd, env=None, user=""):
    return int(
        post_json(
            "/open",
            {
                "cmd": cmd,
                "env": env or {},
                "cwd": str(cwd),
                "user": user,
            },
        ).decode()
    )


def terminal_read(pid, num_bytes=32768):
    return post_raw(f"/read/{pid}", str(num_bytes).encode()).decode(errors="ignore")


def terminal_write(pid, data):
    payload = data.encode() if isinstance(data, str) else data
    return post_raw(f"/write/{pid}", payload)


def run_in_shell(pid, cmd, marker, timeout=1800):
    terminal_write(pid, cmd + "\n")
    output = ""
    start = time.time()
    while time.time() - start < timeout:
        chunk = terminal_read(pid)
        output += chunk
        if marker in output:
            return True, output
        time.sleep(1)
    return False, output


def env_get_any(names):
    for name in names:
        value = os.environ.get(name)
        if value:
            return value
    return None


def configure_cargo():
    CARGO_HOME.mkdir(parents=True, exist_ok=True)

    username = env_get_any([
        "CAAS_ARTIFACTORY_READER_USERNAME",
        "ARTIFACTORY_USERNAME",
        "CARGO_REGISTRIES_ARTIFACTORY_USERNAME",
    ])
    password = env_get_any([
        "CAAS_ARTIFACTORY_READER_PASSWORD",
        "ARTIFACTORY_PASSWORD",
        "CARGO_REGISTRIES_ARTIFACTORY_PASSWORD",
    ])
    registry_url = env_get_any([
        "CAAS_CARGO_REGISTRY_URL",
        "CARGO_REGISTRIES_ARTIFACTORY_INDEX",
    ]) or DEFAULT_REGISTRY_URL

    config = f"""
[source.crates-io]
replace-with = "artifactory"

[registries.artifactory]
index = "{registry_url}"

[net]
git-fetch-with-cli = true
retry = 2
""".strip() + "\n"
    CARGO_CONFIG.write_text(config)

    credentials_written = False
    if username and password:
        token = base64.b64encode(f"{username}:{password}".encode()).decode()
        credentials = "[registries.artifactory]\n" f'token = "Basic {token}"\n'
        CARGO_CREDENTIALS.write_text(credentials)
        credentials_written = True

    return {
        "config_path": str(CARGO_CONFIG),
        "credentials_path": str(CARGO_CREDENTIALS),
        "credentials_written": credentials_written,
        "registry_url": registry_url,
    }


def reset_dir(path: Path):
    if path.exists() or path.is_symlink():
        if path.is_symlink() or path.is_file():
            path.unlink()
        else:
            shutil.rmtree(path)


def remove_if_exists(path: Path):
    if path.exists() or path.is_symlink():
        path.unlink()


def extract_tar(tar_path: Path, dest: Path, clean=False):
    if not tar_path.exists():
        return {"ok": False, "error": f"missing tarball: {tar_path}"}
    if clean:
        reset_dir(dest)
    dest.mkdir(parents=True, exist_ok=True)
    shutil.unpack_archive(str(tar_path), str(dest))
    return {"ok": True, "dest": str(dest)}


def ensure_canon_workspace_root():
    CANON_ROOT.mkdir(parents=True, exist_ok=True)
    WORKSPACE_ROOT_CARGO.write_text(
        "[workspace]\n"
        'members = ["canon-mini-agent"]\n'
        'resolver = "2"\n\n'
        "[workspace.dependencies]\n"
        'anyhow = "1"\n'
        'thiserror = "1"\n'
    )
    return {"ok": True, "path": str(WORKSPACE_ROOT_CARGO)}


def install_rust(shell_pid):
    reset_dir(RUST_SANDBOX)
    RUST_SANDBOX.mkdir(parents=True, exist_ok=True)

    temp_root = HOME / "rust-nightly-install"
    reset_dir(temp_root)
    temp_root.mkdir(parents=True, exist_ok=True)

    ok_extract, extract_out = run_in_shell(
        shell_pid,
        f"rm -rf {temp_root}/* && tar -xzf {RUST_TAR} -C {temp_root} && echo __EXTRACT_DONE__",
        "__EXTRACT_DONE__",
        timeout=1800,
    )
    if not ok_extract:
        return {"ok": False, "stage": "extract", "output": extract_out[-4000:]}

    install_output = {}
    for component in ["rustc", "cargo", "rust-std-x86_64-unknown-linux-gnu"]:
        install_cmd = (
            f"cd {temp_root}/*nightly* && "
            f"./install.sh --prefix={RUST_SANDBOX} --disable-ldconfig "
            f"--components={component} && echo __INSTALL_DONE__"
        )
        ok_install, component_out = run_in_shell(
            shell_pid, install_cmd, "__INSTALL_DONE__", timeout=1800
        )
        install_output[component] = component_out[-1000:]
        if not ok_install:
            return {
                "ok": False,
                "stage": f"install:{component}",
                "output": component_out[-4000:],
            }

    return {"ok": True, "extract_output": extract_out[-1000:], "install_output": install_output}


def binary_version(path: Path):
    if not path.exists():
        return {"exists": False, "executable": False, "version": None}
    executable = os.access(path, os.X_OK)
    version = None
    if executable:
        try:
            version = os.popen(f"{path} --version").read().strip()
        except Exception:
            version = None
    return {"exists": True, "executable": executable, "version": version}


def rust_env():
    return {
        "PATH": f"{RUST_BIN}:/usr/bin:/bin",
        "CARGO_HOME": str(CARGO_HOME),
    }


def start_background_job(command, cwd, log_path: Path):
    remove_if_exists(log_path)
    bash_command = (
        f"cd '{shell_quote(cwd)}' && "
        f"export PATH='{shell_quote(RUST_BIN)}':$PATH && "
        f"export CARGO_HOME='{shell_quote(CARGO_HOME)}' && "
        f"{command} 2>&1 | tee '{shell_quote(log_path)}'"
    )
    pid = terminal_open(
        cmd=["/bin/bash", "-lc", bash_command],
        cwd=cwd,
        env=rust_env(),
    )
    time.sleep(2)
    sample = terminal_read(pid, 4096)
    return {"pid": pid, "output_sample": sample[:2000], "log_path": str(log_path)}


def smoke_test(shell_pid):
    test_src = HOME / "rust_toolchain_smoke_test.rs"
    test_bin = HOME / "rust_toolchain_smoke_test"
    test_src.write_text('fn main() { println!("ok"); }\n')
    cmd = (
        f"export PATH={RUST_BIN}:$PATH; "
        f"export CARGO_HOME={CARGO_HOME}; "
        f"cd {HOME} && {RUSTC_PATH} {test_src.name} -o {test_bin.name} && ./{test_bin.name} && echo __SMOKE_DONE__"
    )
    ok, out = run_in_shell(shell_pid, cmd, "__SMOKE_DONE__", timeout=300)
    return {"ok": ok, "output": out[-2000:]}


def main():
    report = {
        "terminal_server": ensure_terminal_server(),
        "extraction_status": False,
        "rustc_version": None,
        "rustc_path": str(RUSTC_PATH),
        "cargo_path": str(CARGO_PATH),
        "home_rust_sandbox_bin": str(RUST_BIN),
        "canon_project_path": str(CANON_DIR),
        "workspace_root_cargo": str(WORKSPACE_ROOT_CARGO),
        "build_log": str(BUILD_LOG),
        "test_log": str(TEST_LOG),
    }

    if not report["terminal_server"].get("ok"):
        print(json.dumps(report, indent=2))
        return

    shell_pid = terminal_open(cmd=["/bin/bash"], cwd=HOME, env={})
    report["pty_pid"] = shell_pid

    report["rust_install"] = install_rust(shell_pid)
    report["cargo_config"] = configure_cargo()
    report["autonomous_extract"] = extract_tar(AUTONOMOUS_TAR, AUTONOMOUS_DIR, clean=True)
    report["canon_extract"] = extract_tar(CANON_TAR, CANON_ROOT, clean=True)
    report["workspace_root"] = ensure_canon_workspace_root()

    rustc_info = binary_version(RUSTC_PATH)
    cargo_info = binary_version(CARGO_PATH)
    report["rustc_info"] = rustc_info
    report["cargo_info"] = cargo_info
    report["rustc_version"] = rustc_info["version"]
    report["cargo_version"] = cargo_info["version"]

    report["smoke_test"] = smoke_test(shell_pid)

    report["build_session"] = start_background_job(
        "cargo build --workspace",
        CANON_ROOT,
        BUILD_LOG,
    )
    report["test_session"] = start_background_job(
        "cargo test --workspace",
        CANON_ROOT,
        TEST_LOG,
    )

    report["extraction_status"] = bool(
        report["rust_install"].get("ok")
        and report["autonomous_extract"].get("ok")
        and report["canon_extract"].get("ok")
        and report["workspace_root"].get("ok")
        and rustc_info["executable"]
        and cargo_info["executable"]
        and report["smoke_test"].get("ok")
    )

    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
