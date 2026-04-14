#!/usr/bin/env python3
import base64
import importlib.util
import json
import os
import shutil
import sys
import time
import urllib.request
from pathlib import Path

BASE = "http://127.0.0.1:1384"
MNT = Path("/mnt/data")
HOME = Path.home()

RUST_TAR = MNT / "rust-nightly-x86_64-unknown-linux-gnu.tar.gz"
AUTONOMOUS_TAR = MNT / "autonomous_agent_upgrade.tar.gz"
CANON_TAR = MNT / "canon-mini-agent.tar.gz"
README_PATH = MNT / "README.md"

RUST_SANDBOX = MNT / "rust-sandbox"
RUST_BIN = RUST_SANDBOX / "bin"
RUSTC_PATH = RUST_BIN / "rustc"
CARGO_PATH = RUST_BIN / "cargo"
HOME_RUST_SANDBOX = HOME / "rust-sandbox"

AUTONOMOUS_DIR = HOME / "autonomous_agent_upgrade"
CANON_DIR = MNT / "canon-mini-agent-extracted" / "canon-mini-agent"
AGENT_MAIN = AUTONOMOUS_DIR / "main.py"

CARGO_HOME = MNT / ".cargo"
CARGO_CONFIG = CARGO_HOME / "config.toml"
CARGO_CREDENTIALS = CARGO_HOME / "credentials.toml"

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


def ensure_terminal_server():
    health = {"ok": False, "response": None, "error": None}
    try:
        with urllib.request.urlopen(BASE + "/healthcheck", timeout=5) as r:
            body = r.read().decode(errors="ignore")
            health["ok"] = r.status == 200 and '"status": "ok"' in body
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
            health["ok"] = r.status == 200 and '"status": "ok"' in body
            health["response"] = body
            health["error"] = None
    except Exception as e:
        health["error"] = str(e)
    return health


def start_bash():
    return int(
        post_json(
            "/open",
            {
                "cmd": ["/bin/bash"],
                "env": {},
                "cwd": str(HOME),
                "user": "",
            },
        ).decode()
    )


def run_and_wait(pid, cmd, marker, timeout=1800):
    post_raw(f"/write/{pid}", (cmd + "\n").encode())
    output = ""
    start = time.time()
    while time.time() - start < timeout:
        chunk = post_raw(f"/read/{pid}", b"32768").decode(errors="ignore")
        output += chunk
        if marker and marker in output:
            return True, output
        if not marker:
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
        credentials = (
            "[registries.artifactory]\n"
            f'token = "Basic {token}"\n'
        )
        CARGO_CREDENTIALS.write_text(credentials)
        credentials_written = True

    return {
        "config_path": str(CARGO_CONFIG),
        "credentials_path": str(CARGO_CREDENTIALS),
        "credentials_written": credentials_written,
        "registry_url": registry_url,
    }


def extract_tar(tar_path: Path, dest: Path):
    if not tar_path.exists():
        return {"ok": False, "error": f"missing tarball: {tar_path}"}
    dest.mkdir(parents=True, exist_ok=True)
    shutil.unpack_archive(str(tar_path), str(dest))
    return {"ok": True, "dest": str(dest)}


