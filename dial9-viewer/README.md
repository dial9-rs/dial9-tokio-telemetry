# dial9-viewer

CLI tool that serves a web UI for exploring [dial9-tokio-telemetry](../dial9-tokio-telemetry) trace files stored in S3 or on the local filesystem.

## Installation

Pre-built binaries are available from [GitHub Releases](https://github.com/dial9-rs/dial9-tokio-telemetry/releases) for Linux (x86_64, aarch64), macOS (x86_64, aarch64), and Windows (x86_64).

```bash
# From source via crates.io
cargo install --locked dial9-viewer

# Or with cargo-binstall (downloads a pre-built binary, faster)
cargo binstall dial9-viewer
```

## Quick start

```bash
# Build
cargo build -p dial9-viewer

# Serve traces from a local directory (no AWS credentials needed)
cargo run -p dial9-viewer -- serve --local-dir /tmp/my_traces

# Serve traces from S3
AWS_PROFILE=my-profile cargo run -p dial9-viewer -- serve --bucket my-trace-bucket

# Serve traces from S3 with a key prefix
AWS_PROFILE=my-profile cargo run -p dial9-viewer -- serve --bucket my-trace-bucket --prefix traces

# Custom port
cargo run -p dial9-viewer -- serve --port 8080 --bucket my-trace-bucket
```

Open `http://localhost:3000` to browse traces. Enter a search prefix (e.g. `2026-04-09/1910/checkout-api`), select one or more segments, and click "View Selected" to open them in the viewer.

## CLI

The binary has two subcommands: `serve` and `agents`.

### `serve`

Starts the web server.

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | `3000` | Port to listen on |
| `--bucket` | none | Default S3 bucket (can also be set per-request in the UI) |
| `--prefix` | none | Default S3 key prefix prepended to searches |
| `--local-dir` | none | Serve traces from a local directory instead of S3 |
| `--ui-dir` | `ui` | Directory containing static UI files |

### `agents`

Provides skill documentation and an analysis toolkit for AI agents working with dial9 traces.

```bash
# Print the agent skill header (overview + available segments)
cargo run -p dial9-viewer -- agents

# Print a specific skill segment
cargo run -p dial9-viewer -- agents skill loading
cargo run -p dial9-viewer -- agents skill recipes

# Extract the JS analysis toolkit to a directory
cargo run -p dial9-viewer -- agents toolkit /tmp/dial9-toolkit
```

Available skill segments:

| Segment | Description |
|---------|-------------|
| `loading` | Trace format details, parsing options, time range filtering |
| `analysis` | Full analysis pipeline API reference |
| `recipes` | Diagnostic recipes for common questions |
| `red-flags` | Automated checks for common runtime problems |

The `toolkit` subcommand extracts bundled JS modules (`analyze.js`, `decode.js`, `trace_parser.js`, `trace_analysis.js`) so agents can analyze traces locally via `node analyze.js <trace.bin>`.

## API

| Endpoint | Description |
|----------|-------------|
| `GET /api/search?q=<prefix>&bucket=<bucket>` | List S3 objects matching the prefix |
| `GET /api/trace?keys=<k1>&keys=<k2>&bucket=<bucket>` | Fetch, decompress, and concatenate trace segments |

The trace endpoint returns raw binary data (`application/octet-stream`) suitable for loading directly in the viewer via the `?trace=` URL parameter. Maximum response size is 50 MB.

## S3 key layout

The viewer expects the [time-first key layout](../dial9-tokio-telemetry/design/s3-worker-design.md) used by `dial9-tokio-telemetry`'s S3 worker:

```
{prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{instance}/{boot_id}/{epoch}-{index}.bin.gz
```

Search by entering prefixes that match this structure, e.g.:
- `2026-04-09/` — all traces from April 9
- `2026-04-09/1910/` — traces from the 19:10 minute bucket
- `2026-04-09/1910/checkout-api/` — traces from checkout-api at 19:10

## Local directory mode

For local development, point the viewer at your dial9 traces directory instead of S3:

```bash
cargo run -p dial9-viewer -- serve --local-dir /tmp/my_traces
```

Files under the directory are served recursively. Search, prefix browsing, and trace viewing all work the same way — no AWS credentials needed.

## Development

The UI is plain HTML/JS with no build step. Edit files in `ui/` and refresh the browser.

```bash
# Run the server (serves ui/ from disk — edit and refresh)
cargo run -p dial9-viewer -- serve --bucket my-bucket

# Or use serve.py for UI-only iteration (no backend)
./dial9-viewer/serve.py
```

`../dial9-tokio-telemetry/serve.py` still works for iterating on the trace viewer (`viewer.html`) without the S3 browser.

## Testing

```bash
cargo nextest run -p dial9-viewer
```

Integration tests use [s3s](https://docs.rs/s3s/) to run a fake S3 server in-process.

## Future enhancements

- Structured search query parser (e.g. `19:10-19:20 checkout-api`)
- Bucket listing endpoint and dropdown
- Rich result metadata (service, instance, timestamp columns)
- Deep linking with time range parameters
- Pluggable backends (GCS)
