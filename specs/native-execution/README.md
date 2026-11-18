# Native execution models

TLA+ is the source of the abstract concurrency models. TLC explores their finite
cases and checks the stated properties. The small Python runner only invokes TLC;
it has no model, graph search or scheduling rules.

These models check proposed protocols, not the running service. Native process,
Rust and SQLite tests remain separate. A model assumes that an OS observation or
stop operation works; it cannot establish that capability.

## Run a check

Use Java 21 and the official `tla2tools.jar` from
[TLA+ v1.7.4](https://github.com/tlaplus/tlaplus/releases/tag/v1.7.4).
The tested tools are Homebrew OpenJDK 21.0.13 and TLC 2.19.
No Toolbox GUI or Python packages are needed.

From this directory:

```sh
uv run --no-project python check.py --list
uv run --no-project python check.py --case dual-one-fault
uv run --no-project python check.py --case cross-unsent
uv run --no-project python check.py
```

The last command runs all **155 required cases**, one at a time. Defaults are one
TLC worker, a 512 MiB Java heap limit and 30 seconds per case. `--timeout SECONDS`
changes the time limit. Repeat `--case NAME` to select several cases.

The runner uses `JAVA`, or Homebrew's Java 21 executable, then `java` from PATH.
It uses `TLA_TOOLS_JAR`, or `~/.local/share/tlaplus/1.7.4/tla2tools.jar`.
For another installation:

```sh
export JAVA=/path/to/java
export TLA_TOOLS_JAR=/path/to/tla2tools.jar
```

The runner does not install tools or change global settings. It creates temporary
TLC inputs and removes them afterward. It stores nothing in the checkout.
To retain a run, add `--output /path/to/results`: this writes only `summary.json`
and a combined `tlc.log`, including exact configurations and input hashes, with
no visited-state cache. Reusing that output directory replaces those two files.

Two larger stage cases need `--extended`:

```sh
uv run --no-project python check.py --extended --case stage-two-extra
uv run --no-project python check.py --extended --case stage-two-extra-controls
```

A timeout reports **INCOMPLETE**; a parse, runtime or launch failure reports
**ERROR**. Both exit unsuccessfully. Neither counts an explored prefix as a pass.
The runner accepts an expected invariant
failure only when TLC names the exact configured predicate. `MATCHED` can mean
an expected failure or a reachability witness; it does not mean the bad protocol
is safe. A witness case checks the negation of a target so TLC supplies a trace
that reaches it.

## Models and cases

| Source | Scope |
| --- | --- |
| `Lifecycle.tla` | Two jobs/domains, lock holders, launch, descendants, crashes and conditional cleanup |
| `LifecycleScenarios.tla` | Frozen owners, original-parent authority, first-scope versus all-job closure, three broken protocols and concurrency witnesses |
| `Stages.tla` | Two immutable stages, duplicate/delayed messages, separate authorization/send/consume/spawn, cancellation, results and late callbacks |
| `CrossBoundary.tla` | Pending old GO, lost/frozen owner, explicit closure evidence and replacement generation |
| `DeadlinePolicy.tla` | Bounded hard/soft deadline policy, activity renewal, quiet events and missing samples |

`cases.json` holds each model's defaults and named overrides. Constants contain
TLA+ expressions. The runner generates configurations from those records; there
is no second set of checked-in `.cfg` files. It disables TLC's deadlock check
because completed models may make no further state change. The lifecycle cases
still check their named eventual-closure properties.

## Checked scope and remaining gaps

At the 2026-11-17 checkpoint, SANY parsed these sources and the 155 required TLC
cases produced their intended result, including exact negative-control targets.
The two-message stage case completed with **802,787 distinct states** in 28.793
seconds. The larger case including all controls hit its 30-second limit and
remains **INCOMPLETE**. The earlier Python run that reached 300,001 states was a
different exploration; it also remains incomplete, not a TLC result.

The old 60 Python test methods are not 60 TLC tests or runtime checks. The TLA+
models replace the abstract Python explorers. Identity-store requirements moved
to the real Rust/SQLite contract tests; those suites and the macOS fault tests
remain. See [the native contract](stage-runtime-contract.md) and
[the saved-task contract](stage-store-contract.md).

The coarse models retain the earlier supervisor/guardian alias arrangement.
They do **not** model the selected SDK registry/token recovery plan, a retained
third service descriptor, boot/exec token capture or takeover. Full-domain lock
acquisition, sealed launch gates, observed whole-scope termination, authenticated
callbacks and release of the relevant descriptors remain explicit assumptions.
Fairness assumes responsive actors eventually take specified actions; it does
not assume a frozen owner resumes or an arbitrary child exits.

The separate models do not constitute a proof of their combined implementation.
Deadline-policy checks establish neither real clock behavior nor process exit.
The current-stage callback, persistence, OS and service-integration obligations
still require the real implementation tests described in
[the design](https://github.com/jcreinhold/proofs/blob/main/docs/projects/lean-tooling/design/native-execution-lifecycle.md).
