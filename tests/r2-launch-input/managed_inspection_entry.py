"""Instrument a private copy of the Lean inspection shim for the H0 fault fixture.

Run before building and sealing the disposable worker's shim package. The
instrumented image appends one line per entry to SCRATCH/inspection-entries.
Each line contains the worker PID and the exact declaration. For this probe
only, execution waits for SCRATCH/release-inspection or its bounded timeout.
No host-side trace or request receipt is needed to observe entry.
"""

import argparse
import json
import os
from pathlib import Path

ENTRY = """def envInspectDeclaration (env : Environment) (request : DeclarationInspectionRequest) (sourceRoots : Array String)
    (heartbeats : UInt64) : IO DeclarationInspectionResult := do
"""


def entry_body(scratch: Path, declaration: str, late_write: bool) -> str:
    def lean_string(value):
        return json.dumps(str(value), ensure_ascii=False)

    body = f"""  if request.name == {lean_string(declaration)} then
    IO.FS.withFile {lean_string(scratch / 'inspection-entries')} .append fun trace => do
      trace.putStrLn s!"{{← IO.Process.getPID}} {{request.name}}"
      trace.flush
    for _ in [0:2000] do
      if ← (System.FilePath.mk {lean_string(scratch / 'release-inspection')}).pathExists then
        break
      IO.sleep 10
"""
    if late_write:
        body += f"""    if ← (System.FilePath.mk {lean_string(scratch / 'release-inspection')}).pathExists then
      IO.FS.withFile {lean_string(scratch / 'inspection-writes')} .append fun writes => do
        writes.putStrLn s!"{{← IO.Process.getPID}} {{request.name}}"
        writes.flush
"""
    return body


def stage(scratch: Path, shim: Path, declaration: str, late_write: bool = False) -> None:
    root = scratch.resolve(strict=True)
    metadata = root.stat()
    if root != scratch or metadata.st_uid != os.geteuid() or metadata.st_mode & 0o777 != 0o700:
        raise ValueError("expected a canonical, private, job-owned scratch directory")
    if (
        shim.resolve(strict=True) != shim
        or not shim.is_relative_to(root)
        or shim.parts[-2:] != ("LeanRsHostShims", "Environment.lean")
        or not shim.is_file()
        or shim.stat().st_nlink != 1
        or shim.stat().st_uid != os.geteuid()
    ):
        raise ValueError("expected a private copy of the inspection shim inside scratch")
    if not declaration or any(not char.isprintable() for char in declaration):
        raise ValueError("the probe requires one printable declaration name")
    if any(
        path.exists() or path.is_symlink()
        for path in (root / "inspection-entries", root / "release-inspection", root / "inspection-writes")
    ):
        raise ValueError("inspection probe state already exists")
    source = shim.read_text()
    if source.count(ENTRY) != 1 or "inspection-entries" in source:
        raise ValueError("expected exactly one uninstrumented inspection body")
    shim.write_text(source.replace(ENTRY, ENTRY + entry_body(root, declaration, late_write), 1))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--scratch", type=Path, required=True)
    parser.add_argument("--shim", type=Path, required=True)
    parser.add_argument("--declaration", required=True)
    parser.add_argument("--late-write", action="store_true", help="add a write witness after the release barrier")
    args = parser.parse_args()
    stage(args.scratch, args.shim, args.declaration, args.late_write)
