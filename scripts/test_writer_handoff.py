#!/usr/bin/env python3
"""Qualify exact parent/worker images with private real Lean readers and writers.

This opt-in harness creates and retains private projects, owner records, logs and
single-call receipts. It never connects to the production port, calls a drain,
resubmits a Lean operation, or removes an artifact/demand lock. Supply the exact
frozen binaries and their SHA-256 values. A failed run is evidence, not a retry
instruction. Existing unit/CLI suites separately exercise failed-kill custody.
"""
from __future__ import annotations

import argparse
import concurrent.futures
import datetime
import fcntl
import hashlib
import http.client
import json
import os
import queue
import re
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
from pathlib import Path

LIMIT = 20.0
CAPACITY = 4
PROOF = "example (n : Nat) : n + 0 = n := Nat.add_zero n"
HEADER_DIGEST = "eb951e1171d39b828e7ad60aed0b90759598b678bfe58cf85c65a1559c021bf9"
DEVELOPMENT_PROVENANCE_WARNING = (
    "worker for v4.35.0 has no provenance record (installed by an older host); "
    "reinstall to enable header-drift detection: lean-host-mcp install-worker --toolchain v4.35.0"
)


def utc():
    return datetime.datetime.now(datetime.UTC).isoformat()


def require(condition, detail):
    if not condition:
        raise AssertionError(detail)


def wait_for(predicate, detail, timeout=LIMIT):
    deadline = time.monotonic() + timeout
    while not predicate():
        if time.monotonic() >= deadline:
            raise TimeoutError(detail)
        time.sleep(0.01)


class Image:
    def __init__(self, path, expected):
        self.path = Path(path)
        self.expected = expected
        require(re.fullmatch(r"[0-9a-f]{64}", expected), "lowercase SHA-256 required")
        self.verify()

    def verify(self):
        require(self.path.is_absolute() and self.path.resolve(strict=True) == self.path,
                f"canonical absolute image required: {self.path}")
        require(self.path.is_file() and os.access(self.path, os.X_OK), "executable image required")
        require(hashlib.sha256(self.path.read_bytes()).hexdigest() == self.expected,
                f"image bytes differ: {self.path}")


class Evidence:
    def __init__(self, root):
        self.root = root
        self.lock = threading.RLock()
        self.data = {"started_at": utc(), "passed": False, "events": [], "cases": []}
        self.save()

    def save(self):
        with self.lock:
            temporary = self.root / "report.new"
            temporary.write_text(json.dumps(self.data, indent=2) + "\n")
            temporary.replace(self.root / "report.json")

    def update(self, **facts):
        with self.lock:
            self.data.update(facts)
            self.save()

    def update_item(self, key, **facts):
        with self.lock:
            self.data[key].update(facts)
            self.save()

    def item(self, key):
        with self.lock:
            return json.loads(json.dumps(self.data.get(key)))

    def start(self, kind, arguments):
        with self.lock:
            event = {"kind": kind, "arguments": arguments, "intent_at": utc(),
                     "ordinal": len(self.data["events"]) + 1}
            self.data["events"].append(event)
            self.save()
            return event

    def finish(self, event, **facts):
        with self.lock:
            event.update(facts, returned_at=utc())
            self.save()

    def case(self, name, **facts):
        with self.lock:
            self.data["cases"].append({"name": name, "passed": True, **facts})
            self.save()


def unwrap(raw):
    require(not raw.get("isError"), raw)
    if isinstance(raw.get("structuredContent"), dict):
        return raw["structuredContent"]
    return json.loads(next(row["text"] for row in raw["content"] if row.get("type") == "text"))


def trial_receipt(payload, project):
    # Bare developer images have no installer sidecar. Accept precisely that
    # known warning; the frozen byte hashes and actual loaded ABI/header/runtime
    # receipts below remain mandatory. Never manufacture provenance metadata.
    for error in payload["errors"]:
        require(error == {"severity": "warning", "code": "warning",
                          "message": DEVELOPMENT_PROVENANCE_WARNING}, error)
    require(payload["data"]["diagnostics"]["summary"]["errors"] == 0, payload)
    compatibility = payload["trust"]["native_compatibility"]
    expected = compatibility["expected"]["required_host_bindings"]
    require(len(expected) == 3, "compiled expected ABI contract must contain all three bindings")
    loaded = compatibility["loaded"]
    require(loaded, "actual loaded compatibility required")
    for row in loaded:
        require(row["project_root"] == str(project) and row["protocol_version"] == 13, row)
        facts = row["loaded"]
        require(facts["compiled_header_sha256"] == HEADER_DIGEST, facts)
        require(facts["compiled_lean_version"] == "4.35.0" and facts["runtime_lean_version"] == "4.35.0", facts)
        require(facts["checked_host_bindings"].items() >= expected.items(), facts)
        require(any(epoch["project_root"] == str(project) and epoch["epoch"]
                    for epoch in facts["artifact_read_epochs"]), facts)
    return [(row["worker_session_id"], row["worker_generation"]) for row in loaded]


