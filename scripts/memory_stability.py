#!/usr/bin/env python3
"""Measure lean-host-mcp worker memory stability for any Lake workspace.

The script starts one stdio MCP server, replays a caller-provided sequence of
public MCP tool calls `--repeats` times against it, and emits JSONL records with
runtime-envelope memory/restart facts. The question it answers is whether a
long-lived server grows or recycles with *call count*: watch
`final_worker_generation` (it should stay at the value the first call reported)
and `call_restart_count` (it should stay 0) across increasing `--repeats`.

It replaces a sweep over four resident-memory thresholds, which no longer exist
— worker memory is now bounded inside Lean by `runtime.lean_max_memory_kib`, and
nothing restarts on RSS. RSS is still reported, as an observation rather than a
policy input.

It intentionally knows nothing about any particular repository. Put
workspace-specific declarations and file paths in the workload JSON file.

Workload schema:

{
  "calls": [
    {
      "label": "proof_state_main",
      "tool": "proof_state",
      "arguments": {
        "file": "${PROJECT_ROOT}/MyProject/Main.lean",
        "declaration": "MyProject.mainTheorem"
      }
    }
  ]
}

All strings in the workload are expanded with:

- ${PROJECT_ROOT}: absolute project root passed on the command line

Example:

  scripts/memory_stability.py \
    --project-root fixtures/lean \
    --workload scripts/memory_stability.fixture.json \
    --server-bin target/debug/lean-host-mcp \
    --workers-dir target/debug \
    --repeats 20
"""

from __future__ import annotations

import argparse
import json
import os
import select
import signal
import subprocess
import time
from pathlib import Path
from typing import Any

MCP_PROTOCOL_VERSION = "2025-06-18"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Replay an MCP workload against one lean-host-mcp server and report memory/restart facts."
    )
    parser.add_argument(
        "--project-root",
        required=True,
        type=Path,
        help="Lake project root to pass as --lake-root.",
    )
    parser.add_argument(
        "--workload",
        required=True,
        type=Path,
        help='JSON file containing an array or {"calls": [...]} of MCP tool calls.',
    )
    parser.add_argument(
        "--server-bin",
        default=os.environ.get("LEAN_HOST_MCP_BIN", "lean-host-mcp"),
        help="lean-host-mcp binary to execute. Relative paths are resolved before changing cwd.",
    )
    parser.add_argument(
        "--workers-dir",
        type=Path,
        help="Optional LEAN_HOST_MCP_WORKERS_DIR override, useful for development builds.",
    )
    parser.add_argument(
        "--repeats",
        default=1,
        type=int,
        help=(
            "How many times to replay the workload against the same server. "
            "Growth with call count, if any, shows up as this rises."
        ),
    )
    parser.add_argument(
        "--lean-max-memory-kib",
        type=int,
        help=(
            "Optional LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB override (the Lean heap "
            "ceiling). When omitted, the server default is used."
        ),
    )
    parser.add_argument(
        "--request-timeout-secs",
        default=240,
        type=float,
        help="Timeout for each MCP response.",
    )
    parser.add_argument(
        "--output",
        type=Path,
        help="Optional JSONL output file. Records are always also printed to stdout.",
    )
    parser.add_argument(
        "--rust-log",
        default="warn",
        help="RUST_LOG value for the server process.",
    )
    return parser.parse_args()


def load_workload(path: Path, project_root: Path) -> list[dict[str, Any]]:
    data = json.loads(path.read_text())
    if isinstance(data, dict):
        calls = data.get("calls")
    else:
        calls = data
    if not isinstance(calls, list) or not calls:
        raise ValueError(
            "workload must be a non-empty array or an object with non-empty 'calls'"
        )

    expanded = expand_placeholders(calls, project_root.resolve())
    out: list[dict[str, Any]] = []
    for index, call in enumerate(expanded):
        if not isinstance(call, dict):
            raise ValueError(f"call {index} must be an object")
        label = call.get("label")
        tool = call.get("tool") or call.get("tool_name")
        arguments = call.get("arguments")
        if not isinstance(label, str) or not label:
            raise ValueError(f"call {index} needs a non-empty string label")
        if not isinstance(tool, str) or not tool:
            raise ValueError(f"call {label} needs a non-empty string tool")
        if not isinstance(arguments, dict):
            raise ValueError(f"call {label} needs an object arguments field")
        out.append({"label": label, "tool": tool, "arguments": arguments})
    return out


