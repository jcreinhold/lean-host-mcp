#!/usr/bin/env python3
"""Sweep lean-host-mcp RSS policy thresholds for any Lake workspace.

The script starts a fresh stdio MCP server for each threshold, runs a caller
provided sequence of public MCP tool calls, and emits JSONL records with
runtime-envelope memory/restart facts. It intentionally knows nothing about any
particular repository. Put workspace-specific declarations and file paths in the
workload JSON file.

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

  scripts/rss_threshold_sweep.py \
    --project-root fixtures/lean \
    --workload scripts/rss_threshold_sweep.fixture.json \
    --server-bin target/debug/lean-host-mcp \
    --workers-dir target/debug \
    --thresholds-kib 3145728,5242880,7340032 \
    --import-switch-soft-kib 2097152,5242880,7340032
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

DEFAULT_THRESHOLDS_KIB = [3 * 1024 * 1024, 5 * 1024 * 1024, 7 * 1024 * 1024]
MCP_PROTOCOL_VERSION = "2025-06-18"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run a lean-host-mcp RSS threshold sweep for a supplied MCP workload."
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
        "--thresholds-kib",
        default=",".join(str(value) for value in DEFAULT_THRESHOLDS_KIB),
        help="Comma-separated LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB values.",
    )
    parser.add_argument(
        "--import-switch-soft-kib",
        help=(
            "Optional comma-separated LEAN_HOST_MCP_IMPORT_SWITCH_RSS_SOFT_KIB values. "
            "When omitted, the server default is used."
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


def parse_thresholds(raw: str) -> list[int]:
    thresholds: list[int] = []
    for part in raw.split(","):
        text = part.strip()
        if not text:
            continue
        value = int(text)
        if value <= 0:
            raise ValueError(f"thresholds must be positive KiB values, got {value}")
        thresholds.append(value)
    if not thresholds:
        raise ValueError("at least one threshold is required")
    return thresholds


def parse_optional_thresholds(raw: str | None) -> list[int | None]:
    if raw is None or not raw.strip():
        return [None]
    return parse_thresholds(raw)


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
    def __init__(
        self,
        args: argparse.Namespace,
        post_job_threshold_kib: int,
        import_switch_soft_kib: int | None,
    ):
        env = os.environ.copy()
        env["LEAN_HOST_MCP_WORKER_RSS_POST_JOB_RESTART_KIB"] = str(
            post_job_threshold_kib
        )
        if import_switch_soft_kib is not None:
            env["LEAN_HOST_MCP_IMPORT_SWITCH_RSS_SOFT_KIB"] = str(
                import_switch_soft_kib
            )
        env["RUST_LOG"] = args.rust_log
        env.pop("LEAN_HOST_MCP_WORKER_RSS_CEILING_KIB", None)
        if args.workers_dir is not None:
            env["LEAN_HOST_MCP_WORKERS_DIR"] = str(args.workers_dir)

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
                    "name": "lean-host-mcp-rss-threshold-sweep",
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
    result = response.get("result")
    if not isinstance(result, dict):
        return {}
    structured = result.get("structuredContent")
    if isinstance(structured, dict):
        return structured
    nested = result.get("result")
    if isinstance(nested, dict):
        return nested
    return {}


def response_status(response: dict[str, Any], content: dict[str, Any]) -> str:
    if "error" in response:
        return "jsonrpc_error"
    if isinstance(content.get("status"), str):
        return content["status"]
    result = content.get("result")
    if isinstance(result, dict) and isinstance(result.get("status"), str):
        return result["status"]
    raw_result = response.get("result")
    if isinstance(raw_result, dict) and raw_result.get("isError") is True:
        return "tool_error"
    return "ok"


def nested_get(value: dict[str, Any], *parts: str) -> Any:
    current: Any = value
    for part in parts:
        if not isinstance(current, dict):
            return None
        current = current.get(part)
    return current


def first_candidates(content: dict[str, Any], limit: int = 5) -> list[str] | None:
    candidates = nested_get(content, "result", "candidates")
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
    post_job_threshold: int,
    import_switch_soft: int | None,
    call: dict[str, Any],
    response: dict[str, Any],
    wall_ms: int,
) -> dict[str, Any]:
    content = structured_content(response)
    runtime = content.get("runtime") if isinstance(content.get("runtime"), dict) else {}
    freshness = (
        content.get("freshness") if isinstance(content.get("freshness"), dict) else {}
    )
    query_facts = nested_get(content, "result", "query_facts")
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
        "event": "rss_threshold_sweep_call",
        "threshold_kib": post_job_threshold,
        "post_job_threshold_kib": post_job_threshold,
        "import_switch_soft_kib": import_switch_soft,
        "label": call["label"],
        "tool": call["tool"],
        "wall_ms": wall_ms,
        "status": response_status(response, content),
        "jsonrpc_error": response.get("error"),
        "project_hash": str(freshness.get("project_hash", ""))[:12] or None,
        "session_id": str(freshness.get("session_id", ""))[:8] or None,
        "worker_generation": runtime.get("worker_generation"),
        "worker_restarted": runtime.get("worker_restarted"),
        "retry_count": runtime.get("retry_count"),
        "admission_wait_millis": runtime.get("admission_wait_millis"),
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
        "top_candidates": first_candidates(content)
        if call["tool"] == "search_for_proof"
        else None,
    }


def summarize(
    post_job_threshold: int,
    import_switch_soft: int | None,
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
    max_admission_wait = 0
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
        admission_wait = record.get("admission_wait_millis")
        if isinstance(admission_wait, int):
            max_admission_wait = max(max_admission_wait, admission_wait)
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
        "event": "rss_threshold_sweep_summary",
        "threshold_kib": post_job_threshold,
        "post_job_threshold_kib": post_job_threshold,
        "import_switch_soft_kib": import_switch_soft,
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
        "max_admission_wait_millis": max_admission_wait,
        "max_queue_wait_millis": max_queue_wait,
        "stderr_contains_session_missing": "session_missing" in stderr,
        "stderr_contains_sigkill": "SIGKILL" in stderr,
        "exit_code": exit_code,
    }


def run_threshold(
    args: argparse.Namespace,
    post_job_threshold: int,
    import_switch_soft: int | None,
    calls: list[dict[str, Any]],
    sink: JsonlSink,
) -> dict[str, Any]:
    server = McpServer(args, post_job_threshold, import_switch_soft)
    server.initialize()
    records: list[dict[str, Any]] = []
    started = time.monotonic()
    stderr = ""
    try:
        for call in calls:
            call_started = time.monotonic()
            response = server.request(
                "tools/call", {"name": call["tool"], "arguments": call["arguments"]}
            )
            wall_ms = int((time.monotonic() - call_started) * 1000)
            record = call_record(
                post_job_threshold, import_switch_soft, call, response, wall_ms
            )
            records.append(record)
            sink.write(record)
    finally:
        stderr = server.shutdown()

    summary = summarize(
        post_job_threshold=post_job_threshold,
        import_switch_soft=import_switch_soft,
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
    calls = load_workload(args.workload, args.project_root)
    thresholds = parse_thresholds(args.thresholds_kib)
    import_switch_thresholds = parse_optional_thresholds(args.import_switch_soft_kib)
    sink = JsonlSink(args.output)
    try:
        summaries = [
            run_threshold(args, threshold, import_switch_soft, calls, sink)
            for threshold in thresholds
            for import_switch_soft in import_switch_thresholds
        ]
        sink.write(
            {"event": "rss_threshold_sweep_all_summaries", "summaries": summaries}
        )
    finally:
        sink.close()
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except KeyboardInterrupt:
        os.kill(os.getpid(), signal.SIGTERM)
