"""Opt-in Darwin worker regression. Requires the independently sealed test deployment."""

import argparse
import hashlib
import json
import os
import shutil
import signal
import subprocess
import sys
import time
from pathlib import Path

from prepare import prepare

SDK = "7fe48a56a3091e05e99a024e3f5f524732a82ed6"
PACKET = "2b4a13bd0769ff4263647d377afe4d66f3d55cedc112d550fe6fe1f5d26509a1"
ALTERNATE = "1d4619d1fc37d4af24a0f339aa5f7d7300bc357a6c5c3373587b9730c2901e42"
HERE = Path(__file__).resolve().parent
HOST = HERE.parents[1]


def verified(path, digest):
    data = path.read_bytes()
    if hashlib.sha256(data).hexdigest() != digest:
        raise ValueError(f"unexpected bytes at {path}")
    return data


def package(directory, sdk, crates, extras, *, worker):
    lines = [
        "[workspace]",
        "[package]",
        f'name = "{directory.name}-plugin-test"',
        'version = "0.0.0"',
        'edition = "2024"',
        "publish = false",
    ]
    if worker:
        shutil.copyfile(
            HOST / "crates/lean-host-mcp-worker/build.rs", directory / "build.rs"
        )
    lines += [
        "[dependencies]",
        *[
            f"{crate} = {{ path = {json.dumps(str(sdk / 'crates' / crate))} }}"
            for crate in crates
        ],
        *extras,
    ]
    (directory / "Cargo.toml").write_text("\n".join(lines) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for opt in ("sdk", "deployment", "alternate", "helper", "scratch", "target"):
        parser.add_argument("--" + opt, required=True, type=lambda s: Path(s).resolve())
    parser.add_argument("--helper-sha256", required=True)
    a = parser.parse_args()
    git = ["git", "-C", str(a.sdk)]
    if (
        sys.platform != "darwin"
        or subprocess.check_output(git + ["rev-parse", "HEAD"]).decode().strip() != SDK
        or subprocess.check_output(
            git + ["status", "--porcelain", "--untracked-files=all"]
        )
    ):
        raise RuntimeError("requires Darwin and the clean pinned SDK checkout")
    verified(a.helper, a.helper_sha256)
    rec = json.loads(verified(a.deployment, PACKET))
    replacement = verified(a.alternate, ALTERNATE)
    if not a.target.is_dir() or shutil.disk_usage(a.target).free < 26 * 2**30:
        raise RuntimeError(
            "requires the existing Cargo target and space above the reserve"
        )
    a.scratch.mkdir(mode=0o700)
    for role in ("worker", "controller"):
        shutil.copytree(HERE / role, a.scratch / role)
    (a.scratch / "alternate").mkdir()
    (a.scratch / "alternate/QualifiedPlugin.olean").write_bytes(replacement)
    (a.scratch / "owners").mkdir(mode=0o700)
    prepare(a.scratch, rec)
    package(
        a.scratch / "worker",
        a.sdk,
        ["lean-rs", "lean-rs-host", "lean-rs-worker-child", "lean-toolchain"],
        [
            'serde = {version="1", features=["derive"]}',
            'serde_json = "1"',
            'sha2 = "0.11"',
            'libc = "0.2"',
            'mach2 = "0.6"',
        ],
        worker=True,
    )
    package(
        a.scratch / "controller",
        a.sdk,
        ["lean-toolchain", "lean-rs-worker-protocol", "lean-rs-worker-parent"],
        ['serde_json = "1"'],
        worker=False,
    )
    release = Path(rec["sysroot"])
    build = dict(
        os.environ,
        LEAN_SYSROOT=str(release),
        CARGO_TARGET_DIR=str(a.target),
        CARGO_INCREMENTAL="0",
        HOST_PLUGIN_TRIAL=str(a.scratch),
        HOST_PLUGIN_WORKER_ENTRY=str(HOST / "crates/lean-host-mcp-worker/src/main.rs"),
    )
    for role in ("worker", "controller"):
        with subprocess.Popen(
            ["cargo", "build", "--offline", "-j1"],
            cwd=a.scratch / role,
            env=build,
            start_new_session=True,
        ) as proc:
            while proc.poll() is None:
                if shutil.disk_usage(a.target).free < int(25.25 * 2**30):
                    os.killpg(proc.pid, signal.SIGTERM)
                    proc.wait()
                    raise RuntimeError("stopped build at free-space reserve")
                time.sleep(0.5)
            if proc.returncode:
                raise subprocess.CalledProcessError(proc.returncode, proc.args)

    env = {
        "HOME": os.environ["HOME"],
        "PATH": f"{release}/bin:/usr/bin:/bin",
        "LEAN_SYSROOT": str(release),
        "LEAN_NUM_THREADS": "1",
        "LEAN_HOST_MCP_WRITER_EXECUTABLE": str(a.helper),
        "LEAN_RS_WORKER_OWNERS_DIRECTORY": str(a.scratch / "owners"),
        "HOST_PLUGIN_WORKER": str(a.target / "debug/worker-plugin-test"),
        "LEAN_RS_TEST_PLUGIN_MARKER": str(a.scratch / "worker/marker"),
    }
    for phase in (
        "override",
        "positive",
        "writer-replace",
        "refused",
        "writer-restore",
        "positive",
    ):
        marker = a.scratch / "worker/marker"
        marker.unlink(missing_ok=True)
        invocation = (
            dict(env, DYLD_LIBRARY_PATH=str(a.scratch / "alternate"))
            if phase == "override"
            else env
        )
        for mode in (phase, "census"):
            subprocess.run(
                [a.target / "debug/controller-plugin-test", mode],
                env=invocation if mode == phase else env,
                check=True,
            )


if __name__ == "__main__":
    main()
