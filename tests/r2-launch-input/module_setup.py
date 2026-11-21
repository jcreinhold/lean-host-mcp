"""Exercise the public no-build ModuleSetup handoff on a disposable cached project."""

import argparse
import hashlib
import json
import os
import select
import subprocess as s
import sys
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument(
    "--already-built",
    action="store_true",
    help="exercise both existing test projects without any fixture build",
)
args = parser.parse_args()

r = Path(__file__).resolve().parents[2]
b = Path(os.environ.get("R2_SETUP_ROOT") or r / "target/r2-module-setup").resolve()
parent = Path(os.environ.get("R2_SETUP_PARENT") or r / "target/debug/lean-host-mcp").resolve()
toolchain = (r / "fixtures/lean/lean-toolchain").read_text().strip()
toolchain_id = toolchain.rsplit(":", 1)[-1]
worker = b / "workers" / toolchain_id / "current-generation"
if not worker.is_file():
    parser.error(f"install the matching {toolchain_id} worker and shims first: {worker}")
if not parent.is_file() or not os.access(parent, os.X_OK):
    parser.error(f"expected an executable parent binary: {parent}")
prefix_check = s.run([r / "scripts/fixture-lean-prefix.sh"], cwd=r, stdout=s.PIPE, text=True)
if prefix_check.returncode != 0:
    raise SystemExit(prefix_check.returncode)
release = Path(prefix_check.stdout.rstrip("\n"))
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
import LeanRsFixture.ProofSearchFacts
open Lean LeanRsFixture.ProofSearchFacts
theorem demo (q : MiniRat) : cast (q.num * q.den) = q.num * q.den := by
  run_tac
    unless (← getOptionDecls).contains `r2PluginWitness do
      throwError "missing setup plugin"
  exact cast_num_den_helper q
"""


def build(project, plugin):
    project.mkdir(exist_ok=True)
    config = "import Lake\nopen Lake DSL\npackage demo\nlean_lib Witness\nlean_lib LeanRsFixture\nlean_lib Demo"
    if plugin:
        config += " where\n  plugins := #[`@demo/Witness:shared]"
    facts = Path("LeanRsFixture/ProofSearchFacts.lean")
    (project / facts.parent).mkdir(exist_ok=True)
    (project / facts).write_bytes((r / "fixtures/lean" / facts).read_bytes())
    (project / "lakefile.lean").write_text(config + "\n")
    (project / "lean-toolchain").write_text(toolchain + "\n")
    (project / "Witness.lean").write_text(
        'import Lean\nregister_option r2PluginWitness : Bool := { defValue := false, descr := "plugin witness" }\n'
    )
    (project / "Demo.lean").write_text(
        source if plugin else source.split("  run_tac")[0] + "  exact cast_num_den_helper q\n"
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
        try:
            exit_status = proc.wait(timeout=30)
        except s.TimeoutExpired:
            proc.kill()
            proc.wait()
            raise
        finally:
            proc.stdout.close()
        assert exit_status == 0, f"server exited with status {exit_status}"


def row(reply):
    assert "error" not in reply, reply
    envelope = json.loads(reply["result"]["content"][0]["text"])
    assert not envelope["errors"], envelope
    return envelope["data"]["results"][0]


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
        data = search["data"]
        assert data["diagnostics"]["proof_state_status"] == "semantic_frontend", data
        assert not any("no proof-goal features" in warning for warning in data.get("warnings", [])), data
        top = data["candidates"][0]
        assert top["name"] == "LeanRsFixture.ProofSearchFacts.cast_num_den_helper", top
        assert "semantic:" in str(top["match_reason"]), top
        print("semantic_frontend ranked", top["name"], flush=True)
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
