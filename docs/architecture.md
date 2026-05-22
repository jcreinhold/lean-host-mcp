# Architecture

One crate, library + binary. Internal layering:

```
                ┌─────────────────────────────────────────┐
                │  main.rs (clap CLI + rmcp stdio entry)  │
                └────────────────────┬────────────────────┘
                                     │
                ┌────────────────────▼─────────────────────┐
                │  server.rs (LeanHostService, rmcp glue)  │
                └────────────────────┬─────────────────────┘
                                     │
                  ┌──────────────────┴──────────────────┐
                  ▼                                     ▼
            tools::lean                          tools::scan
            (six handlers)                       (one handler)
                  │
                  ▼
            session.rs
            (SessionHost, dedicated thread)
                  │
                  ▼
        lean-rs / lean-rs-host  (lifetime-bound, single-threaded)
```

## Why a dedicated session thread

`lean-rs` exposes `LeanRuntime`, `LeanHost`, `LeanCapabilities`,
`LeanSession`, and `SessionPool` as `!Send` types carrying a `'lean`
lifetime anchored to a process-global runtime. They cannot cross a
`tokio` task boundary; they cannot live inside an
`Arc<Mutex<…>>` accessed from multiple async tasks.

`SessionHost::spawn` creates one OS thread that initialises `LeanRuntime`,
opens the `LeanHost`, loads capabilities, and serves requests off a
`tokio::mpsc::Receiver` in a blocking loop. Tool handlers submit a
`Request` enum value and `await` a `oneshot` reply. From the caller's
side this looks like an async actor; the lifetime tangle stays hidden.

This is also the abstraction boundary that lets us swap the in-process
implementation for `lean-rs-worker` (subprocess isolation) later without
touching tool code — the `SessionHost` API is closure / channel shaped on
purpose.

## The envelope contract

Every tool wraps its result in `Response<T>` from `envelope.rs`. That
struct is the only thing the entire tool layer agrees on. The three
volatile decisions it hides are:

1. **What "freshness" means.** Today: lake root, imports, session id,
   toolchain label. Tomorrow we may add file-version vectors or build
   ids. Tools don't pick the shape.
2. **What a warning looks like.** A plain string today, but maybe a
   structured `{ code, message }` once we have a stable warning catalogue.
3. **Whether `next_actions` are present.** A hint surface for the LLM
   client. Tools sprinkle them; the envelope decides if/how to serialize.

## Why no SQLite index in v0.1

The plan called for one (under `index.rs`), backing `find_symbol`,
`find_lemma`, and `outline`. Building it from the live session needs
`LeanSession::list_declarations` to return strings; in published
`lean-rs` 0.1.x that call returns `Vec<LeanName>` and `LeanName` is an
opaque handle with no Rust-side `to_string`. Indexing 0 named rows isn't
useful, so the index module and its three tools land together in v0.2
once `lean-rs` exposes a `LeanName → String` rendering shim.

## Why no `lean-rs-worker` in v0.1

Subprocess isolation is real value, but pulling in `lean-rs-worker` means
authoring a separate worker child binary, wiring its framed-protocol
transport, and threading capability loading through the supervisor. None
of that earns its keep until we actually see a tool that wedges the
in-process Lean state. The `SessionHost` shape will accept a worker-backed
implementation later without rippling to tools.

## Why seven tools, not ten

The approved plan promised ten. `find_symbol`, `find_lemma`, and `outline`
all depend on enumerating declarations by string name; the published
`lean-rs` 0.1.x has no `LeanName → String` conversion, so the index those
tools would query has nothing to populate. The honest move is to ship the
seven that work today and sequence the other three behind an upstream
`lean-rs` shim. See the v0.2 roadmap in the README.