def expand_placeholders(value: Any, project_root: Path) -> Any:
    if isinstance(value, str):
        return value.replace("${PROJECT_ROOT}", str(project_root))
    if isinstance(value, list):
        return [expand_placeholders(item, project_root) for item in value]
    if isinstance(value, dict):
        return {
            key: expand_placeholders(item, project_root) for key, item in value.items()
        }
    return value


class McpServer:
    def __init__(self, args: argparse.Namespace):
        env = os.environ.copy()
        if args.lean_max_memory_kib is not None:
            env["LEAN_HOST_MCP_LEAN_MAX_MEMORY_KIB"] = str(args.lean_max_memory_kib)
        env["RUST_LOG"] = args.rust_log
        if args.workers_dir is not None:
            env["LEAN_HOST_MCP_WORKERS_DIR"] = str(args.workers_dir)
        # Every memory/restart fact this script reports -- worker_generation,
        # runtime_rss_kib, call_restart -- rides in the envelope's `telemetry`
        # block, which the server omits at its default `quiet` verbosity. Without
        # this the run still succeeds and every one of those columns is silently
        # null, which reads as "nothing restarted" rather than "nothing measured".
        env["LEAN_HOST_MCP_TELEMETRY_VERBOSITY"] = "full"

        self.proc = subprocess.Popen(
            [args.server_bin, "--lake-root", str(args.project_root)],
            cwd=str(args.project_root),
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        if self.proc.stdin is None or self.proc.stdout is None:
            raise RuntimeError("server did not expose stdin/stdout pipes")
        self.stdin = self.proc.stdin
        self.stdout = self.proc.stdout
        self.next_id = 0
        self.timeout_secs = args.request_timeout_secs

    def initialize(self) -> None:
        response = self.request(
            "initialize",
            {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "lean-host-mcp-memory-stability",
                    "version": "0.1.0",
                },
            },
        )
        if "error" in response:
            raise RuntimeError(f"initialize failed: {response['error']}")
        self.notify("notifications/initialized", {})

    def notify(self, method: str, params: dict[str, Any]) -> None:
        self._send({"jsonrpc": "2.0", "method": method, "params": params})

    def request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        self.next_id += 1
        request_id = self.next_id
        self._send(
            {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params}
        )
        while True:
            message = self._recv()
            if message.get("id") == request_id:
                return message

    def _send(self, message: dict[str, Any]) -> None:
        self.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.stdin.flush()

    def _recv(self) -> dict[str, Any]:
        if self.proc.stdout is None:
            raise RuntimeError("server stdout closed")
        fd = self.proc.stdout.fileno()
        deadline = time.monotonic() + self.timeout_secs
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError("timed out waiting for MCP response")
            readable, _, _ = select.select([fd], [], [], min(remaining, 1.0))
            if not readable:
                if self.proc.poll() is not None:
                    raise RuntimeError(f"server exited with {self.proc.returncode}")
                continue
            line = self.proc.stdout.readline()
            if not line:
                raise RuntimeError("server stdout closed")
            return json.loads(line)

    def shutdown(self) -> str:
        try:
            self.stdin.close()
        except OSError:
            pass
        if self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait(timeout=5)
        if self.proc.stderr is None:
            return ""
        return self.proc.stderr.read()


class JsonlSink:
    def __init__(self, output: Path | None):
        self.file = output.open("w") if output is not None else None

    def write(self, record: dict[str, Any]) -> None:
        line = json.dumps(record, separators=(",", ":"))
        print(line, flush=True)
        if self.file is not None:
            self.file.write(line + "\n")
            self.file.flush()

    def close(self) -> None:
        if self.file is not None:
            self.file.close()


def structured_content(response: dict[str, Any]) -> dict[str, Any]:
    """Return the tool's `SemanticResponse` envelope, whichever carrier it rode.

    The server's default `server.response_carrier` is `text`, so the envelope is
    a JSON *string* inside `content[0].text`; `structured` and `both` also fill
    `structuredContent`. Reading only the latter is how this script used to see
    an empty envelope on every call under stock configuration.
    """
    result = response.get("result")
    if not isinstance(result, dict):
        return {}
    structured = result.get("structuredContent")
    if isinstance(structured, dict):
        return structured
    for block in result.get("content") or []:
        if isinstance(block, dict) and block.get("type") == "text":
            try:
                decoded = json.loads(block.get("text") or "")
            except json.JSONDecodeError:
                continue
            if isinstance(decoded, dict):
                return decoded
    return {}


