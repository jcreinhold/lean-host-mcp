"""Check that both compiled preparation examples reject conflicting writer settings."""

import argparse
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("development-shims", "semantic-smoke", "sysroot", "scratch"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("build_development_shims requires macOS")

    checkout = Path(__file__).resolve().parents[2]
    scratch = args.scratch.resolve(strict=True)
    if not scratch.is_dir() or not scratch.is_relative_to(checkout / "target"):
        parser.error("--scratch must be an existing directory under the host checkout's target directory")
    toolchain = (checkout / "fixtures/lean/lean-toolchain").read_text().strip()
    sysroot = args.sysroot.resolve(strict=True)
    if not (sysroot / "bin/lake").is_file() or not (sysroot / "include/lean/lean.h").is_file():
        parser.error("--sysroot must be an installed Lean prefix for the host fixture toolchain")
    shims = args.development_shims.resolve(strict=True)
    semantic = args.semantic_smoke.resolve(strict=True)
    for executable in (shims, semantic):
        if not executable.is_file() or not os.access(executable, os.X_OK):
            parser.error(f"expected a compiled example executable at {executable}")

    with tempfile.TemporaryDirectory(prefix="example-alias.", dir=scratch) as directory:
        root = Path(directory)
        sdk = root / "sdk"
        for name in ("lean-rs-host-shims", "lean-rs-interop-shims"):
            # Isolate any lock files an incorrectly admitted build would open.
            (sdk / "crates/lean-rs-host/shims" / name).mkdir(parents=True)
        output, cache, tmp = (root / name for name in ("output", "cache", "tmp"))
        for path in (output, cache, tmp):
            path.mkdir(mode=0o700)
        marker = root / "writer-invoked"
        owners = root / "owners"
        legacy_writer = root / "legacy-writer"
        neutral_writer = root / "neutral-writer"
        trap = f'#!/bin/sh\nprintf "invoked\\n" > {shlex.quote(str(marker))}\nexit 97\n'
        for writer in (legacy_writer, neutral_writer):
            writer.write_text(trap)
            writer.chmod(0o700)
        env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("LEAN_", "LAKE_", "DYLD_", "__XPC_DYLD_"))
        }
        env.update(
            TMPDIR=str(tmp),
            LAKE_CACHE_DIR=str(tmp / "lake-cache"),
            LEAN_SYSROOT=str(sysroot),
            LEAN_RS_WORKER_OWNERS_DIRECTORY=str(owners),
            LEAN_HOST_MCP_WRITER_EXECUTABLE=str(legacy_writer),
            LEAN_RS_WRITER_EXECUTABLE=str(neutral_writer),
        )
        conflict = "conflicting LEAN_RS_WRITER_EXECUTABLE and LEAN_HOST_MCP_WRITER_EXECUTABLE values"
        for command in (
            [shims, sdk, toolchain, output, legacy_writer],
            [semantic, cache, toolchain, sysroot, legacy_writer],
        ):
            outcome = subprocess.run(command, cwd=root, env=env, capture_output=True, text=True, timeout=15)
            assert outcome.returncode != 0 and not outcome.stdout and conflict in outcome.stderr, outcome
            assert not marker.exists(), f"{command[0].name} launched the writer"
            assert not os.path.lexists(owners), f"{command[0].name} created the owner directory"
            assert not any(output.iterdir()) and not any(cache.iterdir()), f"{command[0].name} created build state"
        print("Both preparation examples refused conflicting writer settings before creating state or starting the writer")


if __name__ == "__main__":
    main()
