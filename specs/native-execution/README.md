# Native execution models

TLA+ is the source of the abstract concurrency models. TLC explores their finite
cases and checks the stated properties. The small Python runner only invokes TLC;
it has no model, graph search or scheduling rules.

These models check proposed protocols, not the running service. Native process,
Rust and SQLite tests remain separate. A model assumes that an OS observation or
stop operation works; it cannot establish that capability.

## Host contract

`Stages.tla` extends the SDK's `WorkerCustody` module. Host-only steps stutter on SDK state. Bootstrap, custody loss,
cleanup and return use the SDK actions themselves. Positive composition configurations check the SDK's initial condition and every projected transition via its
`Spec` temporal property. The host checks progress separately under its stated fairness assumptions. `Mode = "good"` selects the proposed SDK protocol. Host-only configurations use
the same input module. The other modules preserve historical traces and focused policy tests. They do not define
additional ways for the host to establish closure.

**Identity across the host/SDK interface.** At reservation the host binds the saved operation to the modeled SDK
instance's scope, generation and owner. For each send it records the caller request key, native ID, command ordinal
and bound instance. The worker compares the received key to the command expected on its own instance. A running
command may emit one reply carrying its producer key and payload outcome. The host compares that key to its saved
send before interpreting the response. The first validated companion reply enables the second command; only the
second valid reply can supply ranked success. A closure receipt instead names the *worker generation*, since both
commands share that lifetime. The host accepts it against the reservation to release its domain holds. The SDK
returns its own capacity through its existing retirement rule.

The identity case includes authentic delayed evidence from one retired generation with the same scope and owner
names. Its reply belongs to a different original request; its closure belongs to the retired reservation. The case
also offers a current command misaddressed to that old generation. The live SDK instance contributes a closure
receipt only upon certified closure (including certified no-spawn). `ReplyIdentity` checks the accepted key and
payload against a reply actually emitted by the live command. `ClosureIdentity` checks that accepted cleanup
evidence belongs to the bound, closed instance after SDK capacity return.

**Active recovery.** Two contenders each try to claim the durable binding while the old launcher is still live.
Only an exact match for the request, intent, native ID, scope, generation and owner can win. A second exact
contender gets busy. Changed intent and stale scope are refused. Winning seals unused GO authority and permits
requesting a fence. The claim leaves an already issued GO with the old launcher. The receiver fence arbitrates
against that GO and target start. The bounded case reaches both orders after both contenders have tried.
It delivers the same GO twice and counts target starts inside the SDK scope separately from GO redemption and
SDK bootstrap. Removing receiver deduplication allows two target starts despite the single SDK spawn. This
case tests a live, responsive old authority and one durable request under two contenders and two deliveries.
It assumes durable arbitration and a receiver that enforces the fence. Unavailable authority or evidence can
leave execution uncertain. The model grants recovery no right to signal a PID or issue another GO.

**Host actor permit.** One warm actor holds one host permit and binds one custody scope. It saves the result
of one request and starts a second request on that generation while still charged. It returns the permit after
sealing admission and accepting authenticated closure for its bound scope. A serialized fence and certified
no-start can also settle the scope. Stale closure and redelivery of an accepted receipt release nothing.
The SDK slot can return before the host permit. Return can proceed with result ACK outstanding. The positive
case checks return progress once matching closure and sealing are both true. Broken rules allow return upon
request completion, on stale evidence, and more than once. Production retirement must settle every scope that
an actor could still use. This case tracks one bound scope and checks neither enumeration of multiple scopes
nor recovery of the real host permit ledger from disk.

**Trust assumption.** The adapter must obtain command responses and custody observations from the authentic
producer, preserve their originating binding through delivery and recovery, and keep generation identities unique
across reuse. The model represents those channels by their sets of possible producer outputs. Emitted replies remain eligible for delivery while the command awaits its reply. Arbitrary transport loss is outside
this finite case. A stale authentic output can reach the matcher. A forged output relabeled as current is outside the model. The old generation is
represented by its historical evidence; its process lifetime and the durable allocation of generation numbers
are outside this check. The reservation step pairs a host binding with an SDK instance in one abstract transition.
An implementation must establish that association across persistence and startup crash cuts. No new SDK wire field
is required by this model. Host visibility of SDK internals in TLC does not establish an authentic native observation.