def issue_codes(content: dict[str, Any], severity: str) -> list[str]:
    errors = content.get("errors")
    if not isinstance(errors, list):
        return []
    return [
        str(issue.get("code"))
        for issue in errors
        if isinstance(issue, dict) and issue.get("severity") == severity
    ]


def response_status(response: dict[str, Any], content: dict[str, Any]) -> str:
    """Collapse one response to a single status token.

    `SemanticResponse` has no `status` field: a failed call is one carrying an
    `error`-severity issue (`runtime_unavailable` is the only one the runtime
    itself raises), and Lean-domain failures are ordinary `data`, by design.
    """
    if "error" in response:
        return "jsonrpc_error"
    raw_result = response.get("result")
    if isinstance(raw_result, dict) and raw_result.get("isError") is True:
        return "tool_error"
    codes = issue_codes(content, "error")
    if codes:
        return codes[0]
    return "ok"


def nested_get(value: dict[str, Any], *parts: str) -> Any:
    current: Any = value
    for part in parts:
        if not isinstance(current, dict):
            return None
        current = current.get(part)
    return current


def first_candidates(content: dict[str, Any], limit: int = 5) -> list[str] | None:
    candidates = nested_get(content, "data", "candidates")
    if not isinstance(candidates, list):
        return None
    out: list[str] = []
    for candidate in candidates[:limit]:
        if isinstance(candidate, dict):
            out.append(
                str(candidate.get("name") or candidate.get("declaration") or candidate)[
                    :120
                ]
            )
        else:
            out.append(str(candidate)[:120])
    return out


def call_record(
    pass_index: int,
    call: dict[str, Any],
    response: dict[str, Any],
    wall_ms: int,
) -> dict[str, Any]:
    content = structured_content(response)
    # `telemetry` is present only under `telemetry.verbosity = full`, which this
    # script's server env sets; `trust` is always emitted.
    runtime = nested_get(content, "telemetry", "runtime")
    if not isinstance(runtime, dict):
        runtime = {}
    telemetry = content.get("telemetry") if isinstance(content.get("telemetry"), dict) else {}
    trust = content.get("trust") if isinstance(content.get("trust"), dict) else {}
    query_facts = nested_get(content, "data", "query_facts")
    if not isinstance(query_facts, dict):
        query_facts = {}
    timings = (
        query_facts.get("timings")
        if isinstance(query_facts.get("timings"), dict)
        else {}
    )
    call_restart = (
        runtime.get("call_restart")
        if isinstance(runtime.get("call_restart"), dict)
        else {}
    )
    last_restart = (
        runtime.get("last_restart")
        if isinstance(runtime.get("last_restart"), dict)
        else {}
    )
    return {
        "event": "memory_stability_call",
        "pass_index": pass_index,
        "label": call["label"],
        "tool": call["tool"],
        "wall_ms": wall_ms,
        "status": response_status(response, content),
        "jsonrpc_error": response.get("error"),
        "project_hash": str(telemetry.get("project_hash", ""))[:12] or None,
        "session_id": str(trust.get("session_id", ""))[:8] or None,
        "worker_generation": runtime.get("worker_generation"),
        "worker_restarted": runtime.get("worker_restarted"),
        "retry_count": runtime.get("retry_count"),
        "queue_wait_millis": runtime.get("queue_wait_millis"),
        "runtime_rss_kib": runtime.get("rss_kib"),
        "worker_lanes": runtime.get("worker_lanes"),
        "profile_switch_count": runtime.get("profile_switch_count"),
        "call_restart_cause": call_restart.get("cause"),
        "call_restart_planned": call_restart.get("planned"),
        "call_restart_rss_kib": call_restart.get("rss_kib"),
        "call_restart_limit_kib": call_restart.get("limit_kib"),
        "last_restart_cause": last_restart.get("cause"),
        "cache_status": query_facts.get("cache_status"),
        "elaboration_micros": timings.get("elaboration_micros"),
        "warnings": issue_codes(content, "warning") or None,
        "top_candidates": first_candidates(content)
        if call["arguments"].get("kind") == "proof_search"
        else None,
    }


