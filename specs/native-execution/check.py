#!/usr/bin/env python3
"""Run the named TLA+ cases with standard TLC; contains no model semantics."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent


def configuration(model, case):
    constants = model["constants"] | case.get("constants", {})
    lines = ["SPECIFICATION " + case.get("specification", model["specification"]), "CONSTANTS"]
    lines += [f"  {key} = {value}" for key, value in constants.items()]
    lines += ["INVARIANTS " + " ".join(case.get("invariants", model["invariants"]))]
    lines += ["CHECK_DEADLOCK FALSE"]  # Terminal stuttering is intentional.
    lines += ["PROPERTY " + p for p in case.get("properties", model["properties"])]
    return "\n".join(lines) + "\n"


def run_case(java, jar, work, case, cfg, timeout):
    name = case["name"]
    (work / (name + ".cfg")).write_text(cfg)
    states = work / (name + "-states")
    (work / "java-tmp").mkdir(exist_ok=True)
    command = [java, "-Xmx512m", "-XX:+UseParallelGC", "-XX:ActiveProcessorCount=2",
               "-Djava.io.tmpdir=" + str(work / "java-tmp"),
               "-cp", str(jar), "tlc2.TLC", "-workers", "1", "-seed", "1", "-fp", "0",
               "-metadir", str(states), "-config", name + ".cfg", case["model"] + ".tla"]
    started = time.monotonic()
    timed_out = False
    try:
        result = subprocess.run(command, cwd=work, text=True, stdout=subprocess.PIPE,
                                stderr=subprocess.STDOUT, timeout=timeout)
        code, log = result.returncode, result.stdout
    except subprocess.TimeoutExpired as error:
        timed_out = True
        code = None
        log = error.stdout.decode(errors="replace") if isinstance(error.stdout, bytes) else error.stdout or ""
        log += f"\nINCOMPLETE: external {timeout:g}-second limit reached.\n"
    except OSError as error:
        code, log = None, f"ERROR: could not run Java/TLC: {error}\n"
    finally:
        shutil.rmtree(states, ignore_errors=True)
    counts = re.findall(r"([\d,]+) states generated, ([\d,]+) distinct states found, ([\d,]+) states left on queue", log)
    inv = re.search(r"Invariant (\w+) is violated", log)
    observed = "INCOMPLETE" if timed_out else "ERROR"
    if code == 0 and "Model checking completed. No error has been found." in log and counts and counts[-1][2] == "0":
        observed = "pass"
    elif code == 12 and inv:
        observed = "invariant"
    elif code == 13 and "Temporal properties were violated" in log:
        observed = "liveness"
    matched = observed == case["expect"] and (observed != "invariant" or inv.group(1) == case["invariant"])
    row = {"name": name, "expected": case["expect"], "observed": observed,
           "matched": matched, "returncode": code, "seconds": round(time.monotonic() - started, 3),
           "command": command, "configuration": cfg}
    if inv:
        row["violated_invariant"] = inv.group(1)
    if counts:
        row.update(zip(("generated_states", "distinct_states", "states_left"),
                       (int(value.replace(",", "")) for value in counts[-1])))
    return row, log


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--model", help="Run all cases for one module")
    parser.add_argument("--case", action="append", help="Run a named case; repeat to select several")
    parser.add_argument("--extended", action="store_true", help="Include the two larger stage explorations")
    parser.add_argument("--list", action="store_true", help="List cases without starting Java")
    parser.add_argument("--timeout", type=float, help="Override per-case seconds (default: 30, 180 for host-faults)")
    parser.add_argument("--output", type=Path, help="Save summary.json and the combined tlc.log in this directory")
    args = parser.parse_args()
    catalog_bytes = (ROOT / "cases.json").read_bytes()
    catalog = json.loads(catalog_bytes)
    if args.model and args.model not in catalog["models"]:
        parser.error("Unknown model: " + args.model)
    if args.model and args.case:
        parser.error("Select a model or named cases")
    cases = [c for c in catalog["cases"] if not args.model or c["model"] == args.model]
    if args.list:
        for case in cases:
            print(case["name"], case["expect"], case.get("invariant", ""), "[extended]" if case.get("extended") else "")
        return 0
    if args.timeout is not None and args.timeout <= 0:
        parser.error("--timeout must be positive")
    names = {case["name"] for case in catalog["cases"]}
    if set(args.case or []) - names:
        parser.error("Unknown case: " + ", ".join(sorted(set(args.case) - names)))
    selected = [c for c in cases if not args.case or c["name"] in args.case]
    if args.case and not args.extended and any(c.get("extended") for c in selected):
        parser.error("The selected larger exploration needs --extended")
    selected = [c for c in selected if args.extended or not c.get("extended")]
    java = os.environ.get("JAVA") or next((str(p) for p in (
        Path("/opt/homebrew/opt/openjdk@21/bin/java"), Path("/usr/local/opt/openjdk@21/bin/java")) if p.is_file()), "java")
    jar = Path(os.environ.get("TLA_TOOLS_JAR", "~/.local/share/tlaplus/1.7.4/tla2tools.jar")).expanduser()
    if not jar.is_file():
        parser.error("Set TLA_TOOLS_JAR to the official tla2tools.jar")
    jar = jar.resolve()
    model_bytes = {name: (ROOT / (name + ".tla")).read_bytes() for name in catalog["models"]}
    if any(case["model"] == "Stages" for case in selected):
        sdk_path = Path(os.environ.get("LEAN_RS_WORKER_CUSTODY",
            ROOT.parents[2] / "lean-rs/specs/worker-custody/WorkerCustody.tla"))
        if not sdk_path.is_file():
            parser.error("Set LEAN_RS_WORKER_CUSTODY to the pinned SDK module (see README)")
        sdk = sdk_path.read_bytes()
        if hashlib.sha256(sdk).hexdigest() != "f1852903f4b35d044d2c5e75555c4f12b430313871ba56c1a49bb9a67d7aa6b5":
            parser.error("Set LEAN_RS_WORKER_CUSTODY to the reviewed SDK module (see README)")
        model_bytes["WorkerCustody"] = sdk
    jar_bytes = jar.read_bytes()
    hashes = {"cases.json": hashlib.sha256(catalog_bytes).hexdigest()}
    hashes.update({name + ".tla": hashlib.sha256(data).hexdigest() for name, data in model_bytes.items()})
    rows = []
    output_log = None
    if args.output:
        args.output.mkdir(parents=True, exist_ok=True)
        output_log = (args.output / "tlc.log").open("w")
    try:
        with tempfile.TemporaryDirectory(prefix="lean-tla-") as temporary:
            work = Path(temporary)
            for name, data in model_bytes.items():
                (work / (name + ".tla")).write_bytes(data)
            jar = work / "tla2tools.jar"
            jar.write_bytes(jar_bytes)
            for case in selected:
                cfg = configuration(catalog["models"][case["model"]], case)
                timeout = args.timeout or case.get("timeout_seconds", 30)
                row, log = run_case(java, jar, work, case, cfg, timeout)
                row["timeout_seconds"] = timeout
                rows.append(row)
                status = "MATCHED" if row["matched"] else row["observed"] if row["observed"] in {"INCOMPLETE", "ERROR"} else "UNEXPECTED"
                print(f"{case['name']}: {status} ({row['observed']}, {row['seconds']:.3f}s)", flush=True)
                if output_log:
                    output_log.write(f"\n===== {case['name']} =====\n{log}")
                    output_log.flush()
                elif not row["matched"]:
                    print(log[-3000:])
    finally:
        if output_log:
            output_log.close()
        if args.output:
            summary = {"input_sha256": hashes, "jar_sha256": hashlib.sha256(jar_bytes).hexdigest(),
                       "workers": 1, "heap_limit_mib": 512, "timeout_seconds": args.timeout or 30, "cases": rows}
            (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return 0 if all(row["matched"] for row in rows) else 1


if __name__ == "__main__":
    raise SystemExit(main())