def install_rust(pid):
    sandbox_root = RUST_SANDBOX
    if sandbox_root.exists():
        shutil.rmtree(sandbox_root)
    sandbox_root.mkdir(parents=True, exist_ok=True)

    temp_root = MNT / "rust-nightly-install"
    if temp_root.exists():
        shutil.rmtree(temp_root)
    temp_root.mkdir(parents=True, exist_ok=True)

    ok_extract, extract_out = run_and_wait(
        pid,
        f"rm -rf {temp_root}/* && tar -xzf {RUST_TAR} -C {temp_root} && echo __EXTRACT_DONE__",
        "__EXTRACT_DONE__",
    )
    if not ok_extract:
        return {"ok": False, "stage": "extract", "output": extract_out[-4000:]}

    install_cmd = (
        f"cd {temp_root}/*nightly* && "
        f"./install.sh --prefix={sandbox_root} --disable-ldconfig "
        f"--components=rustc,cargo,rust-std-x86_64-unknown-linux-gnu && "
        f"echo __INSTALL_DONE__"
    )
    ok_install, install_out = run_and_wait(pid, install_cmd, "__INSTALL_DONE__", timeout=1800)
    if not ok_install:
        return {"ok": False, "stage": "install", "output": install_out[-4000:]}

    HOME_RUST_SANDBOX.parent.mkdir(parents=True, exist_ok=True)
    if HOME_RUST_SANDBOX.exists() or HOME_RUST_SANDBOX.is_symlink():
        if HOME_RUST_SANDBOX.is_symlink() or HOME_RUST_SANDBOX.is_file():
            HOME_RUST_SANDBOX.unlink()
        else:
            shutil.rmtree(HOME_RUST_SANDBOX)
    HOME_RUST_SANDBOX.symlink_to(RUST_SANDBOX, target_is_directory=True)

    return {"ok": True, "extract_output": extract_out[-1000:], "install_output": install_out[-1000:]}


def import_main(path: Path):
    result = {"loaded": False, "error": None}
    if not path.exists():
        result["error"] = f"main.py not found: {path}"
        return result
    try:
        if str(path.parent) not in sys.path:
            sys.path.insert(0, str(path.parent))
        spec = importlib.util.spec_from_file_location("autonomous_agent_main", str(path))
        if spec is None or spec.loader is None:
            result["error"] = "failed to create import spec"
            return result
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        result["loaded"] = True
        return result
    except Exception as e:
        result["error"] = str(e)
        return result


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


def maybe_build_probe(pid):
    if not CANON_DIR.exists() or not CARGO_PATH.exists():
        return {"ran": False, "reason": "workspace or cargo missing"}
    env = f"export PATH={RUST_BIN}:$PATH; export CARGO_HOME={CARGO_HOME};"
    cmd = f"cd {CANON_DIR} && {env} cargo build --workspace && echo __BUILD_DONE__"
    ok, out = run_and_wait(pid, cmd, "__BUILD_DONE__", timeout=3600)
    return {
        "ran": True,
        "success": ok,
        "output_sample": out[-4000:],
    }


def main():
    report = {
        "terminal_server": ensure_terminal_server(),
        "extraction_status": False,
        "rustc_version": None,
        "rustc_path": str(RUSTC_PATH),
        "cargo_path": str(CARGO_PATH),
        "main_py_path": str(AGENT_MAIN),
        "main_loaded": False,
    }

    pid = start_bash()
    report["pty_pid"] = pid

    rust_install = install_rust(pid)
    report["rust_install"] = rust_install

    report["autonomous_extract"] = extract_tar(AUTONOMOUS_TAR, HOME)
    report["canon_extract"] = extract_tar(CANON_TAR, MNT / "canon-mini-agent-extracted")
    report["cargo_config"] = configure_cargo()

    rustc_info = binary_version(RUSTC_PATH)
    cargo_info = binary_version(CARGO_PATH)
    report["rustc_info"] = rustc_info
    report["cargo_info"] = cargo_info
    report["rustc_version"] = rustc_info["version"]
    report["cargo_version"] = cargo_info["version"]

    main_import = import_main(AGENT_MAIN)
    report["main_import"] = main_import
    report["main_loaded"] = main_import["loaded"]

    report["home_rust_sandbox"] = str(HOME_RUST_SANDBOX)
    report["canon_project_path"] = str(CANON_DIR)
    report["build_probe"] = maybe_build_probe(pid)

    report["extraction_status"] = bool(
        rust_install.get("ok") and
        report["autonomous_extract"].get("ok") and
        report["canon_extract"].get("ok") and
        rustc_info["executable"] and
        cargo_info["executable"]
    )

    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