class Client:
    def __init__(self, url, evidence):
        self.url, self.evidence = url, evidence
        self.session, self.next_id = None, 1
        self.send("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                 "clientInfo": {"name": "writer-handoff-qualification", "version": "1"}})
        self.send("notifications/initialized", notification=True)

    def send(self, method, params=None, notification=False):
        value = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            value["params"] = params
        if not notification:
            value["id"] = self.next_id
            self.next_id += 1
        headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}
        if self.session:
            headers["Mcp-Session-Id"] = self.session
        endpoint = urllib.parse.urlsplit(self.url)
        require(endpoint.hostname == "127.0.0.1" and endpoint.port != 8765, "private loopback endpoint required")
        event = self.evidence.start("HTTP " + method, value)
        wire = self.evidence.root / f"http-{event['ordinal']}.body"
        raw = bytearray()
        deadline = time.monotonic() + 40
        connection = http.client.HTTPConnection(endpoint.hostname, endpoint.port, timeout=40)
        timer = None
        try:
            connection.connect()
            owned_socket = connection.sock
            def close_waiter():
                # Only this call's socket. A local timeout does not replay or
                # certify termination of any accepted server operation.
                try:
                    owned_socket.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
            timer = threading.Timer(max(0, deadline - time.monotonic()), close_waiter)
            timer.daemon = True
            timer.start()
            connection.request("POST", endpoint.path, json.dumps(value).encode(), headers)
            response = connection.getresponse()
            if response.getheader("Mcp-Session-Id"):
                self.session = response.getheader("Mcp-Session-Id")
            while True:
                require(time.monotonic() < deadline, "absolute HTTP call deadline elapsed")
                block = response.read1(64 * 1024)
                if not block:
                    break
                raw.extend(block)
                require(len(raw) <= 32 * 1024 * 1024, "bounded HTTP reply exceeded")
            wire.write_bytes(raw)
            self.evidence.finish(event, status=response.status, wire_reply=str(wire))
            require(200 <= response.status < 300, f"HTTP {response.status}; raw reply {wire}")
            if not raw.strip():
                return None
            if raw.lstrip().startswith(b"{"):
                result = json.loads(raw)
            else:
                messages = [json.loads(line[5:].strip()) for line in raw.splitlines()
                            if line.startswith(b"data:") and line[5:].strip()]
                result = next(item for item in messages if item.get("id") == value.get("id"))
            require("error" not in result, result)
            return result.get("result")
        except BaseException as error:
            wire.write_bytes(raw)
            self.evidence.finish(event, error=repr(error), wire_reply=str(wire))
            raise
        finally:
            if timer is not None:
                timer.cancel()
            connection.close()

    def tool(self, name, arguments):
        event = self.evidence.start("MCP " + name, arguments)
        started = time.monotonic()
        try:
            raw = self.send("tools/call", {"name": name, "arguments": arguments})
            self.evidence.finish(event, raw=raw, seconds=time.monotonic() - started)
            payload = unwrap(raw)
        except BaseException as error:
            self.evidence.finish(event, error=repr(error), seconds=time.monotonic() - started)
            raise
        self.evidence.finish(event, raw=raw, payload=payload, seconds=time.monotonic() - started)
        return payload

    def trial(self, project, commands=PROOF):
        payload = self.tool("lean_trial", {"kind": "command", "project": str(project),
                                          "imports": ["Init"], "commands": commands})
        return trial_receipt(payload, project)


