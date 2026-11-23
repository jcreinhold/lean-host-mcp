"""Exercise the operator command with one real path dependency and registered Elan."""

import argparse
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
        dep = project / "dep"
        dep.mkdir()
        env = {**os.environ, "TMPDIR": directory, "LEAN_SYSROOT": str(sysroot)}
        for key in ("LEAN_PATH", "LEAN_SRC_PATH", "ELAN_TOOLCHAIN"):
            env.pop(key, None)
        for root in (project, dep):
            (root / "lean-toolchain").write_text("leanprover/lean4:v4.36.0\n")
        (project / "lakefile.toml").write_text(
            "name = 'root'\n[[require]]\nname = 'dep'\npath = 'dep'\n[[lean_lib]]\nname = 'Root'\n"
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
        plan.write_text(json.dumps(selection))
        command = [host, "prepare-generation", "--plan", plan, "--writer", writer]
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
