---
name: find-crate-source
description: Find sources for a Rust crate from crates.io. Activate this skill to inspect a crate's API.
---

To find the source for a crate, use the `cargo agents` CLI tool available in the PATH:

* `cargo agents crate-info $CRATE_NAME` will give you the source path for the crate, preferring to use the version found in your current package. If the crate is not yet used, it will give you the latest version.
* `cargo agents crate-info $CRATE_NAME --version $VERSION` will give you the source path for a specific version
  * `$VERSION` is a semver constraint, so `1.0` will give you something compatible with `1.0`; `=1.0.0` is guaranteed to give you an exact version.
