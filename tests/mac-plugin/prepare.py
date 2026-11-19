import hashlib
import json
from pathlib import Path


def prepare(here, rec):
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
        (
            "QualifiedPlugin.olean",
            "ad957f043228ba7f35b4bd2e464b6bb6a8dfb3cedaed510c779882ad4ccfb2ea",
        ),
        (
            "libQualifiedPlugin.dylib",
            "b10ea2003d5e8a4cc1cb2f4276805f608832f7fbfd0665a093200f508c743c6c",
        ),
    ]:
        src = original / ".lake/build/lib/lean" / name
        if (
            hashlib.sha256(src.read_bytes()).hexdigest() != expected
            or rec["files"][str(src)][2] != expected
        ):
            raise ValueError("unreviewed fixture input")
        dst = root / name
        with dst.open("xb") as out:
            out.write(src.read_bytes())
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
    rec.update(
        project=str(project),
        files=files,
        directories=[str(root), *directories],
        roots=sorted([str(project), *rec["shim_roots"]]),
        import_roots=paths,
        ordered_directory_ids=[
            (Path(p).stat().st_dev, Path(p).stat().st_ino) for p in paths
        ],
    )
    for k in list(rec):
        if k.startswith("consumer_"):
            del rec[k]
    (support / "deployment.json").write_text(json.dumps(rec, separators=(",", ":")))
