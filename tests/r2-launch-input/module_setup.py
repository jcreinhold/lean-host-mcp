"""Exercise the public no-build ModuleSetup handoff on a disposable cached project."""

import os, json, subprocess as s, hashlib, select, shutil
from pathlib import Path

r = Path(__file__).resolve().parents[2]
b = r / "target/r2-module-setup"
b.mkdir()
p = b / "project"
p.mkdir()
for n in ["owners", "tmp", "workers", "server-owners", "cache"]:
    (b / n).mkdir(mode=0o700)
release = Path.home() / ".elan/toolchains/leanprover--lean4---v4.36.0"
env = dict(
    os.environ,
    LEAN_SYSROOT=str(release),
    TMPDIR=str(b / "tmp"),
    LEAN_HOST_MCP_PROCESS_REGISTRY_DIR=str(b / "server-owners"),
    LEAN_RS_WORKER_OWNERS_DIRECTORY=str(b / "owners"),
    LEAN_HOST_MCP_WORKERS_DIR=str(b / "workers"),
    LAKE_CACHE_DIR=str(b / "cache"),
)
(p / "lakefile.toml").write_text(
    'name = "demo"\n[[lean_lib]]\nname = "Dep"\n[[lean_lib]]\nname = "Demo"\n'
)
(p / "lean-toolchain").write_text("leanprover/lean4:v4.36.0\n")
(p / "Dep.lean").write_text("theorem dep : True := True.intro\n")
(p / "Demo.lean").write_text("import Dep\ntheorem demo : True := dep\n")
s.run(
    [release / "bin/lake", "--no-cache", "build", "+Demo"],
    cwd=p,
    env=env,
    check=True,
    capture_output=True,
)


def inventory():
    return {
        str(f.relative_to(p)): (
            f.stat().st_ino,
            f.stat().st_mtime_ns,
            hashlib.sha256(f.read_bytes()).digest(),
        )
        for f in p.rglob("*")
        if f.is_file()
    }


def probe():
    proc = s.Popen(
        [
            r / "target/debug/lean-host-mcp",
            "--lake-root",
            p,
            "--request-journal",
            b / "journal",
        ],
        env=env,
        stdin=s.PIPE,
        stdout=s.PIPE,
    )

    def rpc(i, m, params):
        msg = dict(jsonrpc="2.0", method=m, params=params)
        if i:
            msg["id"] = i
        proc.stdin.write((json.dumps(msg) + "\n").encode())
        proc.stdin.flush()
        if not i:
            return
        while select.select([proc.stdout], [], [], 40)[0]:
            v = json.loads(proc.stdout.readline(1048576))
            if v.get("id") == i:
                return v
        raise TimeoutError

    try:
        rpc(
            1,
            "initialize",
            dict(
                protocolVersion="2025-03-26",
                capabilities={},
                clientInfo=dict(name="setup", version="1"),
            ),
        )
        rpc(None, "notifications/initialized", {})
        return rpc(
            2,
            "tools/call",
            dict(
                name="lean_verify",
                arguments=dict(
                    targets=[
                        dict(kind="explicit", file="Demo.lean", declarations=["demo"])
                    ]
                ),
            ),
        )
    finally:
        proc.stdin.close()
        assert proc.wait(timeout=30) == 0


a = inventory()
v = probe()
print(str(v)[:1000])
assert "Lake module setup staged" in str(v)
assert inventory() == a
dep = p / ".lake/build/lib/lean/Dep.olean"
dep.rename(b / "held-Dep.olean")
v = probe()
print(str(v)[:1000])
assert (
    "Lake module setup staged" not in str(v)
    and not dep.exists()
    and ("failed" in str(v) or "No such file" in str(v))
)
(b / "held-Dep.olean").rename(dep)
print("restored; no rebuild")
records = [f for f in b.glob(".lean-rs-writer-*.lock") if "demand" not in f.name]
assert len(records) == 1 and json.loads(records[0].read_text())["process"] is None
assert not list((b / "owners").iterdir())
assert sum(f.stat().st_blocks * 512 for f in b.rglob("*")) < 16 * 1024 * 1024
shutil.rmtree(b)
assert not b.exists()
