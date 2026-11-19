"""Select local dependencies in the explicitly disposable matched parent checkout."""

import json
import re
import subprocess
import sys
from pathlib import Path

host, sdk, semantic = (Path(value).resolve(strict=True) for value in sys.argv[1:])
for repo, commit in [
    (sdk, "7fe48a56a3091e05e99a024e3f5f524732a82ed6"),
    (semantic, "fc03e559a8f25d6b1b510aadb10b7c3809d39c91"),
]:
    git = ["git", "-C", str(repo)]
    if subprocess.check_output(
        git + ["rev-parse", "HEAD"], text=True
    ).strip() != commit or subprocess.check_output(git + ["status", "--porcelain"]):
        raise SystemExit(f"expected clean pinned source: {repo}")
# Print a scratch manifest. The caller creates the disposable source export.
manifest = (host / "Cargo.toml").read_text()
for name in (
    "lean-rs-worker-child",
    "lean-rs-worker-parent",
    "lean-rs-worker-protocol",
    "lean-toolchain",
    "lean-semantic-search-capability",
    "lean-semantic-search-contract",
    "lean-semantic-search-retrieval",
    "lean-semantic-search-runtime",
):
    path = (
        semantic / "crates" / name.split("-")[-1]
        if name.startswith("lean-semantic")
        else sdk / "crates" / name
    )
    line = f"{name} = {{ path = {json.dumps(str(path))} }}"
    manifest, count = re.subn(
        rf'(?m)^{name} = "[^"]+"$',
        lambda _, line=line: line,
        manifest,
    )
    if count != 1:
        raise SystemExit(f"unrecognized workspace dependency {name}")
print(manifest + "\n[patch.crates-io]")
for name in ("lean-rs-worker-protocol", "lean-toolchain"):
    print(f"{name} = {{ path = {json.dumps(str(sdk / 'crates' / name))} }}")
