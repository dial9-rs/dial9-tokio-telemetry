# dial9

[![Crates.io](https://img.shields.io/crates/v/dial9.svg)](https://crates.io/crates/dial9)
![License](https://img.shields.io/crates/l/dial9.svg)

CLI tool for viewing and analyzing [dial9-tokio-telemetry](https://crates.io/crates/dial9-tokio-telemetry) traces.

## Looking to instrument your application?

This crate is the **viewer/analysis CLI**. If you want to add dial9 telemetry to your Tokio application, you need [`dial9-tokio-telemetry`](https://crates.io/crates/dial9-tokio-telemetry):

```toml
[dependencies]
dial9-tokio-telemetry = "0.3"
```

See the [dial9-tokio-telemetry README](https://github.com/dial9-rs/dial9-tokio-telemetry/tree/main/dial9-tokio-telemetry) for setup instructions.

## Installation

Pre-built binaries are available from [GitHub Releases](https://github.com/dial9-rs/dial9-tokio-telemetry/releases) for Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows (x86_64).

```bash
# From source via crates.io
cargo install --locked dial9

# Or with cargo-binstall (downloads a pre-built binary, faster)
cargo binstall dial9
```

## Usage

The binary has two subcommands: `serve` and `agents`. Run `dial9 --help` or `dial9 <subcommand> --help` for full options.

### `serve`

Starts a web server for browsing and viewing traces from S3 or the local filesystem.

```bash
# Serve traces from a local directory
dial9 serve --local-dir /tmp/my_traces

# Serve traces from S3
AWS_PROFILE=my-profile dial9 serve --bucket my-trace-bucket
```

Open `http://localhost:3000` to browse traces. Enter a search prefix (e.g. `2026-04-09/1910/checkout-api`), select one or more segments, and click "View Selected" to open them in the viewer.

### `agents`

Manages skill documentation and the JS analysis toolkit for AI agents.

```bash
# Print the agent skill header
dial9 agents

# Print a specific skill segment
dial9 agents skill recipes

# Unpack all skills as an Agent Skills spec directory (for native skill loading)
dial9 agents skills /tmp/dial9-skills

# Extract the JS analysis toolkit to a directory
dial9 agents toolkit /tmp/dial9-toolkit
```

If you use [Symposium](https://symposium.dev), skills auto-install when your project depends on `dial9-tokio-telemetry`:

```bash
cargo agents sync
```
