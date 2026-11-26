# Lean test fixture

`LeanRsFixture` is the Lake package used by the server's opt-in integration and performance tests. Its modules cover ABI
values, handles, capabilities, source ranges, declaration queries, proof actions, and proof search. The umbrella module
lists the current cases.

## Build and test

With the pinned Lean toolchain installed:

```sh
cd fixtures/lean
lake build
```

The package is `lean_rs_fixture`; its library and umbrella module are `LeanRsFixture`. Compiled imports land under
`.lake/build/lib/lean/`. The lakefile also builds a shared facet for direct library-loading tests; the MCP server uses
imported `.olean` files and the host shims bundled with `lean-rs-host`.

From the repository root, install the matching worker and run the opt-in server tests:

```sh
lean-host-mcp install --lake-root fixtures/lean
LEAN_HOST_MCP_TEST_FIXTURE="$PWD/fixtures/lean" \
  cargo test -p lean-host-mcp --test e2e -- --ignored
```

`install` reads the fixture's `lean-toolchain` and installs under `fixtures/lean/.local/lean-host-mcp/`, which Git
ignores; the tests find the worker there. For a worker built in this checkout, set `LEAN_HOST_MCP_WORKERS_DIR` to its
build directory instead. When testing a non-default Lean toolchain, set `LEAN_SYSROOT` for the entire Rust
build-and-test invocation so the compiled header fingerprint matches the worker. See
[development guidance](../../AGENTS.md) and [operations](../../docs/operations.md#installing-workers).

Consumer projects need their own lakefile, `lean-toolchain`, and built imports. They do not need to copy this fixture,
use a shared facet, or declare the host shim package.
