"""Darwin installed cancellation check.

Run with PARENT DISPOSABLE_WORKERS_DIR NEW_CHECKOUT_TARGET_CHILD [cancel|death].
Use the normal install-worker command with --workers-dir under target.
The smoke project and native registry must be exclusive to this test.
"""

import contextlib
import ctypes
import itertools
import json
import os
import queue
import shutil
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path


def observations(scope):
    library = scope / "observation.dylib"
    subprocess.run(
        [
            "/usr/bin/clang",
            "-dynamiclib",
            "-Wall",
            "-Wextra",
            "-Werror",
            str(Path(__file__).with_name("warm_process.c")),
            "-o",
            library,
        ],
        env=dict(os.environ, TMPDIR=str(scope / "t")),
        check=True,
    )
    lib = ctypes.CDLL(str(library))

    def observe(function, pid, left, right):
        err = function(pid, ctypes.byref(left), ctypes.byref(right))
        if err:
            raise OSError(err, os.strerror(err))
        return left.value, right.value

    def pipe(pid):
        return observe(lib.stdin_pipe, pid, ctypes.c_int(), ctypes.c_int64())

    def birth(pid):
        return observe(lib.birth, pid, ctypes.c_uint64(), ctypes.c_uint64())

    return pipe, birth


@contextlib.contextmanager
def resume_on_exit(pid, identity, birth):
    try:
        yield
    finally:
        try:
            if birth(pid) == identity:
                os.kill(pid, signal.SIGCONT)
        except ProcessLookupError:
            pass


def children(pid):
    rows = subprocess.check_output(["ps", "-axo", "ppid=,pid="], text=True)
    return [
        int(child)
        for parent, child in map(str.split, rows.splitlines())
        if int(parent) == pid
    ]


def absent(group):
    try:
        os.killpg(group, 0)
    except ProcessLookupError:
        return True
    return False


def until(probe):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        value = probe()
        if value:
            return value
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {probe}")


