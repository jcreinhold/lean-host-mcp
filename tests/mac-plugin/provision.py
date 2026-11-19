"""Run the SDK application's guarded prepare/seal against the plugin-capable SDK."""

import argparse
import importlib.util
import io
import json
import platform
import shutil
import subprocess
import sys
import tarfile
from pathlib import Path

from run import SDK, bounded_run, verified

FIXTURE_COMMIT = "a091e4a9427040c5741c8f84f9508fa8b7aed665"
FIXTURE_PATH = "fixtures/metadata-qualification/app"
FIXTURE_TREE = "ee1bf0cecda10560c7b58b33ea679188f87fea9c"


def fixture_tree(repo):
    git = ["git", "-C", str(repo)]
    tree = subprocess.check_output(
        git + ["rev-parse", "--verify", f"{FIXTURE_COMMIT}:{FIXTURE_PATH}"], text=True
    ).strip()
    if tree != FIXTURE_TREE:
        raise ValueError("SDK fixture subtree differs from the reviewed application")
    return git


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for key in ("sdk", "fixture-repo", "writer", "lean-release", "scratch", "target"):
        parser.add_argument("--" + key, required=True, type=lambda p: Path(p).resolve())
    parser.add_argument("--writer-sha256", required=True)
    a = parser.parse_args()
    if (sys.platform, platform.machine()) != ("darwin", "arm64"):
        raise ValueError("requires an arm64 Mac")
    git = ["git", "-C", str(a.sdk)]
    if subprocess.check_output(
        git + ["rev-parse", "HEAD"], text=True
    ).strip() != SDK or subprocess.check_output(git + ["status", "--porcelain"]):
        raise ValueError("dirty or incorrect SDK")
    spec = importlib.util.spec_from_file_location(
        "supplier", a.sdk / "scripts/qualify_metadata.py"
    )
    supplier = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(supplier)
    fixture_git = fixture_tree(a.fixture_repo)
    writer_bytes = verified(a.writer, a.writer_sha256)
    for name, digest in {
        "bin/lean": "a4c5d3dfc1672faf014e59bd17724035259ff0d22a4ffbdf5a75e142f564817e",
        "bin/lake": "a0e516de0ba03a26cb90c80f06666022ef9de3f0943f8b26e9b902d229b43ebe",
        "lib/lean/libleanshared.dylib": "9fa4e429820684b59ff37740ca77744a2cd6de70e6e963ef0904760c6921aed5",
    }.items():
        supplier.check_file(a.lean_release / name, digest)
    supplier.space(a.target)
    supplier.space(a.scratch.parent)
    a.scratch.mkdir(mode=0o700)
    root = a.scratch
    writer = root / "lean-host-mcp"
    writer.write_bytes(writer_bytes)
    writer.chmod(0o500)
    del writer_bytes
    app = root / "app"
    app.mkdir()
    entries = subprocess.check_output(fixture_git + ["ls-tree", "-rz", FIXTURE_TREE])
    for entry in entries.rstrip(b"\0").split(b"\0"):
        header, filename = entry.split(b"\t", 1)
        mode, kind, blob = header.decode("ascii").split()
        if kind != "blob" or mode not in ("100644", "100755"):
            raise ValueError("expected only ordinary fixture source files")
        target = app / filename.decode("utf-8")
        target.parent.mkdir(exist_ok=True, parents=True)
        target.write_bytes(
            subprocess.check_output(fixture_git + ["cat-file", "blob", blob])
        )
        target.chmod(int(mode, 8) & 0o777)
    sdk = root / "sdk"
    sdk.mkdir()
    archive = subprocess.check_output(git + ["archive", SDK])
    with tarfile.open(fileobj=io.BytesIO(archive)) as source:
        source.extractall(sdk, filter="data")
    bounded_run(
        ["/bin/cp", "-cR", a.lean_release, root / "release"], root, {}, root, a.target
    )
    (root / "owners").mkdir(mode=0o700)
    project = app / "lean"
    project.mkdir()
    fixture = sdk / "fixtures/interop-shims"
    shutil.copytree(
        fixture / "LeanRsInteropConsumer", project / "LeanRsInteropConsumer"
    )
    for name in ("LeanRsInteropConsumer.lean", "lean-toolchain"):
        shutil.copyfile(fixture / name, project / name)
    lakefile = (fixture / "lakefile.lean").read_text()
    dependency = "../../crates/lean-rs/shims/lean-rs-interop-shims"
    if lakefile.count(dependency) != 1:
        raise ValueError("unexpected interop fixture dependency")
    (project / "lakefile.lean").write_text(
        lakefile.replace(
            dependency, str(sdk / "crates/lean-rs-host/shims/lean-rs-interop-shims")
        )
    )
    env = supplier.environment(root, writer, a.target)
    for cwd, command in [
        (project, ["lake", "update"]),
        (app, ["cargo", "build", "--locked", "--offline", "-j1", "--bin", "prepare"]),
        (app, [a.target / "debug/prepare"]),
    ]:
        supplier.space(a.target)
        bounded_run(command, cwd, env, a.target, a.scratch)
    record = app / "deployment.json"
    sealed = json.loads(record.read_bytes())
    exports = json.loads(sealed["shim_json"])["exports"]
    symbol = "lean_rs_host_session_import_plugin_v1"
    if not any(item.get("symbol") == symbol for item in exports):
        raise ValueError("sealed manifest lacks attested plugin import")
    image = Path(sealed["host_image"])
    device, inode, digest = sealed["files"][str(image)]
    supplier.check_file(image, digest)
    st = image.stat()
    symbols = subprocess.check_output(["/usr/bin/nm", "-gU", image], text=True)
    if [device, inode] != [st.st_dev, st.st_ino] or "_" + symbol not in {
        line.split()[-1] for line in symbols.splitlines() if line.strip()
    }:
        raise ValueError("sealed host image lacks attested plugin import")
    print(supplier.digest(record), record)


if __name__ == "__main__":
    main()