def project(root, name):
    path = root / name
    path.mkdir()
    (path / "lean-toolchain").write_text("leanprover/lean4:v4.35.0\n")
    (path / "lakefile.toml").write_text(f'name = "{name.lower()}"\nversion = "0.1.0"\n')
    (path / "lake-manifest.json").write_text('{"version":"1.1.0","packagesDir":".lake/packages","packages":[]}\n')
    return path


class Writer:
    def __init__(self, harness, project, name):
        self.harness, self.project, self.name = harness, project, name
        harness.verify()
        descriptor = json.loads(subprocess.check_output(
            [str(harness.parent.path), "writer-lock-path", str(project)], env=harness.env, timeout=5))
        require(descriptor["project_root"] == str(project) and descriptor["protocol"] == 1, descriptor)
        self.lock_path = Path(descriptor["lock_path"])
        self.event = harness.evidence.start("writer", {"project": str(project), "name": name})
        self.stdout = (harness.root / (name + ".stdout")).open("wb")
        self.stderr = (harness.root / (name + ".stderr")).open("wb")
        try:
            self.process = subprocess.Popen([str(harness.parent.path), "writer", str(project),
                                             descriptor["lock_path"], "10"], env=harness.env,
                                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr)
        except BaseException:
            self.stdout.close()
            self.stderr.close()
            raise
        self.frames = queue.Queue()
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.closed = False
        harness.writers.append(self)
        self.reader.start()

    def read(self):
        try:
            while True:
                line = self.process.stdout.readline(1024 * 1024 + 1)
                if not line:
                    break
                self.stdout.write(line)
                self.stdout.flush()
                if len(line) > 1024 * 1024 or not line.endswith(b"\n"):
                    self.frames.put(RuntimeError("bounded writer frame invalid"))
                    break
                self.frames.put(json.loads(line))
        except (OSError, ValueError) as error:
            self.frames.put(error)
        finally:
            self.frames.put(None)

    def frame(self):
        value = self.frames.get(timeout=LIMIT)
        if isinstance(value, BaseException):
            raise value
        require(value is not None, f"writer exited without expected frame: {self.name}")
        return value

    def locked(self):
        require(self.frame()["state"] == "locked", "writer admission receipt required")

    def wait_demand(self):
        # Test-only observation of the private stable sibling. No descriptor is
        # inherited and no file is created/unlinked; actual artifact EX remains
        # the authority. The active call already holds its ordinary read scope.
        demand = self.lock_path.with_name(self.lock_path.stem + "-demand.lock")
        def pending():
            if not demand.exists():
                return False
            with demand.open("rb") as file:
                try:
                    fcntl.flock(file, fcntl.LOCK_EX | fcntl.LOCK_NB)
                except BlockingIOError:
                    try:
                        fcntl.flock(file, fcntl.LOCK_SH | fcntl.LOCK_NB)
                    except BlockingIOError:
                        return False  # A transient reader EX gate, not writer SH.
                    return True
                return False
        wait_for(pending, "writer demand must be held before testing new admission", timeout=5)

    def run(self, command):
        self.harness.verify()
        self.process.stdin.write(json.dumps({"action": "run", "command": command, "timeout": 5.0}).encode() + b"\n")
        self.process.stdin.flush()
        require(self.frame()["state"] == "started", "actual writer start receipt required")

    def stopped(self, usable=True):
        frame = self.frame()
        require(frame["state"] == "stopped" and frame["owned_work_stopped"] is True, frame)
        require(frame["result"] == ("result" if usable else "no_result"), frame)
        if usable:
            require(frame["exit_code"] == 0, frame)
        return frame

    def marker(self, filename):
        self.run([str(self.harness.python), "-c", "from pathlib import Path;import sys;Path(sys.argv[1]).open('a').write('once\\n')",
                  str(filename)])
        return self.stopped()

    def close_input(self):
        if self.process.stdin is not None:
            self.process.stdin.close()
            self.process.stdin = None

    def close(self):
        if self.closed:
            return
        self.close_input()
        code = self.process.wait(timeout=LIMIT)
        if self.reader.ident is not None:
            self.reader.join(timeout=2)
            require(not self.reader.is_alive(), "writer stdout did not close after exit")
        else:
            self.process.stdout.close()
        self.stdout.close()
        self.stderr.close()
        self.closed = True
        self.harness.evidence.finish(self.event, exit_code=code)
        self.harness.verify()
        require(code == 0, f"writer helper failed: {self.name}, exit {code}")


