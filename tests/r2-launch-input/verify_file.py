"""Check the prepared disposable R2 fixture via the ordinary stdio endpoint."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--ordinary-imports", action="store_true")
    parser.add_argument("--fixture", type=Path)
    args = parser.parse_args()
    checkout = Path(__file__).resolve().parents[2]
    fixture = args.fixture or checkout / "target" / "r2-prepared-project"
    project = fixture / "project"
    source = project / "Root.lean"
    original = source.read_bytes()
    proof = b"theorem first_ok : True := dependency_ok"
    assert source.resolve() == source and original.count(proof) == 1
    artifact = project / ".lake/build/lib/lean/Root.olean"
    initial_hash = hashlib.sha256(artifact.read_bytes()).digest()
    generation = None if args.ordinary_imports else (fixture / "installed-input").read_text()
    release = Path(os.environ["LEAN_SYSROOT"]) if generation is None else Path(json.loads(generation)["plan"]["release"]).parents[1]
    environment = {
        **os.environ,
        "LEAN_SYSROOT": str(release),
        "LEAN_RS_WORKER_OWNERS_DIRECTORY": str(fixture / "owners"),
        "LEAN_HOST_MCP_WORKERS_DIR": str(fixture / "workers"),
        "LEAN_HOST_MCP_PROCESS_REGISTRY_DIR": str(fixture / "server-owners"),
        "TMPDIR": str(fixture / "tmp"),
    }
    command = [
        checkout / "target/debug/lean-host-mcp",
        "--lake-root", project,
        *([] if args.ordinary_imports else ["--prepared-generation", generation]),
        "--request-journal", fixture / "journal",
    ]
    with subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                          env=environment) as process:
        def send(message):
            message["jsonrpc"] = "2.0"
            process.stdin.write((json.dumps(message) + "\n").encode())
            process.stdin.flush()

        def receive(call_id):
            while select.select([process.stdout], [], [], 60)[0]:
                reply = json.loads(process.stdout.readline(1_048_576))
                if reply.get("id") == call_id:
                    return reply["result"]
            raise TimeoutError(f"No reply for {call_id}")

        def verify(call_id, allow_sorry=False, report_axioms=False):
            send({"id": call_id, "method": "tools/call", "params": {
                "name": "lean_verify", "arguments": {
                    "targets": [{"kind": "explicit", "file": "Root.lean",
                                 "declarations": ["first_ok"]}],
                    "allow_sorry": allow_sorry, "report_axioms": report_axioms,
                }}})
            result = json.loads(receive(call_id)["content"][0]["text"])
            assert result["data"], result["errors"]
            row, = result["data"]["results"]
            assert row["id"] == "group_1:first_ok"
            return row

        def diagnostics(call_id):
            send({"id": call_id, "method": "tools/call", "params": {
                "name": "lean_status", "arguments": {"kind": "file_diagnostics", "file": "Root.lean"}}})
            reply = json.loads(receive(call_id)["content"][0]["text"])
            assert reply["data"] is not None, reply["errors"]
            return reply["data"]["diagnostics"]

        try:
            send({"id": 1, "method": "initialize", "params": {
                "protocolVersion": "2025-03-26", "capabilities": {},
                "clientInfo": {"name": "file-verification-test", "version": "1"}}})
            receive(1)
            send({"method": "notifications/initialized"})
            good = verify(2, report_axioms=True)
            assert good["verification_status"] == "verified" and good["facts"]["axioms_available"]

            if args.ordinary_imports:
                for call_id, target in enumerate((
                    {"kind": "file_all", "file": "Root.lean"},
                    {"kind": "module_all", "module": "Root"},
                ), start=10):
                    send({"id": call_id, "method": "tools/call", "params": {
                        "name": "lean_verify", "arguments": {"targets": [target]}}})
                    reply = json.loads(receive(call_id)["content"][0]["text"])
                    rows = reply["data"]["results"]
                    assert any(row["id"] == good["id"] and row["verification_status"] == "verified"
                               for row in rows), reply
                clean = diagnostics(12)
                assert clean["summary"]["errors"] == 0 and not clean.get("truncated"), clean
                # Several bounded messages exercise the aggregate cap even when Lean
                # retains the last complete message that crossed its byte budget.
                source.write_bytes(original + (b'\n#check "' + b"x" * 512 + b'"\n') * 20)
                clipped = diagnostics(13)
                assert clipped.get("truncated"), clipped
            source.write_bytes(original.replace(proof, b"theorem first_ok : True := False.intro"))
            if args.ordinary_imports:
                errors = diagnostics(14)
                line = original[:original.index(proof)].count(b"\n") + 1
                column = len(b"theorem first_ok : True := ")
                assert errors["summary"]["errors"] > 0 and any(
                    row["severity"] == "error"
                    and "False.intro" in row["message"]
                    and (span := row.get("original_range"))
                    and Path(span["file"]) == source
                    and span["start_line"] == span["end_line"] == line
                    and span["start_column"] <= column < span["end_column"]
                    for row in errors["diagnostics"]
                ), errors
            broken = verify(3)
            assert broken["verification_status"] != "verified" and broken["facts"]["diagnostics"]["diagnostics"]

            source.write_bytes(original.replace(proof, b"theorem first_ok : True := sorry"))
            if args.ordinary_imports:
                warnings = diagnostics(15)
                assert warnings["summary"]["warnings"] > 0 and any(
                    row["severity"] == "warning" and "sorry" in row["message"]
                    for row in warnings["diagnostics"]), warnings
            denied = verify(4)
            assert denied["verification_status"] == "has_sorry" and denied["facts"]["contains_sorry"]
            allowed = verify(5, allow_sorry=True)
            assert allowed["verification_status"] == "verified" and allowed["facts"]["contains_sorry"]
            assert not allowed["facts"]["axioms_available"]
        finally:
            source.write_bytes(original)
            process.stdin.close()
            assert process.wait(timeout=40) == 0
            assert hashlib.sha256(artifact.read_bytes()).digest() == initial_hash


if __name__ == "__main__":
    main()
