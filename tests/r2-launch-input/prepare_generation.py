"""Exercise the operator command with one real path dependency and registered Elan."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("host", "writer", "sysroot", "scratch"):
        parser.add_argument(f"--{name}", type=Path, required=True)
    args = parser.parse_args()
    host, writer, sysroot = (p.resolve(strict=True) for p in (args.host, args.writer, args.sysroot))
    with tempfile.TemporaryDirectory(prefix="pg.", dir=args.scratch.resolve(strict=True)) as directory:
        project = Path(directory)
        # The source-closure resolver searches the project first. On a
        # case-insensitive volume, a directory named "dep" would shadow the Dep
        # module prefix.
        dep = project / "dependency"
        dep.mkdir()
        env = {**os.environ, "TMPDIR": directory, "LEAN_SYSROOT": str(sysroot)}
        for key in ("LEAN_PATH", "LEAN_SRC_PATH", "ELAN_TOOLCHAIN"):
            env.pop(key, None)
        for root in (project, dep):
            (root / "lean-toolchain").write_text("leanprover/lean4:v4.36.0\n")
        (project / "lakefile.toml").write_text(
            f"name = 'root'\n[[require]]\nname = 'dep'\npath = '{dep.name}'\n[[lean_lib]]\nname = 'Root'\n"
        )
        (dep / "lakefile.toml").write_text("name = 'dep'\n[[lean_lib]]\nname = 'Dep'\n")
        (dep / "Dep.lean").write_text("theorem dependency_ok : True := True.intro\n")
        (project / "Root.lean").write_text("import Lean\nimport Dep\ntheorem first_ok : True := dependency_ok\n")
        for root in (dep, project):
            subprocess.run([sysroot / "bin/lake", "--no-cache", "update"], cwd=root, env=env, check=True)
        release = sysroot / "lib/lean"
        plan = project / "plan.json"
        selection = {
            "project": str(project),
            "roots": [str(root / ".lake/build/lib/lean") for root in (project, dep)] + [str(release)],
            "release": str(release),
            "modules": [["Root", str(project)], ["Dep", str(dep)]],
        }
        command = [host, "prepare-generation", "--plan", plan, "--writer", writer]
        adapter = project / "Missing.lean"
        adapter.write_text('def main : IO Unit := throw (IO.userError "missing selected archive")\n')
        scratch = project / "archives"
        scratch.mkdir()
        materialization = {
            "adapter": str(adapter),
            "adapter_sha256": list(hashlib.sha256(adapter.read_bytes()).digest()),
            "runtime_roots": [],
            "source_roots": [str(dep)],
            "modules": ["Dep"],
            "archive_scratch": str(scratch),
        }
        plan.write_text(json.dumps({**selection, "dependency_materialization": materialization}))
        refused = subprocess.run(command, cwd=project, env=env, capture_output=True, text=True)
        assert refused.returncode != 0 and not refused.stdout, refused
        assert "missing selected archive" in refused.stderr, refused.stderr
        assert not (project / ".lake/build").exists()
        assert not (dep / ".lake/build").exists()

        # The successful run must reacquire the same writer roots.
        plan.write_text(json.dumps(selection))
        result = subprocess.run(command, cwd=project, env=env, check=True, capture_output=True, text=True)
        record = json.loads(result.stdout)
        assert record["plan"] == {**selection, "installed_release": "leanprover/lean4:v4.36.0"}
        assert len(record["identity"]) == 32 and all(row[0] for row in record["products"])
        assert (project / ".lake/build/lib/lean/Root.olean").is_file()
        assert (dep / ".lake/build/lib/lean/Dep.olean").is_file()
        refused = subprocess.run(command, cwd=project, env=env, capture_output=True, text=True)
        assert refused.returncode != 0 and not refused.stdout, refused
        assert "requires absent output trees" in refused.stderr, refused.stderr
        (project / "lean-toolchain").write_text("leanprover/lean4:v4.35.0\n")
        refused = subprocess.run(command, cwd=project, env=env, capture_output=True, text=True)
        assert refused.returncode != 0 and not refused.stdout, refused
        assert "unapproved installed Elan release" in refused.stderr, refused.stderr


if __name__ == "__main__":
    main()
