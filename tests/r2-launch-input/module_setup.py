"""Exercise the public no-build ModuleSetup handoff on a disposable cached project."""

import hashlib
import json
import os
import select
import sys
import subprocess as s
import argparse
from pathlib import Path

r = Path(__file__).resolve().parents[2]
b = Path(os.environ.get("R2_SETUP_ROOT", r / "target/r2-module-setup"))
parent = Path(os.environ.get("R2_SETUP_PARENT", r / "target/debug/lean-host-mcp"))
release = Path.home() / ".elan/toolchains/leanprover--lean4---v4.36.0"
assert (b / "workers/v4.36.0/current-generation").is_file(), (
    "install the matching 4.36 worker and shims first"
)
env = dict(
    os.environ,
    LEAN_SYSROOT=str(release),
    TMPDIR=str(b / "tmp"),
    LEAN_HOST_MCP_PROCESS_REGISTRY_DIR=str(b / "server-owners"),
    LEAN_RS_WORKER_OWNERS_DIRECTORY=str(b / "owners"),
    LEAN_HOST_MCP_WORKERS_DIR=str(b / "workers"),
    LAKE_CACHE_DIR=str(b / "cache"),
    LAKE_ARTIFACT_CACHE="false",
)
source = """import Lean
open Lean
theorem demo : True := by
  run_tac
    unless (← getOptionDecls).contains `r2PluginWitness do
      throwError "missing setup plugin"
  trivial
"""


def build(project, plugin):
    project.mkdir(exist_ok=True)
    config = "import Lake\nopen Lake DSL\npackage demo\nlean_lib Witness\nlean_lib Demo"
    if plugin:
        config += " where\n  plugins := #[`@demo/Witness:shared]"
    (project / "lakefile.lean").write_text(config + "\n")
    (project / "lean-toolchain").write_text("leanprover/lean4:v4.36.0\n")
    (project / "Witness.lean").write_text(
        'import Lean\nregister_option r2PluginWitness : Bool := { defValue := false, descr := "plugin witness" }\n'
    )
    (project / "Demo.lean").write_text(
        source if plugin else "import Lean\ntheorem demo : True := True.intro\n"
    )
    s.run(
        [release / "bin/lake", "--no-cache", "build", "+Demo"],
        cwd=project,
        env=env,
        check=True,
        capture_output=True,
    )
    if not plugin:
        (project / "Demo.lean").write_text(source)


def inventory(p):
    return {
        str(f.relative_to(p)): (
            f.stat().st_ino,
            f.stat().st_mtime_ns,
            hashlib.sha256(f.read_bytes()).digest(),
        )
        for f in p.rglob("*")
        if f.is_file()
    }


def probe(p, *, search=False):
    proc = s.Popen(
        [
            parent,
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
        if search:
            return rpc(
                2,
                "tools/call",
                dict(
                    name="lean_lookup",
                    arguments=dict(
                        kind="proof_search", file="Demo.lean", declaration="demo"
                    ),
                ),
            )
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


def row(reply):
    assert "error" not in reply, reply
    envelope = json.loads(reply["result"]["content"][0]["text"])
    assert not envelope["errors"], envelope
    return envelope["data"]["results"][0]


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--already-built",
    action="store_true",
    help="exercise both existing test projects without any fixture build",
)
args = parser.parse_args()

for label, plugin in [("project", True), ("omitted", False)]:
    project = b / label
    if not args.already_built:
        build(project, plugin)
    assert (project / "Demo.lean").read_text() == source
    assert (project / ".lake/build/lib/lean/Demo.olean").is_file()
    before = inventory(project)
    proof = probe(project)
    if plugin:
        lookup = probe(project, search=True)
        for tool, response in [("lean_verify", proof), ("lean_lookup", lookup)]:
            if "error" in response:
                print(tool, response["error"], flush=True)
    verdict = row(proof)
    print(label, verdict["verification_status"])
    if plugin:
        reply = lookup
        assert "error" not in reply, reply
        search = json.loads(reply["result"]["content"][0]["text"])
        assert not search["errors"], search
        assert "candidates" in search["data"], search
        assert verdict["verification_status"] == "verified", verdict
    else:
        assert verdict[
            "verification_status"
        ] != "verified" and "missing setup plugin" in str(verdict), verdict
    assert inventory(project) == before, "project changed during verification"

project = b / "project"
(image,) = project.glob(".lake/build/lib/*Witness." + ("dylib" if sys.platform == "darwin" else "so"))
saved = b / "held-plugin.dylib"
image.rename(saved)
try:
    refused = probe(project)
    print("missing plugin", refused.get("error"))
    assert (
        "error" in refused
        and not image.exists()
        and "needs to be rebuilt" in str(refused)
    ), refused
finally:
    saved.rename(image)