def summarize(
    repeats: int,
    records: list[dict[str, Any]],
    wall_ms: int,
    stderr: str,
    exit_code: int | None,
) -> dict[str, Any]:
    status_counts: dict[str, int] = {}
    call_restart_causes: dict[str, int] = {}
    last_restart_causes: dict[str, int] = {}
    cache_hits = 0
    peak_runtime_rss = 0
    peak_call_restart_rss = 0
    max_generation = 0
    final_generation = None
    worker_restarted_true = 0
    planned_restart = 0
    unplanned_restart = 0
    retry_total = 0
    max_retry = 0
    max_queue_wait = 0

    for record in records:
        status = str(record.get("status"))
        status_counts[status] = status_counts.get(status, 0) + 1
        if record.get("cache_status") == "hit":
            cache_hits += 1
        runtime_rss = record.get("runtime_rss_kib")
        if isinstance(runtime_rss, int):
            peak_runtime_rss = max(peak_runtime_rss, runtime_rss)
        restart_rss = record.get("call_restart_rss_kib")
        if isinstance(restart_rss, int):
            peak_call_restart_rss = max(peak_call_restart_rss, restart_rss)
        generation = record.get("worker_generation")
        if isinstance(generation, int):
            max_generation = max(max_generation, generation)
            final_generation = generation
        if record.get("worker_restarted") is True:
            worker_restarted_true += 1
        retry_count = record.get("retry_count")
        if isinstance(retry_count, int):
            retry_total += retry_count
            max_retry = max(max_retry, retry_count)
        queue_wait = record.get("queue_wait_millis")
        if isinstance(queue_wait, int):
            max_queue_wait = max(max_queue_wait, queue_wait)
        call_cause = record.get("call_restart_cause")
        if isinstance(call_cause, str) and call_cause:
            call_restart_causes[call_cause] = call_restart_causes.get(call_cause, 0) + 1
            planned = record.get("call_restart_planned")
            if planned is True:
                planned_restart += 1
            elif planned is False:
                unplanned_restart += 1
        last_cause = record.get("last_restart_cause")
        if isinstance(last_cause, str) and last_cause:
            last_restart_causes[last_cause] = last_restart_causes.get(last_cause, 0) + 1

    return {
        "event": "memory_stability_summary",
        "repeats": repeats,
        "wall_ms": wall_ms,
        "call_count": len(records),
        "status_counts": status_counts,
        "cache_hits": cache_hits,
        "peak_runtime_rss_kib": peak_runtime_rss,
        "peak_call_restart_rss_kib": peak_call_restart_rss,
        "peak_observed_worker_rss_kib": max(peak_runtime_rss, peak_call_restart_rss),
        "max_worker_generation": max_generation,
        "final_worker_generation": final_generation,
        "worker_restarted_true_count": worker_restarted_true,
        "call_restart_count": sum(call_restart_causes.values()),
        "planned_restart_count": planned_restart,
        "unplanned_restart_count": unplanned_restart,
        "call_restart_causes": call_restart_causes,
        "last_restart_causes": last_restart_causes,
        "retry_count_total": retry_total,
        "max_retry_count": max_retry,
        "max_queue_wait_millis": max_queue_wait,
        "stderr_contains_session_missing": "session_missing" in stderr,
        "stderr_contains_sigkill": "SIGKILL" in stderr,
        "exit_code": exit_code,
    }


def run_workload(
    args: argparse.Namespace,
    calls: list[dict[str, Any]],
    sink: JsonlSink,
) -> dict[str, Any]:
    server = McpServer(args)
    server.initialize()
    records: list[dict[str, Any]] = []
    started = time.monotonic()
    stderr = ""
    try:
        for pass_index in range(args.repeats):
            for call in calls:
                call_started = time.monotonic()
                response = server.request(
                    "tools/call", {"name": call["tool"], "arguments": call["arguments"]}
                )
                wall_ms = int((time.monotonic() - call_started) * 1000)
                record = call_record(pass_index, call, response, wall_ms)
                records.append(record)
                sink.write(record)
    finally:
        stderr = server.shutdown()

    summary = summarize(
        repeats=args.repeats,
        records=records,
        wall_ms=int((time.monotonic() - started) * 1000),
        stderr=stderr,
        exit_code=server.proc.returncode,
    )
    sink.write(summary)
    return summary


def main() -> int:
    args = parse_args()
    args.project_root = args.project_root.resolve()
    if not args.project_root.exists():
        raise SystemExit(f"project root does not exist: {args.project_root}")
    if os.sep in args.server_bin or (
        os.altsep is not None and os.altsep in args.server_bin
    ):
        args.server_bin = str(Path(args.server_bin).resolve())
    if args.workers_dir is not None:
        args.workers_dir = args.workers_dir.resolve()
    if args.repeats < 1:
        raise SystemExit("--repeats must be at least 1")
    calls = load_workload(args.workload, args.project_root)
    sink = JsonlSink(args.output)
    try:
        run_workload(args, calls, sink)
    finally:
        sink.close()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        os.kill(os.getpid(), signal.SIGTERM)
