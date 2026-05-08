---
name: rust-best-practice
description: "[Critical] Best practice for Rust coding. Always activate this skill before authoring Rust code or answering questions about Rust."
---

# Important dos and don'ts for working with Rust source code

* Use `cargo add` to add new dependencies or features rather than editing `Cargo.toml` directly
* Before finishing your turn, when editing Rust code:
  * Run `cargo fmt` after modifying Rust source files to ensure consistent formatting
  * Run tests (most commonly `cargo test --all --workspace`, though some projects may have other commands)