The unit of custody is one physical worker generation. The unit of retry is one durable job, logical stage and
explicit execution revision. A command ordinal identifies a message in that stage. Its immutable endpoint is the
original scope, generation and owner. A warm generation can execute successive calls. The unique generation head
belongs to creation of a lifetime; subsequent commands bind to that existing lifetime. Results identify their
original request and producer, even after later work has begun. In the late-command case, `LateDelivery` offers command 1’s key to command 2’s
slot in the same generation. The correct guard disables that transition. There is no modeled receive-and-reject
event, reply or rejection counter. In the mutant, the transition is enabled and `SafeCommands` catches execution at
the wrong endpoint. The identity cases exercise a previous generation; the historical cases retain broader duplicate and late-message coverage.

| Boundary | Host obligation and observable outcome |
| --- | --- |
| Submit and admit | Save the caller binding before native CREATE. Equal retry reads the original identity. Changed intent is refused. Acquire the two modeled domains in order. On refusal of the second, fence the old authority and release the acquired prefix. Domain counts represent prefixes of this fixed order. |
| Bootstrap | Reserve custody before any process spawn. Persist one-use authorization before sending GO. Its receiver consumes once before spawn and drops duplicate GO. A crash between these steps can lose a reply or leave an old GO deliverable. An authenticated recovery owner can query and request a fence on the original revision, with no new launch authorization. An affirmative no-spawn rollback requires the live receiver to serialize sealing against consumption and spawn. |
| Load and run | Physical spawn starts bootstrap. Only startup ACK after authenticated identities, durable bootstrap identity, adoption of the cleanup lease and durable Running publication, followed by protected-read admission, enables command send. An outstanding send may win the race with a subsequent cancel until the receiver fence takes effect. |
| Warm or companion | In the warm profile two serial commands span a saved first completion on the same generation. An ordinary completed call leaves the worker available for reuse until a distinct retirement. In the companion profile one request opens one capability session. Goal features must validate before sending declaration features. A valid second reply enables ranking. An invalid reply at either ordinal is the primary failure. |
| Save and ACK | Reply validation, durable publication, caller copy and ACK are separate transitions. Caller loss can discard a validated but unpublished result. A committed result remains available for same-key reconciliation. An unknown execution disposition is a status report and cannot be copied or ACKed as a semantic result. Result bytes/digests, authenticated GET, expiry and tombstones are detailed in the store contract. The finite composition represents payloads by distinct outcome tags. |
| Stop and recover | Cancellation, deadline and loss can request a stop. A deadline calculation supplies no closure evidence. A responsive exact owner can fence messages. Otherwise reconcile durable facts, surface uncertainty and retain disputed capacity. Before durable Running, retirement needs a durable group-closure receipt from the guardian or adopted cleanup owner plus authenticated guardian absence. For durable Running it needs authenticated group and guardian absence. A dead Preparing owner alone cannot release capacity after possible spawn. Return a host actor permit once the old spawning authority is sealed and all relevant generations are settled. |
| Schedule | A conflicting writer waits for release of the old domain prefix and SDK admission. An unspent, fenced request that never reserved can roll back without a worker certificate. A disjoint request has its own ready worker, domain and spare slot. Its completion is explicitly scheduled. |

**Fairness and bounds.** One original binding, two commands, one custody generation, two ordered domain acquisitions,
at most one caller crash, one independent ready request and one conflicting successor. Healthy cases assume responsive
storage, delivery and execution, valid replies, no cancellation, no deadline expiry and a stable owner. `WF_hv(Main)` applies to the disjunction of forward actions:
when that disjunction stays enabled, some forward action must eventually change state. It provides no separate
fairness promise to a particular store, command, cleanup or admission action. In this bounded model forward paths
contain finitely many state changes, so an endless stream of other forward work cannot starve a continuously
enabled step. `WF_hv(Other)` independently schedules the disjoint request on its assumed ready worker and spare slot.
The checked progress properties rely on those bounds; a scheduler with recurring arrivals or retries needs its own
fairness argument. The fault case permits caller and SDK
supervisor loss independently, delayed execution, guardian/root/group loss, invalid responses, cleanup failure and
cancellation. Faults and natural process exit have no fairness obligation. Recovery may return unknown. Physical
cleanup progress depends on a surviving responsive authority, authentic exact-generation observations and durable
evidence. An unknown group can remain fenced while disjoint work finishes. No property grants a new process the
old supervisor's authority.

