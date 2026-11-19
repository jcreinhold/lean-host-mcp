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

1. Provision the sealed release and prepared shim descriptor using the SDK's
   original guarded application preparation and sealer. The metadata-only SDK CLI
   selects 9457921, whose host shim predates plugin import. The test adapter
   selects 7fe48a56, requires the plugin export, and skips the metadata worker. It needs its independently reviewed metadata
   source archive, approved bootstrap writer, and stock Lean 4.36.0 release.
   Those are external inputs. The SDK checks their hashes and creates the
   release lock, checked manifests and native image inventory in a private
   directory. Its bootstrap writer has its own fixed digest. A newly built host
   parent cannot substitute for that input. Keep the printed deployment path
   and use its `release` and `app/deployment.json` below.

   ```sh
   python3 -B "$TEST/provision.py" --sdk "$SDK" --packet "$PACKET" --writer "$BOOTSTRAP_WRITER" --lean-release "$STOCK_RELEASE" --scratch "$DEPLOYMENT" --target "$TARGET"
   ```

2. Build the two checked-in module sources in separate directories. The
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

3. Build the normal parent helper from a disposable checkout containing the
   reviewed guarded-spawn and loader changes. The test-only commit alone
   cannot supply this binary. Capture the source revision before selecting
   its private graph. Use an existing build target; monitor the free-space
   floor during compilation.

   ```sh
   git clone --no-hardlinks "$HOST_REPOSITORY" "$FRESH_HOST"
   git -C "$FRESH_HOST" checkout --detach "$REVIEWED_HOST_COMMIT"
   python3 "$TEST/select-helper-graph.py" "$FRESH_HOST" "$SDK" "$SEMANTIC" > "$FRESH_HOST/Cargo.toml.selected"
   mv "$FRESH_HOST/Cargo.toml.selected" "$FRESH_HOST/Cargo.toml"
   (cd "$FRESH_HOST" && LEAN_SYSROOT="$DEPLOYMENT/release" CARGO_TARGET_DIR="$TARGET" CARGO_INCREMENTAL=0 cargo build --offline -j1 -p lean-host-mcp --bin lean-host-mcp)
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
     --helper "$TARGET/debug/lean-host-mcp" --helper-sha256 "$REVIEWED_HELPER"
   ```

The SDK supplies worker protocol, artifact locks, writer/guardian dispatch,
metadata provisioning and image import. Test support owns only the application
attestation, the tiny query and replacement fixture, and observation of their
outcomes. The SDK metadata demo worker cannot grant this host's fixed query.