def check(parent, workers, scope, mode):
    target = Path(__file__).resolve().parents[1] / "target"
    assert sys.platform == "darwin" and os.uname().machine == "arm64"
    assert parent.is_file() and workers.is_relative_to(target) and workers != target
    assert scope.parent == target and not scope.exists()
    slot = workers / "v4.36.0"
    project = slot / (slot / "current-generation").read_text().strip() / "smoke-project"
    assert project.resolve().is_relative_to(workers)
    for name, contents in {
        "lean-toolchain": "leanprover/lean4:v4.36.0\n",
        "lakefile.toml": 'name = "warm"\n',
        "lake-manifest.json": '{"version":"1.1.0","packagesDir":".lake/packages","packages":[],"name":"warm","lakeDir":".lake"}',
    }.items():
        path = project / name
        if path.exists():
            assert path.read_text() == contents
        else:
            with path.open("x") as file:
                file.write(contents)
    scope.mkdir(mode=0o700)
    for name in ("s", "t", "owners", "cache", "process"):
        (scope / name).mkdir(mode=0o700)
    pipe_state, birth = observations(scope)
    env = dict(
        os.environ,
        TMPDIR=str(scope / "t"),
        LEAN_HOST_MCP_WORKERS_DIR=str(workers),
        LEAN_HOST_MCP_SOCKET_DIR=str(scope / "s"),
        LEAN_RS_WORKER_OWNERS_DIRECTORY=str(scope / "owners"),
        LAKE_CACHE_DIR=str(scope / "cache"),
        LEAN_HOST_MCP_PROCESS_REGISTRY_DIR=str(scope / "process"),
        LEAN_HOST_MCP_MAX_WORKERS="1",
        LEAN_HOST_MCP_MAX_WORKERS_PER_PROJECT="1",
    )
    proc = subprocess.Popen(
        [
            "sandbox-exec",
            "-p",
            '(version 1)(allow default)(deny file-write* (subpath "/private/tmp"))',
            parent,
            "--lake-root",
            project,
            "--request-journal",
            scope / "journal",
        ],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
        env=env,
    )
    events = queue.Queue()
    replies = {}
    ids = itertools.count(1)

    def reader():
        for line in proc.stdout:
            msg = json.loads(line)
            if "id" in msg:
                events.put(msg)
        events.put(None)

    def send(method, params, notification=False):
        msg = {"jsonrpc": "2.0", "method": method, "params": params}
        if not notification:
            msg["id"] = next(ids)
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()
        return msg.get("id")

    def receive(ident):
        while ident not in replies:
            msg = events.get(timeout=20)
            assert msg is not None, "server exited"
            replies[msg["id"]] = msg
        return replies.pop(ident)

    def call(method_name, **args):
        return send("tools/call", {"name": method_name, "arguments": args})

    def body(ident):
        msg = receive(ident)
        assert "result" in msg, msg
        return json.loads(msg["result"]["content"][0]["text"])

    def pool():
        return body(call("lean_runtime", action="status"))["pool"]

    def lookup(name):
        return call("lean_lookup", kind="declaration", project=str(project), name=name)

    def found(result, name):
        assert not result["errors"] and name in json.dumps(result["data"]), result

    threading.Thread(target=reader, daemon=True).start()
    resumes = contextlib.ExitStack()
    try:
        receive(
            send(
                "initialize",
                {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "warm-cancel", "version": "1"},
                },
            )
        )
        send("notifications/initialized", {}, True)
        found(body(lookup("Nat.add_comm")), "Nat.add_comm")
        (worker,) = children(proc.pid)
        (guardian,) = children(worker)
        assert os.getpgid(worker) == worker and os.getpgid(guardian) == guardian
        worker_birth, guardian_birth = birth(worker), birth(guardian)
        args = {"kind": "declaration", "project": str(project), "name": "Nat.add_assoc"}
        ticket = body(
            call("lean_request", action="prepare", tool="lean_lookup", arguments=args)
        )
        binding = {
            "request_id": ticket["taskId"],
            "request_epoch": ticket["request"]["request_epoch"],
        }
        before, count = pipe_state(worker)
        assert count == 0
        submit = call(
            "lean_request",
            action="submit",
            tool="lean_lookup",
            arguments=args,
            **binding,
        )
        deadline = time.monotonic() + 5
        while pipe_state(worker) == (before, 0):
            if time.monotonic() >= deadline:
                raise AssertionError("no command observed on warmed worker stdin")
        resumes.enter_context(resume_on_exit(worker, worker_birth, birth))
        os.kill(worker, signal.SIGSTOP)
        until(
            lambda: (
                "T"
                in subprocess.check_output(
                    ["ps", "-o", "state=", "-p", str(worker)], text=True
                )
            )
        )
        body(submit)
        parked_pipe = pipe_state(worker)
        second = lookup("Nat.mul_comm")
        state = until(lambda: s if (s := pool())["outstanding_jobs"] == 2 else None)
        assert (
            state["occupied_slots"] == 1
            and state["projects"][0]["outstanding_jobs"] == 2
        )
        assert pipe_state(worker) == parked_pipe
        guard_resume = resumes.enter_context(contextlib.ExitStack())
        guard_resume.enter_context(resume_on_exit(guardian, guardian_birth, birth))
        os.kill(guardian, signal.SIGSTOP)
        until(
            lambda: (
                "T"
                in subprocess.check_output(
                    ["ps", "-o", "state=", "-p", str(guardian)], text=True
                )
            )
        )
        try:
            if mode == "death":
                assert birth(worker) == worker_birth and worker in children(proc.pid)
                os.kill(worker, signal.SIGKILL)

                def dead_or_zombie():
                    state = subprocess.run(
                        ["ps", "-o", "state=", "-p", str(worker)],
                        text=True,
                        capture_output=True,
                        check=False,
                    )
                    return (
                        state.returncode == 1
                        and not state.stdout.strip()
                        and not state.stderr
                    ) or (state.returncode == 0 and "Z" in state.stdout)

                until(dead_or_zombie)
            else:
                intent = body(call("lean_request", action="cancel", **binding))
                assert intent["cancellation_requested"], intent
            held = pool()
            if mode == "cancel" or held["occupied_slots"] == 1:
                assert held["occupied_slots"] == 1, held
                if mode == "cancel":
                    assert not absent(guardian)
            else:
                assert (
                    held["workers_stopped"] and absent(worker) and absent(guardian)
                ), held
            assert set(children(proc.pid)) <= {worker}
            if mode == "cancel":
                assert second not in replies
            # Cancellation may already have reaped the worker while its stopped
            # guardian still holds custody. Only consult the original live pipe.
            try:
                if mode != "death" and birth(worker) == worker_birth:
                    assert pipe_state(worker) == parked_pipe
            except ProcessLookupError:
                pass
        finally:
            guard_resume.close()
        result = body(second)
        if result["errors"]:
            assert (
                result["errors"][0]["details"]["reason"] == "project_shutting_down"
            ), result
            until(lambda: pool()["occupied_slots"] == 0)
            assert absent(worker) and absent(guardian)
            found(body(lookup("Nat.mul_assoc")), "Nat.mul_assoc")
        else:
            found(result, "Nat.mul_comm")
        assert absent(worker) and absent(guardian)
        (replacement,) = children(proc.pid)
        assert replacement != worker and os.getpgid(replacement) == replacement
        (replacement_guardian,) = children(replacement)
        terminal = body(call("lean_request", action="get", **binding))
        assert terminal["execution"]["disposition"] == "stopped", terminal
        receipt = terminal["execution"]["worker_stop_receipt"]
        assert receipt["worker_pid"] == worker, receipt
        if mode == "death":
            assert (
                not terminal["cancellation_requested"] and not receipt["exit_success"]
            ), receipt
            assert "SIGKILL" in receipt["exit_status"] and receipt["cause"] in {
                "child_abort",
                "child_exit",
            }, receipt
        else:
            assert receipt["cause"] == "cancelled", receipt
        assert receipt["worker_generation"] == 1 and receipt["worker_request_id"] > 0, (
            receipt
        )
        assert receipt["operation"] == "open_worker_session", receipt
        print(receipt, flush=True)
        print(
            f"{mode} request {ticket['taskId']}, one occupied slot for two jobs; "
            f"closed groups {worker}/{guardian}; replacement {replacement}",
            flush=True,
        )
    finally:
        resumes.close()
        proc.stdin.close()
        assert proc.wait(timeout=30) == 0
    until(lambda: absent(replacement) and absent(replacement_guardian))
    registry = scope / "owners"
    identity = registry.stat()
    subprocess.run(
        [
            parent,
            "doctor",
            "writer-rollback",
            "--registry",
            registry,
            "--registry-device",
            str(identity.st_dev),
            "--registry-inode",
            str(identity.st_ino),
            "--confirm-launchers-drained",
        ],
        env=env,
        check=True,
    )
    shutil.rmtree(scope)


if __name__ == "__main__":
    mode = sys.argv[4] if len(sys.argv) == 5 else "cancel"
    assert mode in {"death", "cancel"} and len(sys.argv) in {4, 5}
    check(*(Path(arg).resolve() for arg in sys.argv[1:4]), mode)