**Source and test obligations.**

- `src/project.rs::run_job` uses the existing warm handle; `run_semantic_job` opens a per-call companion, samples its
  CPU, preserves artifact domains and tries shutdown. `src/semantic_search.rs::run_semantic_proof_search` issues the
  two commands on the same session and validates schema, command, command version and feature version for each.
  `finish_semantic_shutdown(...)?` can displace a computed primary outcome. Runtime acceptance requires both the
  original result/error and independent pending/failed cleanup, with capacity retained. Streaming is outside this path.
- Production `src/tasks.rs` and `src/request_journal.rs` own retained requests. The separate caller/native SQLite
  files, original-key crash cuts and copy-before-ACK in [the store contract](stage-store-contract.md) are test evidence.
  The composition assumes successful commits survive caller loss and a status reader can authenticate the original binding.
- `stage_kernel.rs::accept` allows successive stages in a generation. Test `store.rs::bind_scope` currently rejects
  a generation at or below the scope head for a new native row. The test adapter in `stage_runtime_contract/runtime.rs`
  allows one native context per generation. Split generation creation from binding another command to an existing
  generation when implementing the selected contract. These fixtures cannot establish warm-path conformance.
- `src/worker_retirement.rs` seals an actor and waits on a tracker of direct children. That actor permit and the
  SDK's per-generation custody return have different owners and units. Direct-child exit, an unlocked shared
  descriptor, a missing result or guardian death cannot certify whole-group closure. The SDK supplies trusted
  generation and process-birth evidence, an adopted cleanup owner, private groups without escaped descendants,
  shared-descriptor semantics and serialized durable publication. OS evidence and independent review remain required.
- The bounded successor is admission and completion of a conflicting writer. Multiple simultaneously live instances,
  arbitrary duplicate frames, late ACK/expiry, frozen owners and full deadline arithmetic retain focused coverage in
  the historical modules. The healthy warm pair abstracts the two caller journals to a saved-first-result bit. This
  check does not establish refinement of the Rust server, crash-durable OS observations, multi-generation recovery
  or fairness of a general scheduler under unbounded arrivals.

## Run a check

The host configuration consumes `lean-rs` commit `ba5efb906b8855392b2d5b2a5074bb3497c7e290`,
`specs/worker-custody/WorkerCustody.tla` (SHA-256
`9ede3b6952a12baddbad63f0838ab0b5daf29d0899cf8306cb505088f8df8554`). The runner finds a sibling
`lean-rs` checkout or accepts `LEAN_RS_WORKER_CUSTODY` pointing to the exact file. It checks the digest
before passing those bytes to TLC. Historical-only runs require no SDK checkout.

From this directory, run the composed cases with `python3 check.py --model Stages --timeout 180`.

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

The last command runs the required host and historical cases, one at a time. Defaults are one
TLC worker, a 512 MiB Java heap limit and 30 seconds per case (180 seconds for the composed fault case). `--timeout SECONDS`
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

## SDK pin compatibility at ba5efb9

Compared with `21cd9f5`, the SDK adds two negative-control actions to `Next`. Each requires its own distinct
`Mode`. The first returns the durable Preparing reservation after possible spawn while the group and guardian
remain present. The second returns tracker capacity after direct-child exit while the guardian remains present.
Both guards are false in the host's good mode. SDK inputs, outputs and good-mode transitions used by the host
are unchanged. The previously recorded identity check below retains its original revision.

Seven focused host cases matched with TLC 2.19 and one worker. Identity exhausted 228 distinct states (564
generated), warm reuse exhausted 234 (577 generated), and combined faults exhausted 211,968 (1,221,970 generated).
Wrong ordinal and wrong generation each violated `SafeCommands`; stale reply violated `ReplyIdentity`; stale closure
violated `ClosureIdentity`. Each control stopped at its expected first violation. The host inputs were `Stages.tla`
SHA-256 `c9a0515722cdd3d32ef0d5fb2563179a3321cd7478f2ce566bf8a271742af177` and `cases.json` SHA-256
`e59d02fcffccc9fef773dfb283cdb9cf4126ff0c1917b9c679b3d9bb4a4f8aff`. This run covers the model interface
under its existing provenance and fairness assumptions. Native custody and release qualification remain open.

