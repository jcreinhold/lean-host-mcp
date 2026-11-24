# Darwin plugin worker regression candidate

Run from an idle, matched build lane with a prepared Lean 4.36.0 deployment.
Supply independent local digests for the deployment record, selected plugin,
selected module, alternate module and matched host helper. Use a clean checkout
of the pinned SDK, an existing Cargo target and a fresh scratch path.
Run `python3 tests/mac-plugin/run.py --help` for the input flags. Keep the scratch directory on failure for custody inspection.

The test builds the link-free controller and the linked worker separately. The
test worker installs the fixed deployment authority and calls the actual host
worker main. SharedWriter and native guardian dispatch use the supplied host
binary. The child checks the artifact inventory, mapped image and opened root
order under read protection. Only the fixed diagnostics query is granted.
Replacement and restoration go through the normal writer. Each observation
opens a fresh handle, with no retained request ticket or broker cache.

Building the matched host parent is a prerequisite. The published dependency
pins on this host base predate typed startup custody. An earlier helper or a
private SDK overlay cannot qualify this candidate.

## Rebuilding and reviewing inputs on an arm64 Mac

Use fresh private directories on a volume with at least 25 GiB free. Set
`SDK` to a clean checkout of 7fe48a56, `SEMANTIC` to fc03e559 and `TEST`
to this directory. Use a trusted build environment and one Cargo job. Keep the
private release, toolchain, shim trees, input files and helper stable while
reviewing and running. The following are separate producer and consumer steps.

1. Select the previously reviewed matched parent helper if available. For a
   fresh build, use the reviewed stock Lean release and the following steps.
   The helper source is host commit `bba2ec8cd886107840741b427595c17e9ccc8141`
   (guarded startup and loader policy). The host repository keeps this commit in
   the history of `main`. Use a full clone to access it. Keep the selected
   scratch manifest outside the checkout until its clean revision is verified.

   ```sh
   git clone --no-hardlinks "$HOST_REPOSITORY" "$FRESH_HOST"
   git -C "$FRESH_HOST" checkout --detach bba2ec8cd886107840741b427595c17e9ccc8141
   python3 "$TEST/select-helper-graph.py" "$FRESH_HOST" "$SDK" "$SEMANTIC" > "$FRESH_HOST.selected.toml"
   mv "$FRESH_HOST.selected.toml" "$FRESH_HOST/Cargo.toml"
   (cd "$FRESH_HOST" && LEAN_SYSROOT="$STOCK_RELEASE" CARGO_TARGET_DIR="$TARGET" CARGO_INCREMENTAL=0 cargo build --offline -j1 -p lean-host-mcp --bin lean-host-mcp)
   ```

2. Independently review the resulting local helper and select its SHA-256.
   Provisioning stages those verified bytes privately. Regression takes its
   helper from that same deployment and checks the same approved digest.
   The SDK owns the ten application files at `fixtures/metadata-qualification/app`.
   Supply the SDK repository containing commit
   `a091e4a9427040c5741c8f84f9508fa8b7aed665` separately from the 7fe48a56
   runtime checkout. The adapter verifies the subtree identity,
   exports its original file bytes and requires the plugin-capable host shim.

   ```sh
   python3 -B "$TEST/provision.py" --sdk "$SDK" --fixture-repo "$FIXTURE_SDK_REPO" --writer "$TARGET/debug/lean-host-mcp" --writer-sha256 "$REVIEWED_HELPER" --lean-release "$STOCK_RELEASE" --scratch "$DEPLOYMENT" --target "$TARGET"
   ```

3. Build the two checked-in module sources in separate directories. The
   regression replaces only the .olean, so only the selected variant needs a
   dylib. Review the source, compiler/release identity, exported
   `_initialize_QualifiedPlugin`, every load command and its resolved libraries,
   the selected witness and initializer, and the distinct alternate witness.
   The reviewed plugin must have no unapproved loader dependencies, callbacks
   or search paths. Retain the local hashes after that review.

   ```sh
   sh "$TEST/build-inputs.sh" "$DEPLOYMENT/release" "$FRESH_INPUTS"
   /usr/bin/otool -l "$FRESH_INPUTS/selected/libQualifiedPlugin.dylib"
   ```

4. Independently review the **local** sealed record (including shim/release
   identity and custody), matched helper, and the three module/image artifacts.
   Supply their approved SHA-256 values explicitly. Copying digests from the
   producer without this review does not establish the app attestation.
   The worker checks the new project's file and directory identities under
   custody. It uses the reviewed packet's shim and release entries unchanged.

   ```sh
   python3 "$TEST/run.py" --sdk "$SDK" --target "$TARGET" --scratch "$FRESH_RUN" \
     --deployment "$DEPLOYMENT/app/deployment.json" --deployment-sha256 "$REVIEWED_DEPLOYMENT" \
     --selected "$FRESH_INPUTS/selected" --module-sha256 "$REVIEWED_MODULE" --plugin-sha256 "$REVIEWED_PLUGIN" \
     --alternate "$FRESH_INPUTS/alternate/QualifiedPlugin.olean" --alternate-sha256 "$REVIEWED_ALTERNATE" \
     --helper-sha256 "$REVIEWED_HELPER"
   ```

The SDK supplies worker protocol, artifact locks, writer/guardian dispatch,
metadata provisioning and image import. Test support owns only the application
attestation, the tiny query and replacement fixture, and observation of their
outcomes. The SDK metadata demo worker cannot grant this host's fixed query.
