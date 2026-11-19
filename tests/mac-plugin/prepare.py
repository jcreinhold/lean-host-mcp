import hashlib
import json
from pathlib import Path


def prepare(here, rec, selected, module_digest, plugin_digest):
    project = here / "project"
    root = project / ".lake/build/lib/lean"
    root.mkdir(parents=True)
    support = here / "worker"
    original = Path(rec["project"])
    (project / "lake-manifest.json").write_text(
        '{"version":"1.3.0","packages":[],"lakeDir":".lake","packagesDir":".lake/packages","name":"host_trial"}'
    )
    (project / "lean-toolchain").write_text("leanprover/lean4:v4.36.0\n")
    (project / "lakefile.toml").write_text('name = "host_trial"\nversion = "0.1.0"\n')
    files = {
        p: v for p, v in rec["files"].items() if not Path(p).is_relative_to(original)
    }
    directories = [
        p for p in rec["directories"] if not Path(p).is_relative_to(original)
    ]
    for name, expected in [
        ("QualifiedPlugin.olean", module_digest),
        ("libQualifiedPlugin.dylib", plugin_digest),
    ]:
        data = (selected / name).read_bytes()
        if hashlib.sha256(data).hexdigest() != expected:
            raise ValueError(f"unreviewed fixture input: {name}")
        dst = root / name
        with dst.open("xb") as out:
            out.write(data)
        st = dst.stat()
        files[str(dst)] = [st.st_dev, st.st_ino, expected]
    for p in [
        project / "lake-manifest.json",
        project / "lean-toolchain",
        project / "lakefile.toml",
    ]:
        st = p.stat()
        files[str(p)] = [
            st.st_dev,
            st.st_ino,
            hashlib.sha256(p.read_bytes()).hexdigest(),
        ]
    paths = [str(root), *rec["shim_paths"]]
    identities = {}
    for path, sealed in zip(
        rec["import_roots"], rec["ordered_directory_ids"], strict=True
    ):
        if path in rec["shim_paths"]:
            st = Path(path).stat()
            if [st.st_dev, st.st_ino] != sealed:
                raise ValueError(f"changed sealed shim root: {path}")
            identities[path] = sealed
    st = root.stat()
    rec.update(
        project=str(project),
        files=files,
        directories=[str(root), *directories],
        roots=sorted([str(project), *rec["shim_roots"]]),
        import_roots=paths,
        ordered_directory_ids=[
            [st.st_dev, st.st_ino],
            *(identities[path] for path in rec["shim_paths"]),
        ],
    )
    for k in list(rec):
        if k.startswith("consumer_"):
            del rec[k]
    (support / "deployment.json").write_text(json.dumps(rec, separators=(",", ":")))