## Identity-interface check

On SDK `21cd9f5`, the positive `host-identity` case generated 564 states and exhausted 228 distinct states.
Its temporal property requires both exact companion replies, saved and acknowledged success, and acceptance of
the current generation's closure. The three controls failed the intended invariant:

| Weakened check | Observed violation |
| --- | --- |
| Compare command request, ordinal, scope and owner while omitting generation | `SafeCommands` caught execution of the misaddressed declaration command. |
| Accept a reply by ordinal | `ReplyIdentity` caught an old declaration reply assigned to the current declaration command. |
| Release host domains by scope and owner | `ClosureIdentity` caught acceptance of a retired generation's closure while the new reservation was still charged. |

All nine targeted cases matched their expected outcomes. The warm path exhausted 234 distinct states; the combined
fault case generated 1,221,970 and exhausted 211,968 distinct states. The three retained witnesses reached a lost
validated reply, an invalid second reply with cleanup failure, and known success while cleanup still held capacity.
The earlier required-suite receipt applies to the previous revision.

## Initial composed check at host 69f39a6 and SDK 21cd9f5

At that revision all 16 configured host cases produced their specified outcomes with TLC 2.19, Java 21, one worker and a 512 MiB heap
limit. Five exhausted the graph without a violation. The combined fault case generated **958,386** states and found
**169,248 distinct**, with an empty queue. Healthy companion and warm cases found 216 and 222 distinct states.
Five deliberate protocol mutations hit their named invariants. Six witnesses reached the requested cuts.

| Outcome | What the check distinguishes |
| --- | --- |
| Positive | Healthy companion success and ACK, two warm commands, exact-key retry, stale first-command delivery disabled by the endpoint guard, and combined fault safety/progress. |
| Negative | Early command or wrong command endpoint violates `SafeCommands`; changed-intent rebinding violates `ImmutableBinding`; early ACK or cleanup overwriting the primary violates `SafeStore`. |
| Witness | Partial acquisition rollback, lost validated reply before durable save, invalid second response with cleanup error, success while cleanup holds capacity, cancellation between commands, disjoint completion with orphan custody fenced. |

An earlier composition produced a successor-progress counterexample on an intermediate SDK snapshot
(`2ff97c6ba2589fcd11ae5181c66e6fdcde4c23d9d8a54402b61984503bc21c3c`).
Protected reads could be acquired after physical root loss. At retirement, a worker descriptor alias still held the lock,
so the waiting writer never acquired it. The committed SDK requires a present root and group when acquiring protected
reads. The unchanged successor-progress requirement passes on the pinned input. This is a model counterexample and
recheck; the failed macOS guardian-loss observation remains a separate gate.

## Models and cases

| Source | Scope |
| --- | --- |
| `Lifecycle.tla` | Two jobs/domains, lock holders, launch, descendants, crashes and conditional cleanup |
| `LifecycleScenarios.tla` | Frozen owners, original-parent authority, first-scope versus all-job closure, three broken protocols and concurrency witnesses |
| `Stages.tla` | Authoritative bounded host contract, composed with the pinned SDK worker custody actions |
| `HistoricalStages.tla` | Two immutable stages, duplicate/delayed messages, separate authorization/send/consume/spawn, cancellation, results and late callbacks |
| `CrossBoundary.tla` | Pending old GO, lost/frozen owner, explicit closure evidence and replacement generation |
| `DeadlinePolicy.tla` | Bounded hard/soft deadline policy, activity renewal, quiet events and missing samples |

`cases.json` holds each model's defaults and named overrides. Constants contain
TLA+ expressions. The runner generates configurations from those records; there
is no second set of checked-in `.cfg` files. It disables TLC's deadlock check
because completed models may make no further state change. The lifecycle cases
still check their named eventual-closure properties.

## Checked scope and remaining gaps

At the 2026-11-17 checkpoint, SANY parsed the five historical sources and the 155 required TLC
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

The historical coarse models retain the earlier supervisor/guardian alias arrangement.
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
