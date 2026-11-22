"""Exercise the public no-build ModuleSetup handoff on a disposable cached project."""

import argparse
import hashlib
import json
import os
import select
import subprocess as s
import sys
import time
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


def probe(p, *, search=False, diagnostics=False, retained=False):
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
        if diagnostics:
            return rpc(2, "tools/call", dict(
                name="lean_status", arguments=dict(kind="file_diagnostics", file="Demo.lean")
            ))
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
        arguments = {"targets": [{"kind": "explicit", "file": "Demo.lean", "declarations": ["demo"]}]}
        if not retained:
            return rpc(2, "tools/call", {"name": "lean_verify", "arguments": arguments})

        def request(i, **fields):
            return tool_payload(rpc(i, "tools/call", {"name": "lean_request", "arguments": fields}))

        ticket = request(2, action="prepare", tool="lean_verify", arguments=arguments)
        key = {"request_id": ticket["taskId"], "request_epoch": ticket["request"]["request_epoch"]}
        task = request(3, action="submit", tool="lean_verify", arguments=arguments, **key)
        deadline = time.monotonic() + 30
        while task["execution"]["disposition"] == "pending":
            assert time.monotonic() < deadline, task
            time.sleep(0.1)
            task = request(4, action="get", **key)
        assert task["status"] == "completed" and task["result_available"], task
        envelope = tool_payload({"result": task["result"]})
        assert envelope["trust"]["cli_file_verification"]["server_instance_id"] == ticket["request"]["server_instance_id"]
        return envelope
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


def tool_payload(reply):
    assert "error" not in reply, reply
    return json.loads(reply["result"]["content"][0]["text"])


def row(reply):
    envelope = tool_payload(reply)
    assert not envelope["errors"], envelope
    return envelope["data"]["results"][0]


def cli_receipt(envelope, project):
    trust = envelope["trust"]
    receipt = trust["cli_file_verification"]
    assert "native_compatibility" not in trust, trust
    assert receipt["schema"] == "lean-host-cli-file-verification-v1", receipt
    assert receipt["project_root"] == str(project), receipt
    assert receipt["source_file"] == str(project / "Demo.lean"), receipt
    assert receipt["lean_toolchain"] == toolchain, receipt
    assert receipt["runtime_lean_version"] == toolchain_id.removeprefix("v"), receipt
    assert receipt["lean_executable"] == str(release / "bin/lean"), receipt
    assert receipt["source_sha256"] == hashlib.sha256((project / "Demo.lean").read_bytes()).hexdigest(), receipt
    data = json.dumps(envelope["data"], ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
    assert receipt["data_sha256"] == hashlib.sha256(data).hexdigest(), receipt
    for key in ("module_setup_sha256", "artifact_identity_sha256", "request_sha256"):
        assert len(bytes.fromhex(receipt[key])) == 32, receipt
    # The direct and retained probes each own their own broker instance.
    return {key: value for key, value in receipt.items() if key != "server_instance_id"}


def check_reader_frames(project):
    driver = b / "ReaderFrame.lean"
    driver.write_text("""import LeanRsFixture.ProofSearchFacts

def main (args : List String) : IO Unit := do
  IO.eprintln (String.ofList (List.replicate 4096 'w'))
  if args == ["large"] then
    IO.println (String.ofList (List.replicate 256 'x'))
  else
    IO.println (LeanRsFixture.ProofSearchFacts.cast 42)
""")
    try:
        for large in (False, True):
            result = s.run(
                [parent, "native-reader", "--stdout-result-frame", "--project-root", project,
                 "--capacity-limit", "1", "--timeout-millis", "15000", "--max-output-bytes", "64",
                 "--", release / "bin/lean", "--run", driver, *(["large"] if large else [])],
                cwd=project, env={**env, "LEAN_PATH": str(project / ".lake/build/lib/lean")},
                capture_output=True, timeout=30,
            )
            assert result.stdout.endswith(b"\n") and result.stdout.count(b"\n") == 1, result.stderr
            frame = json.loads(result.stdout)
            assert frame["target_started"] is True and frame["scope_closed"] is True, frame
            assert frame["exit_code"] == 0 and frame["stderr"] == "w" * 64, frame
            assert any(d["project_root"] == str(project) for d in frame["protected_domains"]), frame
            assert frame["output_truncated"] is large, frame
            if large:
                assert result.returncode == 1 and frame["state"] == "stopped", frame
                assert frame["detail"] == "required payload stdout exceeds capture limit", frame
            else:
                assert result.returncode == 0 and frame["state"] == "completed", frame
                assert frame["stdout"] == "42\n" and frame["detail"] == "payload stderr diagnostic clipped", frame
    finally:
        driver.unlink()


for label, plugin in [("project", True), ("omitted", False)]:
    project = b / label
    if not args.already_built:
        build(project, plugin)
    assert (project / "Demo.lean").read_text() == source
    assert (project / ".lake/build/lib/lean/Demo.olean").is_file()
    before = inventory(project)
    proof = probe(project)
    if plugin:
        check_reader_frames(project)
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
        direct = tool_payload(proof)
        retained = probe(project, retained=True)
        assert retained["data"] == direct["data"], retained
        assert cli_receipt(retained, project) == cli_receipt(direct, project)

    else:
        assert verdict[
            "verification_status"
        ] != "verified" and "missing setup plugin" in str(verdict), verdict
    assert inventory(project) == before, "project changed during verification"

project = b / "project"
# Exercise the packaged setup-aware diagnostics driver on current source. The
# leading informational message alone exceeds the default total output budget.
demo = project / "Demo.lean"
original = demo.read_bytes()
noise = '#check "' + "λ" * 35000 + '"\n'
missing = "MissingAfterReleaseSmokeNoise"
noisy_source = original + (noise + f"#check {missing}\n").encode()
try:
    demo.write_bytes(noisy_source)
    before = inventory(project)
    reply = probe(project, diagnostics=True)
    envelope = json.loads(reply["result"]["content"][0]["text"])
    assert not envelope["errors"], envelope
    diagnostics = envelope["data"]["diagnostics"]
    assert diagnostics["summary"]["errors"] > 0 and diagnostics["truncated"], diagnostics
    line = noisy_source.count(b"\n")
    column = len("#check ")
    expected_range = {
        "file": str(demo), "start_line": line, "start_column": column,
        "end_line": line, "end_column": column + len(missing),
    }
    assert any(
        row["severity"] == "error" and missing in row["message"]
        and row.get("original_range") == expected_range
        for row in diagnostics["diagnostics"]
    ), [row for row in diagnostics["diagnostics"] if row["severity"] == "error"]
    assert inventory(project) == before, "project changed during file diagnostics"
finally:
    demo.write_bytes(original)

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
