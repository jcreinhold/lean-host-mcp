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
import sys
import threading
import time


def run_check(args):
    scratch = args.scratch.resolve(strict=True)
    project = args.project.resolve(strict=True)
    prepared = args.prepared.read_text().strip() if args.prepared else None
    if prepared is not None:
        plan = json.loads(prepared)["plan"]
        assert Path(plan["project"]).resolve(strict=True) == project
    root = scratch / "managed"
    root.mkdir(mode=0o700)
    for name in ("config", "owners", "process", "tmp", "cache"):
        (root / name).mkdir(mode=0o700)
    assert not (project / "lean-host-mcp.toml").exists()
    db = root / "stages.sqlite3"
    entry_trace = args.entry_scratch.resolve(strict=True) / "inspection-entries" if args.entry_scratch else None
    if entry_trace:
        assert not entry_trace.exists(), "entry witness must start empty"
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
        *([] if prepared is None else ["--prepared-generation", prepared]),
        "--managed-native-store", db,
        *(["--managed-only"] if args.proof_position else []),
    ]

    @contextmanager
    def server(*, stop_before_arm=False, stop_after_claim=False):
        child_env = dict(env)
        if args.deadline_while_sql_locked:
            child_env["LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS"] = "5000"
        if args.abrupt and entry_trace:
            child_env["LEAN_RS_WORKER_GUARDIAN_DRAIN_SLOT"] = "1"
        if stop_before_arm:
            child_env.update(LEAN_HOST_MCP_TEST_STOP_BEFORE_ARM="1", LEAN_RS_WORKER_GUARDIAN_DRAIN_SLOT="1")
        if stop_after_claim:
            child_env["LEAN_HOST_MCP_TEST_STOP_AFTER_CLAIM"] = "1"
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

            def receive(ident, *, poll=False, error=None, allow_error=False):
                end = time.monotonic() + 25
                while True:
                    answer = replies.get(timeout=max(0, end - time.monotonic()))
                    if answer["id"] == ident:
                        if error is not None:
                            assert answer.get("error", {}).get("message") == error, answer
                            return None
                        if allow_error and "error" in answer:
                            return {"rpc_error": answer["error"]["message"]}
                        if poll and answer.get("error", {}).get("message") == "store_busy":
                            return None
                        assert "error" not in answer, answer
                        return answer["result"]
                    # A successful original START response is deliberately discarded.
                    assert "error" not in answer, answer

            def tool(name, arguments):
                return send("tools/call", dict(name=name, arguments=arguments))

            def result(ident, *, poll=False, error=None, allow_error=False):
                reply = receive(ident, poll=poll, error=error, allow_error=allow_error)
                if reply is None or "rpc_error" in reply:
                    return reply
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

    def resume_guardian(pid):
        try:
            os.kill(pid, signal.SIGCONT)
        except ProcessLookupError:
            pass

    def wait_stopped(pid, end):
        while True:
            state = subprocess.run(
                ["ps", "-p", str(pid), "-o", "stat="], capture_output=True, text=True,
            ).stdout.strip()
            if state.startswith("T"):
                return
            assert state and time.monotonic() < end, (pid, state)
            time.sleep(0.01)

    def assert_artifact_locks_held(record):
        domains = json.loads(record.read_bytes())["protected_domains"]
        assert domains, "original scope had no retained read locks"
        for domain in domains:
            with open(domain["lock_path"], "rb") as lock:
                try:
                    fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    continue
            raise AssertionError("exclusive lock acquired while original custody was held")

    def bindings():
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
            return conn.execute("SELECT registry_generation FROM recovery_bindings").fetchall()

    if args.root_architecture is not None:
        assert project.is_relative_to(scratch), "architecture control requires a disposable project inside scratch"
        operation = "lean_root_architecture_lint_v1"
        executable = project / ".lake/build/bin/runLinter"
        assert executable.resolve(strict=True) == executable
        expected = int(args.root_architecture == "fail")
        reference = subprocess.run(
            ["lake", "--no-build", "--no-cache", "exe", "runLinter", "--no-build", "Proofs"],
            cwd=project, env=env, capture_output=True, text=True, timeout=30,
        )
        assert reference.returncode == expected and args.expected_finding in reference.stdout, reference
        with server() as (managed, result, tool, _):
            discovery = result(managed("discover"))
            assert operation in discovery["operations"], discovery
            origin = dict(
                key=dict(store=discovery["store"], epoch=discovery["acceptance_epoch"],
                         nonce=list(os.urandom(16)), stage=1),
                capability=list(os.urandom(32)),
            )
            intent = list(json.dumps(dict(
                operation=operation, project=str(project), job=list(os.urandom(16)), revision=1,
            )).encode())
            admitted = result(managed("create", intent=intent, **origin))
            assert admitted["execution"] == "admitted", admitted
            for prebuilt in (executable, project / ".lake/build/lib/lean/Proofs.olean"):
                held = prebuilt.with_suffix(".held")
                assert not held.exists() and prebuilt.resolve(strict=True) == prebuilt
                prebuilt.rename(held)
                try:
                    result(managed("start", **origin), error="prebuilt_architecture_linter_or_root_unavailable")
                    assert result(managed("get", **origin)) == admitted
                finally:
                    held.rename(prebuilt)
            closed = result(managed("start", **origin))
            assert closed["custody"] == "closed" and closed["result"]["state"] == "retained", closed
            saved = closed["result"]
            body = json.loads(bytes(saved["body"]))
            assert body["exit_code"] == expected and body["verdict"] == args.root_architecture, body
            assert args.expected_finding in body["stdout"] and not body["output_truncated"], body
            lock_path = json.loads(subprocess.check_output(
                [args.parent, "writer-lock-path", project], cwd=project, env=env, timeout=10,
            ))["lock_path"]
            with open(lock_path, "rb") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                assert result(managed("get", **origin))["result"] == saved
                assert result(managed("start", **origin))["result"] == saved
        print(f"root architecture {args.root_architecture}: default imported finding, exact close and retained retry")
        return

    if args.proof_position:
        env["LEAN_HOST_MCP_MANAGED_READINESS_WRITER"] = str(args.writer.resolve(strict=True))
        source = project / "Positioned.lean"
        original = source.read_bytes()
        selector = dict(kind="after_text", text="skip", occurrence=1)
        operation = "lean_file_proof_position_v1"
        with server() as (managed, result, tool, _):
            discovery = result(managed("discover"))

            def admit():
                key = dict(store=discovery["store"], epoch=discovery["acceptance_epoch"],
                           nonce=list(os.urandom(16)), stage=1)
                origin = dict(key=key, capability=list(os.urandom(32)))
                intent = dict(operation=operation, project=str(project), job=list(os.urandom(16)), revision=1,
                              setup_verification=dict(file=source.name, sha256=list(hashlib.sha256(original).digest()),
                                                      proof_position=dict(declaration="positioned", position=selector)))
                admitted = result(managed("create", intent=list(json.dumps(intent).encode()), **origin))
                assert admitted["execution"] == "admitted", admitted
                return origin, admitted

            changed, pending = admit()
            try:
                source.write_bytes(original + b"\n-- changed after admission\n")
                result(managed("start", **changed), error="managed_proof_position_source_changed")
                assert result(managed("get", **changed)) == pending
            finally:
                source.write_bytes(original)

            origin, _ = admit()
            request = dict(kind="proof_position", file=source.name, declaration="positioned",
                           proof_position=selector, managed_original=origin)
            public = result(tool("lean_context", request))
            context = public["data"]
            assert context.get("goals_before") == context.get("goals_after") == ["⊢ True"], public
            assert not context.get("unavailable"), public
            different = dict(request, proof_position=dict(kind="index", index=0))
            refused = result(tool("lean_context", different), allow_error=True)
            assert "rpc_error" in refused or refused.get("errors"), refused
            saved = result(managed("get", **origin))
            assert saved["custody"] == "closed" and saved["result"]["state"] == "retained", saved
        with server() as (managed, result, _, _):
            assert result(managed("get", **origin)) == saved
        print("managed proof position: second occurrence selected the remaining goal; changed source/selector refused; original GET recovered")
        return

    with ExitStack() as stopped:
        with server(stop_before_arm=args.stopped_owner or bool(args.abrupt and entry_trace)) as (
            managed, result, tool, proc,
        ):
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
            if args.stopped_owner or (args.abrupt and entry_trace):
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
            if args.deadline_while_sql_locked:
                end = time.monotonic() + 5
                while not entry_trace.exists() or not entry_trace.read_text().endswith("\n"):
                    assert time.monotonic() < end, "missing actual Lean entry"
                    time.sleep(0.01)
                clock = time.CLOCK_MONOTONIC if sys.platform == "darwin" else time.CLOCK_BOOTTIME

                def state(pid):
                    return subprocess.run(
                        ["ps", "-p", str(pid), "-o", "stat="], capture_output=True, text=True,
                    ).stdout.strip()

                def assert_database_locked():
                    with sqlite3.connect(db, timeout=0) as contender:
                        try:
                            contender.execute("BEGIN IMMEDIATE")
                        except sqlite3.OperationalError as error:
                            assert error.sqlite_errorcode == sqlite3.SQLITE_BUSY
                        else:
                            raise AssertionError("competing SQLite writer acquired the lock")

                with sqlite3.connect(db, timeout=0) as holder:
                    holder.execute("BEGIN IMMEDIATE")
                    identity, expires_ns = holder.execute(
                        "SELECT identity,expires_ns FROM recovery_bindings"
                    ).fetchone()
                    scope = json.loads(identity)
                    assert scope["parent"]["pid"] == proc.pid
                    worker, guardian = (scope[role]["pid"] for role in ("worker", "guardian"))
                    expected_entry = f"{worker} {args.declaration}\n"
                    assert entry_trace.read_text() == expected_entry
                    record = root / "owners" / f"worker-{scope['generation']}" / "owner.json"
                    domains = json.loads(record.read_bytes())["protected_domains"]
                    assert_artifact_locks_held(record)
                    assert_database_locked()

                    os.kill(proc.pid, signal.SIGSTOP)
                    try:
                        wait_stopped(proc.pid, end)
                        worker_state = state(worker)
                        assert worker_state and not worker_state.startswith("Z")
                        assert time.clock_gettime_ns(clock) < expires_ns
                        guardian_state = state(guardian)
                        assert guardian_state and not guardian_state.startswith(("T", "Z"))
                        while True:
                            worker_state = state(worker)
                            observed_at = time.clock_gettime_ns(clock)
                            assert observed_at < expires_ns + 1_000_000_000, "guardian missed deadline"
                            if not worker_state or worker_state.startswith("Z"):
                                assert observed_at >= expires_ns, "worker exited before deadline"
                                break
                            time.sleep(0.01)
                        assert state(proc.pid).startswith("T")
                        assert_database_locked()
                        assert holder.execute("SELECT result_kind FROM stages").fetchone() == (0,)
                        assert not (entry_trace.parent / "release-inspection").exists()
                        assert_artifact_locks_held(record)
                    finally:
                        holder.rollback()
                        os.kill(proc.pid, signal.SIGCONT)

                settled = result(original_start)
                assert settled["custody"] == "closed" and settled["execution"] == "unknown", settled
                assert settled["result"]["state"] == "pending", settled
                assert result(managed("start", **origin)) == settled
                assert len(bindings()) == 1 and entry_trace.read_text() == expected_entry
                for domain in domains:
                    with open(domain["lock_path"], "rb") as lock:
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                lag_ms = (observed_at - expires_ns) / 1_000_000
                print(
                    f"observed real Lean stop {lag_ms:.1f} ms after the stored deadline "
                    "with broker stopped and SQLite locked"
                )
                print("result stayed pending; confirmed custody returned locks without replay")
                return
            if args.cancel_live:
                end = time.monotonic() + 15
                while not entry_trace.exists() or not entry_trace.read_text().endswith("\n"):
                    assert time.monotonic() < end, "missing actual Lean entry before cancel"
                    time.sleep(0.01)
                with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as observer:
                    identity, = observer.execute("SELECT identity FROM recovery_bindings").fetchone()
                scope = json.loads(identity)
                expected_entry = f"{scope['worker']['pid']} {args.declaration}\n"
                assert entry_trace.read_text() == expected_entry
                record = root / "owners" / f"worker-{scope['generation']}" / "owner.json"
                assert_artifact_locks_held(record)
                domains = json.loads(record.read_bytes())["protected_domains"]
                assert result(tool("lean_runtime", {"action": "status"}))["pool"]["occupied_slots"] == 1
                wrong_cap = list(cap)
                wrong_cap[0] ^= 1
                result(managed("cancel", **dict(origin, capability=wrong_cap)), error="capability_denied")
                stale = dict(origin, key=dict(key, nonce=list(os.urandom(16))))
                result(managed("cancel", **stale), error="unknown_stage")
                # The deferred constraint rejects COMMIT after a successful UPDATE.
                with sqlite3.connect(db) as fault:
                    fault.executescript(
                        "CREATE TABLE stop_fault_parent(id INTEGER PRIMARY KEY);"
                        "CREATE TABLE stop_fault_child(id INTEGER REFERENCES stop_fault_parent(id)"
                        " DEFERRABLE INITIALLY DEFERRED);"
                        "CREATE TRIGGER reject_stop AFTER UPDATE OF stop_requested ON stages"
                        " WHEN NEW.stop_requested=1"
                        " BEGIN INSERT INTO stop_fault_child(id) VALUES(1); END;"
                    )
                with sqlite3.connect(db) as control:
                    control.execute("PRAGMA foreign_keys=ON")
                    assert control.execute("UPDATE stages SET stop_requested=1").rowcount == 1
                    try:
                        control.commit()
                    except sqlite3.IntegrityError as error:
                        assert error.sqlite_errorcode == sqlite3.SQLITE_CONSTRAINT_FOREIGNKEY
                        control.rollback()
                    else:
                        raise AssertionError("deferred stop control COMMIT unexpectedly succeeded")
                result(managed("cancel", **origin), error="store_unavailable")
                pending = result(managed("get", **origin))
                assert pending["custody"] == "open" and not pending["cancel_requested"], pending
                assert_artifact_locks_held(record)
                with sqlite3.connect(db) as fault:
                    fault.execute("DROP TRIGGER reject_stop")

                cancelled_at = time.monotonic()
                accepted = result(managed("cancel", **origin))
                assert accepted["custody"] == "open" and accepted["cancel_requested"], accepted
                repeated = None
                while not repeated:
                    repeated = result(managed("cancel", **origin), poll=True)
                    assert time.monotonic() < cancelled_at + 10, "exact cancellation retry remained busy"
                assert repeated["execution"] == "unknown" and repeated["cancel_requested"], repeated
                while True:
                    settled = result(managed("get", **origin), poll=True)
                    assert time.monotonic() < cancelled_at + 10, "cancel did not retire before the Lean hold timeout"
                    if settled and settled["custody"] == "closed":
                        break
                    time.sleep(0.02)
                assert settled["custody"] == "closed" and settled["execution"] == "unknown", settled
                assert settled["result"]["state"] == "pending" and settled["cancel_requested"], settled
                assert not (entry_trace.parent / "release-inspection").exists()
                assert result(managed("cancel", **origin)) == settled
                end = time.monotonic() + 5
                while result(tool("lean_runtime", {"action": "status"}))["pool"]["occupied_slots"] != 0:
                    assert time.monotonic() < end, "confirmed closure did not return the permit"
                    time.sleep(0.02)
                for domain in domains:
                    with open(domain["lock_path"], "rb") as lock:
                        stat = os.fstat(lock.fileno())
                        assert (stat.st_dev, stat.st_ino) == (domain["device"], domain["inode"])
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                original_binding = bindings()
            elif args.stopped_owner:
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
                        claimant = list(os.urandom(32))
                        rival = list(os.urandom(32))
                        with server(stop_after_claim=True) as (offer, _, _, claimant_process):
                            offer("drain", **claim, submit=True, authorization=drain_authorization,
                                  claimant=claimant)
                            end = time.monotonic() + 5
                            with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as observer:
                                while True:
                                    claim_state = observer.execute(
                                        "SELECT recovery_claim_hash,gate,resource_report,result_kind FROM stages"
                                    ).fetchone()
                                    if claim_state[0] is not None:
                                        break
                                    assert time.monotonic() < end, claim_state[1:]
                                    time.sleep(0.01)
                            assert claim_state == (hashlib.sha256(bytes(claimant)).digest(), 1, 1, 0)
                            read(recover("drain", **claim, submit=True, authorization=drain_authorization,
                                         claimant=rival), error="recovery_claim_busy")
                            assert read(recover("drain", **claim, submit=False))["drain"] == "empty"
                            assert_artifact_locks_held(record)
                            claimant_process.kill()
                            assert claimant_process.wait(timeout=5) == -signal.SIGKILL
                        read(recover("drain", **claim, submit=True, authorization=drain_authorization,
                                     claimant=rival), error="recovery_claim_busy")
                        resumed = read(recover("drain", **claim, submit=True, authorization=drain_authorization,
                                               claimant=claimant))
                        assert resumed["drain"] == "offered" and resumed["claim"] == "same"
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
                        if entry_trace:
                            assert not entry_trace.exists(), "fenced original entered the Lean target"
                            print("observed zero actual Lean target entries through closure and original retry")
                        print("winning claim survived claimant death; rival refused and original locks held until guardian closure")
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
                if entry_trace:
                    wait_stopped(proc.pid, end)
                    assert not entry_trace.exists(), "Lean entered before recovery arbitration"
                    with server(stop_after_claim=True) as (claim_a, reply_a, _, winner), server() as (
                        claim_b, reply_b, _, _,
                    ):
                        exact = dict(origin, generation=scope["generation"], revision=1, submit=True,
                                     authorization=drain_authorization)
                        claimant = os.urandom(32)
                        offer_a = claim_a("drain", **exact, claimant=list(claimant))
                        wait_stopped(winner.pid, end)
                        with sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=1) as observer:
                            assert observer.execute(
                                "SELECT recovery_claim_hash FROM stages"
                            ).fetchone() == (hashlib.sha256(claimant).digest(),)
                        rival = claim_b("drain", **exact, claimant=list(os.urandom(32)))
                        reply_b(rival, error="recovery_claim_busy")
                        assert not entry_trace.exists(), "Lean entered during recovery arbitration"
                        os.kill(proc.pid, signal.SIGCONT)
                        expected_entry = f"{worker} {args.declaration}\n"
                        while not entry_trace.exists() or entry_trace.read_text() != expected_entry:
                            assert time.monotonic() < end, "missing exact Lean target entry"
                            time.sleep(0.01)
                        os.kill(proc.pid, signal.SIGSTOP)
                        wait_stopped(proc.pid, end)
                        os.kill(guardian, signal.SIGSTOP)
                        stopped.callback(resume_guardian, guardian)
                        wait_stopped(guardian, end)
                        os.kill(winner.pid, signal.SIGCONT)
                        decision = reply_a(offer_a)
                        assert decision["claim"] == "won" and decision["drain"] == "offered", decision
                else:
                    os.kill(guardian, signal.SIGSTOP)
                    stopped.callback(resume_guardian, guardian)
                os.kill(worker, signal.SIGSTOP)
                if entry_trace:
                    wait_stopped(worker, end)
                proc.kill()
                proc.wait(timeout=5)
                original_binding = bindings()
                record = root / "owners" / f"worker-{scope['generation']}" / "owner.json"
                assert_artifact_locks_held(record)

                if not entry_trace:
                    corrupt = root / "damaged.sqlite3"
                    damaged = b"deliberately invalid managed store"
                    with os.fdopen(os.open(corrupt, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600), "wb") as output:
                        output.write(damaged)
                    original_owner = record.read_bytes()
                    refused = subprocess.run(
                        [*command[:-1], corrupt], cwd=root, env=env,
                        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=8,
                    )
                    assert refused.returncode != 0 and b"file is not a database" in refused.stderr, refused.stderr
                    assert corrupt.read_bytes() == damaged and record.read_bytes() == original_owner
                    os.kill(guardian, 0)
                    os.kill(worker, 0)
                    assert_artifact_locks_held(record)
                    print("corrupt store refused with original live custody and read locks intact")
            elif not args.cancel_live:
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
            if args.cancel_live:
                assert recovered == retry == settled
                assert result(managed("cancel", **origin)) == settled
                assert bindings() == original_binding and len(original_binding) == 1
                assert entry_trace.read_text() == expected_entry
                print("live managed cancel refused wrong and uncommitted stops, then closed one actual Lean entry without replay")
            elif args.abrupt:
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
                assert_artifact_locks_held(record)
                two_bindings = bindings()
                assert len(two_bindings) == 2 and original_binding[0] in two_bindings
                if entry_trace:
                    # Killing the worker can auto-resume its now-orphaned guardian group.
                    # Keep custody frozen through the unrelated-work check first.
                    os.kill(worker, signal.SIGKILL)
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
                if entry_trace:
                    assert entry_trace.read_text() == expected_entry
                    print("observed exactly one Lean target entry before receiver death and none on recovery")
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
    for name in ("parent", "scratch", "project", "workers", "writer"):
        parser.add_argument(f"--{name}", required=True, type=Path)
    parser.add_argument("--prepared", type=Path)
    parser.add_argument("--declaration")
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--abrupt", action="store_true")
    mode.add_argument("--stopped-owner", action="store_true")
    mode.add_argument("--fail-result-save", action="store_true")
    mode.add_argument("--cancel-live", action="store_true")
    mode.add_argument("--deadline-while-sql-locked", action="store_true")
    mode.add_argument("--proof-position", action="store_true")
    mode.add_argument("--root-architecture", choices=("pass", "fail"))
    parser.add_argument("--expected-finding")
    parser.add_argument("--entry-scratch", type=Path)
    args = parser.parse_args()
    if args.root_architecture is not None:
        if not args.expected_finding or args.prepared or args.declaration:
            parser.error("root architecture requires --expected-finding and no prepared inspection input")
    elif not args.proof_position and (not args.prepared or not args.declaration or args.expected_finding):
        parser.error("inspection requires --prepared and --declaration")

    if args.entry_scratch and not (args.stopped_owner or args.abrupt or args.cancel_live or args.deadline_while_sql_locked):
        parser.error("--entry-scratch requires a stopped-owner, abrupt, live-cancel, or deadline case")
    if (args.cancel_live or args.deadline_while_sql_locked) and not args.entry_scratch:
        parser.error("live-cancel and deadline cases require --entry-scratch")
    run_check(args)
