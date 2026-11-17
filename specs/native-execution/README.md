# Bounded native lifecycle models

Design-validation artifacts, not production execution code. Integrated 2026-11-17. The Python checkers model a proposed
protocol. **TLA+ is unparsed and TLC has not run.** No Rust runtime refinement proof, restart-recovery guarantee or OS
custody qualification follows from this directory.

## Provenance and scope

The Python and TLA+ sources are copied unchanged from the independently reviewed final package:
`~/.local/share/lean-lifecycle-evidence/model-20261117-lock-contract.tar.gz`. SHA256:
`316239c3315042885af1d36e68777e8de635069f057acacc8ec6c00a8c1c1b1e`. That local evidence archive retains README, HANDOFF,
review, environment hashes, full traces and run outputs. It is not needed to run these sources; it is needed to audit
the original recorded execution. Large generated results, scratch process fixtures, binaries and caches are deliberately
not committed.

Recorded package validation: 60 unit tests passed, with independent rerun. Thirty reduced late-control graphs (96–648
states, limit 30,000 each) passed; five deliberately broken handlers produced counterexamples. The larger combined run
remains **INCOMPLETE at 300,001 states**. Reduced checks do not replace it. The checkers report a reached state limit as
incomplete, not success.

| Sources | Purpose |
| --- | --- |
| `check_model.py` | Coarse custody, dispatch, crash and closure transitions; graph/fairness checks |
| `check_frozen.py`, `check_owner_outages.py` | Frozen owners, retained aliases and conditional recovery |
| `check_stages.py` | Immutable job/stage identity; authorization, GO, spawn and late messages |
| `check_atomic_cuts.py`, `check_cross_boundary.py` | Reduced crash/replacement cuts and broken-handler controls |
| `check_more_deliveries.py` | Larger combined exploration; retained incomplete limit |
| `check_late_controls.py` | Two late-control deliveries, stage/generation identity and negative controls |
| `test_*.py` | Checker/model regressions; not native-runtime tests |
| `Lifecycle.tla`, `Lifecycle.cfg` | Unparsed accompanying specification; not checked equivalent to all Python refinements |

## Replay without generating files in the checkout

Run the focused tests from this directory:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest -v \
  test_model.py test_stages.py test_cross_boundary.py test_late_controls.py
```

The exploration scripts write their named JSON outputs in the working directory. For full exploration copy this
directory to a disposable workspace and run there:

```sh
work=$(mktemp -d)
cp specs/native-execution/*.py specs/native-execution/*.tla specs/native-execution/*.cfg "$work/"
(cd "$work" && PYTHONDONTWRITEBYTECODE=1 python3 check_late_controls.py)
```

The second example assumes the repository root as starting directory. Run other `check_*.py` scripts in that same
disposable directory as needed. Preserve failures and incomplete results. No script starts Lean/native jobs or tests
real SQLite.

## Explicit assumptions and omissions

- Complete-domain acquisition is abstracted as one grant. Actual partial-lock acquisition/release and fairness against
  external writers require runtime tests.
- Custody aliases refer to an acquired open file description. **Close-only: no explicit unlock and no SH↔EX mode
  conversion while live custody exists.** Unlock through an alias can affect all aliases; conversion is not assumed
  atomic and may release prior custody. This is a runtime refinement assumption, not a modeled flock syscall or a claim
  that every failed conversion loses its lock.
- Crash, frozen-owner and cleanup transitions are different. Cleanup authority must come from actual retained
  child/qualified OS custody, never a serialized PID alone. Fair progress assumes the stated responsive actors and
  available termination/ observation; it does not cover unkillable or detached children.
- A service's third retained EX alias and original-parent guardian-stop authority were measured in separate disposable
  Rust topologies. They are NOT composed into the old two-alias model. A frozen service can itself retain the final
  alias.
- Pending/consumed GO can survive service loss before spawn. Missing result or no currently observed payload is not
  enough to authorize replacement. No replay of uncertain payloads; explicit new stages must retain different immutable
  IDs.
- Every runtime completion/cancel/cleanup callback must carry and validate its original attempt/stage ID, scope
  generation and bound owner identity before mutating active state. Old ACK/results affect only their own history. These
  are obligations for implementation, not evidence that current runtime does so.
- Storage failure, real descriptor transfer, OS signaling, actual callback code, new START/GO traffic composed with all
  late controls, and all owner failures combined with those controls remain outside completed validation.

The evolving service-death custody experiments are intentionally not claimed as completed by this integration. Reconcile
their final exact topology and evidence before adopting a production contract. See
[the design](../../docs/native-execution-lifecycle-design.md), especially §§4, 8, 11 and 13. Formal models and
real-process tests complement each other; neither silently supplies the other's assumptions.

## Unwired Rust state kernel

[The Rust stage kernel](stage-kernel.md) checks immutable stage bindings, one-use launch effects and isolation of old
callbacks. It is a directly testable reference module, not part of the running server. Its 25 focused tests include 48
ordered pairs of late controls. The document explains its API, model differences and the launch, authentication, closure
and persistence facts a future runtime must supply. Run it directly with `rustc`; no server or Lean build is needed.
