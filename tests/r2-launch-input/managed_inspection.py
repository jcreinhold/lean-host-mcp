"""Check one cold managed inspection against a matched disposable installation."""

import argparse
import fcntl
from contextlib import contextmanager, ExitStack
import hashlib
import itertools
import json
import os
from pathlib import Path
import queue
import signal
import sqlite3
import subprocess
import threading
import time


def run_check(args):
    scratch = args.scratch.resolve(strict=True)
    project = args.project.resolve(strict=True)
    prepared = args.prepared.read_text().strip()
    plan = json.loads(prepared)["plan"]
    assert Path(plan["project"]).resolve(strict=True) == project
    root = scratch / "managed"
    root.mkdir(mode=0o700)
    for name in ("config", "owners", "process", "tmp", "cache"):
        (root / name).mkdir(mode=0o700)
    assert not (project / "lean-host-mcp.toml").exists()
    db = root / "stages.sqlite3"
    env = {
        key: val for key, val in os.environ.items()
        if not key.startswith(("LEAN_", "LAKE_", "DYLD_", "__XPC_DYLD_"))
    }
    env.update(
        TMPDIR=str(root / "tmp"),
        LEAN_HOST_MCP_CONFIG_DIR=str(root / "config"),
        LEAN_HOST_MCP_PROCESS_REGISTRY_DIR=str(root / "process"),
        LEAN_HOST_MCP_SOCKET_DIR=str(scratch / "s"),
        LEAN_HOST_MCP_WORKERS_DIR=str(args.workers.resolve(strict=True)),
        LEAN_HOST_MCP_WRITER_EXECUTABLE=str(args.writer.resolve(strict=True)),
        LEAN_RS_WORKER_OWNERS_DIRECTORY=str(root / "owners"),
        LEAN_HOST_MCP_MAX_WORKERS="2" if args.abrupt else "1",
        LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT="2" if args.abrupt else "1",
        LAKE_CACHE_DIR=str(root / "cache"),
    )
    (scratch / "s").mkdir(mode=0o700)
    command = [
        args.parent.resolve(strict=True), "--lake-root", project,
        "--prepared-generation", prepared, "--managed-native-store", db,
    ]

    @contextmanager
    def server(*, stop_before_arm=False):
        child_env = env
        if stop_before_arm:
            child_env = dict(env, LEAN_HOST_MCP_TEST_STOP_BEFORE_ARM="1", LEAN_RS_WORKER_GUARDIAN_DRAIN_SLOT="1")
        with subprocess.Popen(
            command, cwd=root, env=child_env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True
        ) as proc:
            replies = queue.Queue()
            ids = itertools.count(1)

            def read():
                for line in proc.stdout:
                    message = json.loads(line)
                    if "id" in message:
                        replies.put(message)

            threading.Thread(target=read, daemon=True).start()

            def send(method, params, *, notification=False):
                message = dict(jsonrpc="2.0", method=method, params=params)
                if not notification:
                    message["id"] = next(ids)
                proc.stdin.write(json.dumps(message) + "\n")
                proc.stdin.flush()
                return message.get("id")

            def receive(ident, *, poll=False, error=None):
                end = time.monotonic() + 25
                while True:
                    answer = replies.get(timeout=max(0, end - time.monotonic()))
                    if answer["id"] == ident:
                        if error is not None:
                            assert answer.get("error", {}).get("message") == error, answer
                            return None
                        if poll and answer.get("error", {}).get("message") == "store_busy":
                            return None
                        assert "error" not in answer, answer
                        return answer["result"]
                    # A successful original START response is deliberately discarded.
                    assert "error" not in answer, answer

            def tool(name, arguments):
                return send("tools/call", dict(name=name, arguments=arguments))

            def result(ident, *, poll=False, error=None):
                reply = receive(ident, poll=poll, error=error)
                if reply is None:
                    return None
                assert not reply.get("isError"), reply
                return json.loads(reply["content"][0]["text"])

            def managed(op, **fields):
                request = json.dumps(dict(version=1, command=dict(op=op, **fields)))
                return tool("lean_managed_admission", dict(request=request))

            receive(send("initialize", dict(
                protocolVersion="2025-11-25", capabilities={},
                clientInfo=dict(name="prepared-managed-check", version="1"),
            )))
            send("notifications/initialized", {}, notification=True)
            try:
                yield managed, result, tool, proc
            finally:
                proc.stdin.close()
                try:
                    assert proc.wait(timeout=15) in (0, -signal.SIGKILL)
                finally:
                    if proc.poll() is None:
                        proc.kill()
                        proc.wait()

    def bindings():
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
            return conn.execute("SELECT registry_generation FROM recovery_bindings").fetchall()

    with ExitStack() as stopped:
        with server(stop_before_arm=args.stopped_owner) as (managed, result, tool, proc):
            before = result(tool("lean_runtime", {"action": "status"}))["pool"]
            assert before["occupied_slots"] == 0 and not before["projects"]
            discovery = result(managed("discover"))
            assert discovery["dispatch_enabled"] is True
            key = dict(store=discovery["store"], epoch=discovery["acceptance_epoch"],
                       nonce=list(os.urandom(16)), stage=1)
            cap = list(os.urandom(32))
            intent = list(json.dumps(dict(
                job=list(os.urandom(16)), revision=1,
                prepared_sha256=list(hashlib.sha256(prepared.encode()).digest()),
                imports=[module for module, _ in plan["modules"]],
                declaration=args.declaration,
            )).encode())
            origin = dict(key=key, capability=cap)
            creation = dict(origin, intent=intent)
            if args.stopped_owner:
                drain_authorization = list(os.urandom(32))
                creation["drain_authorization"] = drain_authorization
            assert result(managed("create", **creation))["execution"] == "admitted"
            if args.fail_result_save:
                with sqlite3.connect(db) as fault:
                    fault.execute(
                        "CREATE TRIGGER reject_result BEFORE UPDATE ON stages WHEN NEW.result_kind=1 "
                        "BEGIN SELECT RAISE(ABORT, 'managed result save rejected'); END"
                    )
                settled = result(managed("start", **origin))
                assert settled["custody"] == "closed" and settled["execution"] == "unknown", settled
                assert settled["result"]["state"] == "pending"
                assert result(managed("get", **origin)) == settled
                assert result(managed("start", **origin)) == settled
                assert len(bindings()) == 1
                print("result save failed; confirmed custody closure was still committed without replay")
                return
            original_start = managed("start", **origin)
            if args.stopped_owner:
                # Stages: CommitStoppedBinding, ConsumeStoppedPermit, then PauseRecoveryBroker.
                # The gate below is the durable grant witness. H0's fence-first cut needs zero
                # entries at the worker's declaration-inspection shim across recovery and retry.
                try:
                    end = time.monotonic() + 15
                    with sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=1) as observe:
                        while True:
                            row = observe.execute(
                                "SELECT r.identity FROM stages s JOIN recovery_bindings r USING(epoch,nonce,stage) "
                                "WHERE s.gate=1 AND s.result_kind=0 AND s.resource_report=1"
                            ).fetchone()
                            if row:
                                break
                            assert time.monotonic() < end, "missing committed pre-arm scope"
                            time.sleep(0.01)
                    scope = json.loads(row[0])
                    generation = scope["generation"]
                    record = root / "owners" / f"worker-{generation}" / "owner.json"
                    assert json.loads(record.read_bytes())["guardian_drain_protocol"] == "slot-v1"
                    with server() as (recover, read, _, replacement):
                        lookup = read(recover("get", **origin))
                        assert lookup["custody"] == "open"
                        assert lookup["recovery"] == dict(generation=generation, revision=1)
                        claim = dict(origin, **lookup["recovery"])
                        changed = dict(claim, generation="0" * 48)
                        read(recover("drain", **claim, submit=True), error="capability_denied")
                        read(recover("drain", **claim, submit=True, authorization=cap), error="capability_denied")
                        read(recover("drain", **changed, submit=True, authorization=drain_authorization),
                             error="recovery_claim_changed")
                        assert read(recover("drain", **claim, submit=False))["drain"] == "empty"
                        assert "drain_authorization" not in lookup and "authorization" not in lookup["recovery"]
                        assert read(recover("drain", **claim, submit=True, authorization=drain_authorization))["drain"] == "offered"
                        end = time.monotonic() + 5
                        while True:
                            decision = read(recover("drain", **claim, submit=False))["drain"]
                            if decision == "accepted":
                                break
                            assert decision == "pending" and time.monotonic() < end, decision
                            time.sleep(0.02)
                        # Both SDK collectors run before any managed GET can observe the
                        # terminal receipt, and neither has a handle to the managed store.
                        registry = root / "owners"
                        collectors = 0
                        while collectors < 2:
                            collected = subprocess.run(
                                [command[0], "doctor", "writer-rollback",
                                 "--registry", registry, "--registry-device", str(scope["device"]),
                                 "--registry-inode", str(scope["inode"]), "--confirm-launchers-drained"],
                                cwd=root, env=env, stdin=subprocess.DEVNULL,
                                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=8,
                            )
                            if collected.returncode == 0:
                                collectors += 1
                                continue
                            assert b"custody remains open" in collected.stderr and time.monotonic() < end, collected.stderr
                            time.sleep(0.02)
                        assert json.loads(record.read_bytes())["guardian_zombie_closure"] is True
                        # GuardianCommitStoppedDrain is witnessed by the slot decision above.
                        # ObserveStoppedClosure is a separate exact SDK observation through GET.
                        while True:
                            recovered = read(recover("get", **origin))
                            if recovered["custody"] == "closed":
                                break
                            assert time.monotonic() < end, recovered
                            time.sleep(0.02)
                        assert proc.poll() is None, "original broker was killed before closure"
                        assert recovered["execution"] == "unknown" and recovered["result"]["state"] == "pending"
                        assert read(recover("start", **origin)) == recovered
                        with server() as (second, read_second, _, _):
                            assert read_second(second("get", **origin)) == recovered
                        os.kill(proc.pid, signal.SIGCONT)
                        assert result(original_start) == recovered
                        assert len(bindings()) == 1
                        print("read-only and changed claims refused; competing collector preserved exact closure through broker resume")
                finally:
                    proc.kill()
                    proc.wait(timeout=5)
                return
            if args.abrupt:
                # The SQLite row witnesses consumption, not StartRecoveryTarget. For H0's
                # old-launch-first cut, stop at the real worker shim entry before any reply,
                # then kill the original receiver and keep that single start out of the host DB.
                end = time.monotonic() + 15
                with sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=1) as connection:
                    while True:
                        row = connection.execute(
                            "SELECT r.identity FROM stages s JOIN recovery_bindings r USING(epoch,nonce,stage) "
                            "WHERE s.gate=1 AND s.result_kind=0 AND s.resource_report<3"
                        ).fetchone()
                        if row:
                            break
                        assert time.monotonic() < end, "missed the in-flight stage"
                scope = json.loads(row[0])
                guardian = scope["guardian"]["pid"]
                worker = scope["worker"]["pid"]
                os.kill(guardian, signal.SIGSTOP)
                stopped.callback(os.kill, guardian, signal.SIGCONT)
                os.kill(worker, signal.SIGSTOP)
                proc.kill()
                proc.wait(timeout=5)
                original_binding = bindings()

                corrupt = root / "damaged.sqlite3"
                damaged = b"deliberately invalid managed store"
                with os.fdopen(os.open(corrupt, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600), "wb") as output:
                    output.write(damaged)
                record = root / "owners" / f"worker-{scope['generation']}" / "owner.json"
                original_owner = record.read_bytes()
                refused = subprocess.run(
                    [*command[:-1], corrupt], cwd=root, env=env,
                    stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=8,
                )
                assert refused.returncode != 0 and b"file is not a database" in refused.stderr, refused.stderr
                assert corrupt.read_bytes() == damaged and record.read_bytes() == original_owner
                os.kill(guardian, 0)
                os.kill(worker, 0)
                domains = json.loads(original_owner)["protected_domains"]
                assert domains, "original scope had no retained read locks"
                for domain in domains:
                    with open(domain["lock_path"], "rb") as lock:
                        try:
                            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                        except BlockingIOError:
                            continue
                    raise AssertionError("exclusive lock acquired while original custody was held")
                print("corrupt store refused with original live custody and read locks intact")
            else:
                end = time.monotonic() + 25
                while True:
                    observed = result(managed("get", **origin), poll=True)
                    if observed and observed["custody"] == "closed" and observed["result"]["state"] == "retained":
                        break
                    assert time.monotonic() < end, observed
                    time.sleep(0.04)
                saved = observed["result"]
                body = json.loads(bytes(saved["body"]))
                assert body["status"] == "found" and body["declaration"]["name"] == args.declaration, body
                assert hashlib.sha256(bytes(saved["body"])).digest() == bytes(saved["sha256"])
                original_binding = bindings()
                assert len(original_binding) == 1
                print(f"cold inspection found {args.declaration}; retained {len(saved['body'])} result bytes; SDK confirmed retirement")

        with server() as (managed, result, tool, proc):
            restarted = result(tool("lean_runtime", {"action": "status"}))["pool"]
            assert restarted["instance_id"] != before["instance_id"]
            recovered = result(managed("get", **origin))
            retry = result(managed("start", **origin))
            if args.abrupt:
                assert recovered["execution"] == "unknown" and recovered["custody"] == "open"
                assert recovered["result"]["state"] == "pending"
                assert recovered["intent"] == intent and "intent_sha256" not in recovered
                assert retry == {key: val for key, val in recovered.items() if key != "recovery"}
                other = dict(key=dict(key, nonce=list(os.urandom(16))), capability=list(os.urandom(32)))
                other_intent = json.loads(bytes(intent))
                other_intent.update(job=list(os.urandom(16)), declaration="second_ok")
                assert result(managed("create", intent=list(json.dumps(other_intent).encode()), **other))["execution"] == "admitted"
                completed = result(managed("start", **other))
                assert completed["custody"] == "closed" and completed["result"]["state"] == "retained"
                assert json.loads(bytes(completed["result"]["body"]))["declaration"]["name"] == "second_ok"
                assert result(managed("get", **origin)) == recovered
                two_bindings = bindings()
                assert len(two_bindings) == 2 and original_binding[0] in two_bindings
                stopped.close()
                end = time.monotonic() + 10
                while True:
                    settled = result(managed("get", **origin))
                    if settled["custody"] == "closed":
                        break
                    assert time.monotonic() < end, settled
                    time.sleep(0.04)
                assert settled["execution"] == "unknown" and settled["result"]["state"] == "pending"
                assert result(managed("start", **origin)) == settled
                assert bindings() == two_bindings
                print("replacement inspected second_ok with original custody held; original closed after guardian resumed without replay")
            else:
                assert recovered["result"] == saved and retry["result"] == saved
                # The independent caller journal test covers durable save before ACK.
                ack = dict(origin, result_id=saved["id"], result_digest=saved["sha256"])
                expired = result(managed("ack", **ack))
                assert expired["custody"] == "closed" and expired["result"]["state"] == "expired"
                assert expired["intent"] is None
                assert bytes(expired["intent_sha256"]) == hashlib.sha256(bytes(intent)).digest()
                assert result(managed("ack", **ack)) == expired
                assert result(managed("create", intent=intent, **origin)) == expired
                assert result(managed("start", **origin)) == expired
                result(managed("create", intent=intent + [32], **origin), error="intent_changed")
                with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as audit:
                    assert audit.execute(
                        "SELECT length(intent),length(compacted_intent_sha256),result_body IS NULL FROM stages"
                    ).fetchone() == (0, 32, 1)
                assert bindings() == original_binding
                print("restart returned saved result; exact ACK compacted both bodies and retry opened no worker")
            after = result(tool("lean_runtime", {"action": "status"}))["pool"]
            assert after["occupied_slots"] == 0


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("parent", "scratch", "project", "prepared", "workers", "writer"):
        parser.add_argument(f"--{name}", required=True, type=Path)
    parser.add_argument("--declaration", required=True)
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--abrupt", action="store_true")
    mode.add_argument("--stopped-owner", action="store_true")
    mode.add_argument("--fail-result-save", action="store_true")
    run_check(parser.parse_args())
