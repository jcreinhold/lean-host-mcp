"""Check enrolled launcher custody against the packaged warm HTTP server."""

import contextlib
import hashlib
import errno
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import sys
import time
import urllib.request
import uuid


@contextlib.contextmanager
def selector_reader(project):
    directory = os.open(project, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        fcntl.flock(directory, fcntl.LOCK_SH | fcntl.LOCK_NB)
        yield
    finally:
        os.close(directory)


def rpc(url):
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream"}

    def call(method, params, number=None):
        request = {"jsonrpc": "2.0", "method": method, "params": params}
        if number is not None:
            request["id"] = number
        with urllib.request.urlopen(
            urllib.request.Request(url, json.dumps(request).encode(), headers), timeout=10
        ) as response:
            if session := response.headers.get("Mcp-Session-Id"):
                headers["Mcp-Session-Id"] = session
            if number is None:
                return
            for line in response:
                if line.startswith(b"data:"):
                    data = line.removeprefix(b"data:").strip()
                    if not data:
                        continue
                    reply = json.loads(data)
                    if reply.get("id") == number:
                        break
                elif line.startswith(b"{"):
                    reply = json.loads(line)
                    break
            else:
                raise AssertionError(f"no response for {method} {number}")
        assert reply.get("id") == number and "error" not in reply, reply
        return reply["result"]

    call("initialize", {
        "protocolVersion": "2025-11-25", "capabilities": {},
        "clientInfo": {"name": "managed-launcher-http-guard", "version": "1"},
    }, 1)
    call("notifications/initialized", {})
    return call


def check(host, project, workers, prepared, scratch):
    state = scratch / "h"
    state.mkdir(mode=0o700)
    store = state / "store.sqlite3"
    owners = state / "owners"
    owners.mkdir(mode=0o700)
    (owners / ".admission.lock").touch(mode=0o600)
    env = {
        **os.environ,
        "LEAN_HOST_MCP_SOCKET_DIR": str(scratch),
        "LEAN_RS_WORKER_OWNERS_DIRECTORY": str(owners),
        "LEAN_HOST_MCP_PROCESS_REGISTRY_DIR": str(state / "processes"),
        "LEAN_HOST_MCP_CONFIG_DIR": str(state / "config"),
        "LEAN_HOST_MCP_WORKERS_DIR": str(workers),
    }
    opts = {"cwd": project, "env": env, "capture_output": True, "timeout": 10}
    base = [host, "--lake-root", project]
    managed_store = ["--managed-native-store", store]
    bootstrap = subprocess.run([*base, *managed_store], input=b"", **opts)
    assert bootstrap.returncode == 0, bootstrap.stderr
    with contextlib.closing(sqlite3.connect(store.as_uri() + "?mode=ro", uri=True)) as db:
        original, epoch = db.execute("SELECT store,epoch FROM meta WHERE singleton=1").fetchone()
    selector = project / "lean-host-mcp.toml"
    selector.write_text(
        '[launcher]\nmode = "ordinary"\ngeneration = 1\n'
        f'managed_store = {json.dumps(str(store))}\nmanaged_store_uuid = "{uuid.UUID(bytes=original)}"\n'
    )
    artifact_locks = []
    for root in (project, project / "dep"):
        probe = subprocess.run([host, "writer-lock-path", root], check=True, **opts)
        lock = Path(json.loads(probe.stdout)["lock_path"])
        stat = lock.stat()
        artifact_locks.append([str(lock), stat.st_dev, stat.st_ino])
    registry = owners.stat()
    custody = state / "custody.json"
    custody.write_text(json.dumps({
        "registry": {"path": str(owners), "device": registry.st_dev, "inode": registry.st_ino},
        "locks": artifact_locks,
    }))

    def transition(generation, mode, *, blocked=False, promote=False, cut_host=None):
        args = [cut_host or host, "launcher-transition", "--project", project, "--from-generation", str(generation),
                "--to", mode, "--custody", custody]
        if promote:
            args.append("--promote-warm-store")
        options = opts
        if cut_host:
            options = {**opts, "env": {**env, "LEAN_HOST_MCP_TEST_WARM_PROMOTION_CUT": "1"}}
        result = subprocess.run(args, **options)
        if cut_host:
            assert result.returncode != 0 and b"disposable cut after warm store promotion" in result.stderr, result
            return
        if blocked:
            assert result.returncode != 0 and (
                os.strerror(errno.EAGAIN).encode() in result.stderr
                or os.strerror(errno.EWOULDBLOCK).encode() in result.stderr
            ), result
        else:
            assert result.returncode == 0, result.stderr

    transition(1, "managed")
    real = None
    if guard_source := os.environ.get("LEAN_HOST_MCP_PROOFS_LAUNCHER_GUARD"):
        spec = importlib.util.spec_from_file_location("proofs_launcher_selection", guard_source)
        real = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = real
        spec.loader.exec_module(real)
    guard = real.guard if real else selector_reader
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    managed = [*base, *managed_store, "--managed-only", "--launcher-mode", "managed",
               "--launcher-generation", "2", "--prepared-generation", prepared.read_text().strip()]
    warm = ["--managed-warm-inspections"]

    def listening(broker):
        deadline = time.monotonic() + 10
        while True:
            assert broker.poll() is None
            try:
                with socket.create_connection(("127.0.0.1", port), timeout=0.25):
                    return
            except OSError:
                assert time.monotonic() < deadline
                time.sleep(0.02)

    def refused_broker(flags):
        duplicate = subprocess.run([*managed, *flags], input=b"", **opts)
        assert duplicate.returncode != 0 and b"managed store ownership unavailable" in duplicate.stderr, duplicate

    def schema_version():
        with contextlib.closing(sqlite3.connect(store.as_uri() + "?mode=ro", uri=True)) as db:
            return db.execute("PRAGMA user_version").fetchone()[0]

    bind = ["--bind", f"127.0.0.1:{port}"]
    previous = os.environ.get("LEAN_HOST_MCP_PREVIOUS_COLD_HOST")
    cold_args = [previous, *managed[1:]] if previous else managed
    with subprocess.Popen([*cold_args, *bind], cwd=project, env=env) as cold:
        try:
            listening(cold)
            transition(2, "ordinary", blocked=True)
            transition(3, "managed", blocked=True, promote=True)
            assert schema_version() == 5, "blocked transition changed the cold broker's schema"
        finally:
            if cold.poll() is None:
                cold.terminate()
                try:
                    assert cold.wait(timeout=10) == 0
                except subprocess.TimeoutExpired:
                    cold.kill()
                    cold.wait()
                    raise
    unpromoted = subprocess.run([*managed, *warm], input=b"", **opts)
    assert unpromoted.returncode != 0 and b"requires launcher-transition --promote-warm-store" in unpromoted.stderr, unpromoted
    assert schema_version() == 5
    stale_promotion = subprocess.run([
        host, "launcher-transition", "--project", project, "--from-generation", "1", "--to", "managed",
        "--custody", custody, "--promote-warm-store",
    ], **opts)
    assert stale_promotion.returncode != 0 and b"existing managed selector was not promoted" in stale_promotion.stderr
    assert schema_version() == 5
    transition(2, "ordinary")
    if cut_host := os.environ.get("LEAN_HOST_MCP_WARM_PROMOTION_CUT_HOST"):
        before = selector.read_bytes()
        transition(3, "managed", promote=True, cut_host=cut_host)
        assert schema_version() == 6 and selector.read_bytes() == before
        retry_ordinary = subprocess.run(
            [*base, "--launcher-mode", "ordinary", "--launcher-generation", "3"], input=b"", **opts
        )
        assert retry_ordinary.returncode == 0, retry_ordinary.stderr
    transition(3, "managed", promote=True)
    managed[managed.index("--launcher-generation") + 1] = "4"
    if previous:
        old = subprocess.run([previous, *managed[1:]], input=b"", **opts)
        assert old.returncode != 0 and (
            b"warm store requires --managed-warm-inspections" in old.stderr
            or b"original launcher store schema changed" in old.stderr
        ), old
    selection = real.capture(project) if real else None
    with subprocess.Popen([*managed, *warm, *bind], cwd=project, env=env) as broker:
        try:
            listening(broker)
            assert schema_version() == 6
            transition(4, "ordinary", blocked=True)
            with guard(project):
                call = rpc(f"http://127.0.0.1:{port}/mcp")

                def admission(number, **command):
                    envelope = call("tools/call", {
                        "name": "lean_managed_admission",
                        "arguments": {"request": json.dumps({"version": 1, "command": command})},
                    }, number)
                    assert not envelope.get("isError"), envelope
                    return json.loads(envelope["content"][0]["text"])

                discovery = admission(2, op="discover")
                assert discovery["store"] == list(original) and discovery["acceptance_epoch"] == epoch
                intent = json.dumps({"project": str(project), "job": [1], "revision": 1,
                                     "operation": "lake_module_preparation_v1",
                                     "target": {"kind": "module", "name": "Root"}}).encode()
                if real:
                    real.validate_service(discovery, intent)
                key = {"store": list(original), "epoch": epoch, "stage": 1, "nonce": list(uuid.uuid4().bytes)}
                auth = {"key": key, "capability": [37] * 32}
                admitted = admission(3, op="create", intent=list(intent), **auth)
                assert admitted["execution"] == "admitted", admitted
                cancelled = admission(4, op="cancel", **auth)
                assert cancelled["execution"] == "not_started" and cancelled["custody"] == "unobserved", cancelled
                assert admission(5, op="get", **auth) == cancelled
                for flags in ([], warm):
                    refused_broker(flags)

                # Exercise real warm reuse on the enrolled store. The transition
                # must succeed without any original-key reads after the drain.
                def tool(number, tool_name, **arguments):
                    reply = call("tools/call", {"name": tool_name, "arguments": arguments}, number)
                    assert not reply.get("isError"), reply
                    return json.loads(reply["content"][0]["text"])

                prepared_bytes = prepared.read_bytes().strip()
                imports = ["Init", *[module for module, _ in json.loads(prepared_bytes)["plan"]["modules"]]]
                originals = []
                for ordinal, offset in enumerate((10, 20), start=1):
                    warm_key = {**key, "nonce": list(uuid.uuid4().bytes)}
                    original_auth = {"key": warm_key, "capability": list(os.urandom(32))}
                    fields = dict(source=True, statement=True, docstring=True, attributes=True, flags=True,
                                  rendering="pretty", proof_search=False)
                    warm_intent = dict(
                        operation="lean_declaration_inspection_warm_v1", project=str(project),
                        job=list(uuid.uuid4().bytes), revision=1, imports=imports,
                        prepared_sha256=list(hashlib.sha256(prepared_bytes).digest()),
                        inspection=dict(name="first_ok", fields=fields, budgets=dict(per_field_bytes=8192, total_bytes=65536)),
                    )
                    created = admission(offset, op="create", **original_auth, intent=list(json.dumps(warm_intent).encode()))
                    assert created["execution"] == "admitted", created
                    tool(offset + 1, "lean_lookup", kind="declaration", name="first_ok",
                         project=str(project), imports=imports, managed_original=original_auth)
                    looked_up = admission(offset + 2, op="get", **original_auth)
                    assert looked_up["attempt_completion"]["ordinal"] == ordinal and looked_up["custody"] == "open"
                    saved = looked_up["result"]
                    assert json.loads(bytes(saved["body"]))["declaration"]["name"] == "first_ok"
                    acknowledged = admission(offset + 3, op="ack", **original_auth, result_id=saved["id"],
                                             result_digest=saved["sha256"])
                    assert acknowledged["result"]["state"] == "acknowledged"
                    originals.append((warm_key, bytes(saved["id"]), bytes(saved["sha256"])))

                draining = tool(30, "lean_runtime", action="drain", retire_idle=True)
                token = draining["lease_id"]
                end = time.monotonic() + 10
                number = 30
                while not draining["pool"]["workers_stopped"]:
                    assert time.monotonic() < end, draining
                    time.sleep(0.04)
                    number += 1
                    draining = tool(number, "lean_runtime", action="renew", lease_id=token)
                with contextlib.closing(sqlite3.connect(store.as_uri() + "?mode=ro", uri=True)) as db:
                    for warm_key, wanted_id, wanted_sha in originals:
                        row = db.execute(
                            "SELECT resource_report,result_kind,result_id,result_hash FROM stages "
                            "WHERE (epoch,nonce,stage)=(?,?,?)",
                            (warm_key["epoch"], bytes(warm_key["nonce"]), warm_key["stage"]),
                        ).fetchone()
                        assert row == (3, 3, wanted_id, wanted_sha), row
                broker.send_signal(signal.SIGTERM)
                assert broker.wait(timeout=10) == 0
                transition(4, "ordinary", blocked=True)
            transition(4, "ordinary")
            assert schema_version() == 6
            print("Normal drain settled both acknowledged warm keys; same-store rollback needed no client GET")
            if real:
                try:
                    with real.guard(project, expected=selection):
                        raise AssertionError("stale Python selection accepted")
                except ValueError as error:
                    assert "generation changed" in str(error), error
            stale = subprocess.run([*managed, *warm], input=b"", **opts)
            assert stale.returncode != 0 and b"selection or generation changed" in stale.stderr, stale
            ordinary = subprocess.run(
                [*base, "--launcher-mode", "ordinary", "--launcher-generation", "5"], input=b"", **opts
            )
            assert ordinary.returncode == 0, ordinary.stderr
        finally:
            if broker.poll() is None:
                broker.terminate()
                try:
                    broker.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    broker.kill()
                    broker.wait()
    print(f"Managed HTTP admission, singleton, transition and stale selectors passed ({'Proofs guard' if real else 'Python shared lock'}).")


if __name__ == "__main__":
    check(*map(Path, sys.argv[1:]))
