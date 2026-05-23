# Version matrix

| `lean-host-mcp` | `lean-rs` / `lean-rs-host` | Lean toolchain |
| --- | --- | --- |
| 0.1.1 | 0.1.3 | leanprover/lean4 v4.30.0-rc2 (pinned by `lean-rs`) |
| 0.1.0 | 0.1.x | leanprover/lean4 v4.29.x |

`lean-rs` declares its supported toolchain window in
[`lean-rs/lean-toolchain`](https://github.com/jcreinhold/lean-rs/blob/main/lean-toolchain). The MCP server inherits
whichever toolchain the consumer's Lake project pins, provided that toolchain is inside the `lean-rs` window.

Bumping the supported Lean toolchain is a `lean-rs` change first, then a crate-version bump here.