class Harness:
    def __init__(self, args):
        self.parent, self.worker = Image(args.parent, args.parent_sha256), Image(args.worker, args.worker_sha256)
        require(self.worker.path.name == "lean-host-mcp-worker", "developer worker layout requires exact basename")
        if args.evidence_dir:
            self.root = Path(args.evidence_dir)
            require(self.root.is_absolute() and self.root.parent.resolve(strict=True) == self.root.parent,
                    "canonical evidence parent required")
            self.root.mkdir(mode=0o700)
        else:
            self.root = Path(tempfile.mkdtemp(prefix="writer-handoff-", dir="/tmp")).resolve(strict=True)
        self.evidence = Evidence(self.root)
        self.evidence.update(images={"parent": {"path": str(self.parent.path), "sha256": self.parent.expected},
                                     "worker": {"path": str(self.worker.path), "sha256": self.worker.expected}},
                             limits=["macOS native completion qualification; same inherited process group",
                                     "failed-kill/passive-fence cases remain covered by separate real-child suites"])
        self.writers = []
        self.server = None
        self.server_log = None
        self.children = []
        self.futures = []
        self.a, self.b = project(self.root, "ProjectA"), project(self.root, "ProjectB")
        self.python = Path(sys.executable).resolve(strict=True)
        self.env = dict(os.environ)
        for name in tuple(self.env):
            if name.startswith(("LEAN_HOST_MCP_", "LEAN_RS_WORKER_", "LEAN_RS_NATIVE_GUARDIAN_")):
                self.env.pop(name)
        self.env.update(LEAN_HOST_MCP_CONFIG_DIR=str(self.root / "config"),
                        LEAN_HOST_MCP_PROCESS_REGISTRY_DIR=str(self.root / "server-processes"),
                        LEAN_HOST_MCP_WORKERS_DIR=str(self.worker.path.parent),
                        LEAN_HOST_MCP_MAX_WORKERS=str(CAPACITY), LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT="1",
                        LEAN_HOST_MCP_IDLE_TIMEOUT_SECS="0", LEAN_HOST_MCP_REQUEST_TIMEOUT_MILLIS="30000",
                        LEAN_RS_WORKER_OWNERS_DIRECTORY=str(self.root / "owners"))

    def start(self):
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            self.port = listener.getsockname()[1]
        require(self.port != 8765, "production port is excluded")
        self.url = f"http://127.0.0.1:{self.port}/mcp"
        self.server_log = (self.root / "server.log").open("wb")
        self.server = subprocess.Popen([str(self.parent.path), "serve", "--lake-root", str(self.a),
                                        "--bind", f"127.0.0.1:{self.port}", "--request-journal", str(self.root / "journal")],
                                       cwd=self.root, env=self.env, stdin=subprocess.DEVNULL,
                                       stdout=self.server_log, stderr=subprocess.STDOUT)
        self.evidence.update(server={"pid": self.server.pid, "port": self.port, "idle_timeout_secs": 0})
        def ready():
            require(self.server.poll() is None, "private server exited before readiness")
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=0.1):
                    return True
            except OSError:
                return False
        wait_for(ready, "private server readiness")
        self.client = self.new_client()

    def verify(self):
        self.parent.verify()
        self.worker.verify()

    def new_client(self):
        return Client(self.url, self.evidence)

    def submit(self, function):
        future = concurrent.futures.Future()
        self.futures.append(future)
        def invoke():
            if future.set_running_or_notify_cancel():
                try:
                    future.set_result(function())
                except (
                    AssertionError,
                    AttributeError,
                    http.client.HTTPException,
                    LookupError,
                    OSError,
                    RuntimeError,
                    StopIteration,
                    subprocess.SubprocessError,
                    TypeError,
                    ValueError,
                ) as error:
                    future.set_exception(error)
                except BaseException as error:
                    # Unblock result waiters without suppressing thread termination.
                    future.set_exception(error)
                    raise

        thread = threading.Thread(target=invoke, daemon=True)
        try:
            thread.start()
        except BaseException as error:
            future.set_exception(error)
            raise
        return future

    def submit_trial(self, project, commands=PROOF):
        return self.submit(lambda: self.new_client().trial(project, commands))

    def status(self):
        return self.client.tool("lean_runtime", {"action": "status"})

    def concurrent_readers(self):
        barrier = threading.Barrier(3)
        release = self.root / "readers.release"
        payload = "from pathlib import Path;import sys,time;r=Path(sys.argv[1]);Path(sys.argv[2]).write_text('started');d=time.monotonic()+8\nwhile not r.exists():\n if time.monotonic()>d:raise SystemExit(2)\n time.sleep(.01)\n"
        def reader(ordinal):
            self.verify()
            marker = self.root / f"reader-{ordinal}.started"
            command = [str(self.parent.path), "native-reader", "--project-root", str(self.a),
                       "--capacity-limit", str(CAPACITY), "--timeout-millis", "10000", "--",
                       str(self.python), "-c", payload, str(release), str(marker)]
            event = self.evidence.start("simultaneous cold native reader", {"ordinal": ordinal, "command": command})
            barrier.wait(timeout=LIMIT)
            try:
                result = subprocess.run(command, env=self.env, stdin=subprocess.DEVNULL,
                                        capture_output=True, timeout=15, check=False)
            except subprocess.TimeoutExpired as error:
                (self.root / f"reader-{ordinal}.stdout").write_bytes(error.output or b"")
                (self.root / f"reader-{ordinal}.stderr").write_bytes(error.stderr or b"")
                self.evidence.finish(event, error=repr(error), timeout=True)
                raise
            (self.root / f"reader-{ordinal}.stdout").write_bytes(result.stdout)
            (self.root / f"reader-{ordinal}.stderr").write_bytes(result.stderr)
            self.evidence.finish(event, returncode=result.returncode)
            reply = json.loads(result.stdout)
            self.evidence.finish(event, returncode=result.returncode, reply=reply)
            require(result.returncode == 0 and reply["state"] == "completed" and reply["scope_closed"] is True,
                    reply)
            require(reply["target_started"] is True and reply["exit_code"] == 0 and reply["intervened"] is False,
                    reply)
            self.verify()
        futures = [self.submit(lambda ordinal=ordinal: reader(ordinal)) for ordinal in (1, 2)]
        barrier.wait(timeout=LIMIT)
        wait_for(lambda: all((self.root / f"reader-{n}.started").exists() for n in (1, 2)),
                 "both simultaneous readers must start without writer demand", timeout=6)
        release.write_text("release")
        for future in futures:
            future.result(timeout=LIMIT)
        self.evidence.case("two simultaneous cold native readers complete without writer demand")

    def cases(self):
        self.concurrent_readers()
        before_a = self.client.trial(self.a)
        before_b = self.client.trial(self.b)
        runtime = self.status()
        require(runtime["effective_settings"]["broker"]["idle_timeout_secs"] == 0, runtime)
        started = time.monotonic()
        writer = Writer(self, self.a, "warm-writer")
        writer.locked()
        require(time.monotonic() - started < 6, "idle worker did not automatically hand off")
        marker = self.a / "warm-writer.marker"
        writer.marker(marker)
        writer.close()
        require(marker.read_text() == "once\n", "writer body must run exactly once")
        after_a = self.client.trial(self.a)
        after_b = self.client.trial(self.b)
        require(after_a != before_a, "affected warm actor was not retired")
        require(after_b == before_b, "unrelated warm actor was needlessly retired")
        self.evidence.case("warm worker automatically hands off to external writer", seconds=time.monotonic() - started)

        started_marker, finished_marker = self.a / "active.started", self.a / "active.finished"
        active_release = self.a / "active.release"
        active_command = ("#eval do\n  IO.FS.writeFile " + json.dumps(str(started_marker)) + ' "started"\n'
                          "  let mut waiting := true\n  let mut remaining : Nat := 1000\n"
                          "  while waiting do\n    if remaining == 0 then\n"
                          '      throw <| IO.userError "qualification release deadline"\n'
                          "    remaining := remaining - 1\n    try\n      discard <| IO.FS.readFile "
                          + json.dumps(str(active_release)) + "\n      waiting := false\n"
                          "    catch _ => IO.sleep 10\n"
                          "  IO.FS.writeFile " + json.dumps(str(finished_marker)) + ' "finished"\n')
        active = self.submit_trial(self.a, active_command)
        wait_for(started_marker.exists, "active Lean command start sentinel")
        writer = Writer(self, self.a, "active-writer")
        writer.wait_demand()
        matching_marker = self.a / "matching.started"
        matching_arguments = {"kind": "command", "project": str(self.a), "imports": ["Init"],
                              "commands": "#eval IO.FS.writeFile " + json.dumps(str(matching_marker)) + ' "started"'}
        prepared = self.client.tool("lean_request", {"action": "prepare", "tool": "lean_trial",
                                                     "arguments": matching_arguments})
        require(prepared["execution"] == {"disposition": "pending", "reason": "prepared"}, prepared)
        request_id, binding = prepared["taskId"], prepared["request"]
        identity = {"request_id": request_id, "request_epoch": binding["request_epoch"]}
        self.evidence.update(matching_retained_request={"id": request_id, "binding": binding,
                                                       "single_submission_intent": True})
        self.client.tool("lean_request", {"action": "submit", **identity, "tool": "lean_trial",
                                          "arguments": matching_arguments})
        working = self.client.tool("lean_request", {"action": "get", **identity})
        require(working["status"] == "working" and working["request"] == binding, working)
        require(not matching_marker.exists(), "accepted matching body overtook writer demand")
        require(self.client.trial(self.b) == after_b, "unrelated proof did not keep its warm actor")
        require(not active.done() and not finished_marker.exists(), "controlled active call did not wait for release")
        working = self.client.tool("lean_request", {"action": "get", **identity})
        require(working["status"] == "working" and not matching_marker.exists(), working)
        active_release.write_text("release")
        writer.locked()
        require(finished_marker.exists(), "writer overtook the active Lean body")
        require(active.result(timeout=LIMIT) == after_a, "active call did not retain its original generation")
        working = self.client.tool("lean_request", {"action": "get", **identity})
        require(working["status"] == "working" and not matching_marker.exists(), "matching body ran beneath writer EX")
        writer.marker(self.a / "active-writer.marker")
        writer.close()
        deadline = time.monotonic() + LIMIT
        while True:
            result = self.client.tool("lean_request", {"action": "get", **identity})
            require(result["taskId"] == request_id and result["request"] == binding, result)
            if result["status"] != "working":
                break
            require(time.monotonic() < deadline, "retrieve original retained ticket; do not submit again")
            time.sleep(0.05)
        require(result["status"] == "completed" and result["result_available"], result)
        require(result["execution"]["disposition"] != "unknown", result)
        payload = unwrap(result["result"])
        successor = trial_receipt(payload, self.a)
        require(matching_marker.exists() and successor and successor != after_a,
                "successor did not run after actual closure")
        # The complete terminal result is already saved before the sole own ACK.
        acknowledged = self.client.tool("lean_request", {"action": "acknowledge", **identity,
                                                          "submission_sha256": binding["submission_sha256"]})
        require(acknowledged["acknowledged"], acknowledged)
        self.evidence.update_item("matching_retained_request", acknowledged=True)
        self.evidence.case("active call finishes; server-accepted matching request waits; unrelated proof continues; successor runs")

        first = Writer(self, self.a, "concurrent-writer-1")
        first.locked()
        second = Writer(self, self.a, "concurrent-writer-2")
        require(second.process.poll() is None, "second writer could not wait with compatible demand")
        first.marker(self.a / "concurrent-writer-1.marker")
        first.close()
        second.locked()
        second.marker(self.a / "concurrent-writer-2.marker")
        second.close()
        self.client.trial(self.a)
        require((self.a / "concurrent-writer-1.marker").read_text() == "once\n", "first writer body count")
        require((self.a / "concurrent-writer-2.marker").read_text() == "once\n", "second writer body count")
        self.evidence.case("overlapping writer helper lifetimes serialize bodies and successor reader completes",
                           scope="pre-EX simultaneous SH demand rendezvous is separately covered by SDK real-child tests")

        runtime = self.status()
        require(runtime["request_registry"]["active"] == 0 and runtime["pool"]["outstanding_jobs"] == 0, runtime)
        require(runtime["pool"]["admission_paused"] is False, "manual maintenance was not used")
        self.verify()
        self.evidence.update(passed=True, finished_at=utc())

    def close(self):
        errors = []
        for writer in self.writers:
            try:
                writer.close()
            except (
                AssertionError,
                OSError,
                subprocess.TimeoutExpired,
                ValueError,
            ) as error:
                errors.append(repr(error))
        _, pending = concurrent.futures.wait(self.futures, timeout=45)
        retained_pending = False
        retained = self.evidence.item("matching_retained_request")
        if not pending and retained and not retained.get("acknowledged") and self.server and self.server.poll() is None:
            # Retrieving an already issued ticket is safe after a lost response.
            # No resubmit, result acceptance or unverified acknowledgement here.
            identity = {"request_id": retained["id"], "request_epoch": retained["binding"]["request_epoch"]}
            deadline = time.monotonic() + 35
            try:
                while True:
                    terminal = self.client.tool("lean_request", {"action": "get", **identity})
                    self.evidence.update(cleanup_retained_observation=terminal)
                    if terminal["status"] != "working":
                        if terminal.get("execution", {}).get("disposition") == "unknown":
                            retained_pending = True
                        break
                    if time.monotonic() >= deadline:
                        retained_pending = True
                        break
                    time.sleep(0.1)
            except (
                AssertionError,
                AttributeError,
                http.client.HTTPException,
                LookupError,
                OSError,
                StopIteration,
                TypeError,
                ValueError,
            ) as error:
                retained_pending = True
                errors.append("retained cleanup observation: " + repr(error))
        if pending or retained_pending:
            errors.append("private calls remain unresolved; server kept alive, no resubmission or forced signal")
        if not pending and not retained_pending and self.server is not None and self.server.poll() is None:
            self.server.send_signal(signal.SIGINT)
            try:
                self.server.wait(timeout=LIMIT)
            except subprocess.TimeoutExpired:
                errors.append(f"owned private server {self.server.pid} did not stop; no forced signal used")
        if self.server is not None and self.server.poll() is not None:
            # Closed ownership records intentionally remain until future GC.
            # Observe their private groups without signalling or deleting them.
            observations = []
            def private_scopes_closed():
                nonlocal observations
                listing = subprocess.run(["/bin/ps", "-axo", "pid=,pgid="], check=True,
                                         capture_output=True, text=True, timeout=2)
                rows = [tuple(map(int, line.split())) for line in listing.stdout.splitlines() if line.strip()]
                pids, groups = {row[0] for row in rows}, {row[1] for row in rows}
                observations = []
                for path in sorted((self.root / "owners").glob("*/owner.json")):
                    raw = path.read_bytes()
                    require(len(raw) <= 128 * 1024, "bounded private owner record exceeded")
                    value = json.loads(raw)
                    phase = value.get("phase")
                    if phase == "preparing_closed":
                        live = False
                    elif phase == "preparing":
                        live = value["parent"]["pid"] in pids
                    else:
                        require(phase == "running", "unknown private native owner phase")
                        body = value["process"]["process_group"]
                        guardian = value["guardian"]["process_group"]
                        require(isinstance(body, int) and body > 1 and isinstance(guardian, int) and guardian > 1,
                                "invalid private native group")
                        live = body in groups or guardian in groups
                    observations.append({"record": str(path), "sha256": hashlib.sha256(raw).hexdigest(),
                                         "phase": phase, "physical_scope_present": live})
                return all(not row["physical_scope_present"] for row in observations)
            try:
                wait_for(private_scopes_closed, "private native groups did not stop", timeout=5)
            except (
                AssertionError,
                AttributeError,
                LookupError,
                OSError,
                subprocess.SubprocessError,
                TypeError,
                ValueError,
            ) as error:
                errors.append("private scope observation: " + repr(error))
            self.evidence.update(cleanup_native_scopes=observations)
        if self.server_log is not None:
            self.server_log.close()
        self.evidence.update(cleanup={"errors": errors, "server_exit": self.server.poll() if self.server else None})
        if errors:
            self.evidence.update(passed=False)
        require(not errors, errors)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--parent", required=True)
    parser.add_argument("--parent-sha256", required=True)
    parser.add_argument("--worker", required=True)
    parser.add_argument("--worker-sha256", required=True)
    parser.add_argument("--evidence-dir")
    args = parser.parse_args()
    require(sys.platform == "darwin", "actual native completion is qualified here only on macOS")
    harness = None
    try:
        harness = Harness(args)
        harness.start()
        harness.cases()
    except BaseException as error:
        if harness is not None:
            harness.evidence.update(failure=repr(error), passed=False)
        raise
    finally:
        if harness is not None:
            harness.close()
    print(json.dumps({"passed": True, "evidence": str(harness.root / "report.json")}))


if __name__ == "__main__":
    main()
