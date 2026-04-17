# dial9-viewer

CLI tool that serves a web UI for browsing and viewing [dial9-tokio-telemetry](../dial9-tokio-telemetry) trace files stored in S3.

## Quick start

```bash
# Build
cargo build -p dial9-viewer

# Run with a default bucket
AWS_PROFILE=my-profile cargo run -p dial9-viewer -- serve --bucket my-trace-bucket

# Run with a bucket and prefix
AWS_PROFILE=my-profile cargo run -p dial9-viewer -- serve --bucket my-trace-bucket --prefix traces

# Custom port
cargo run -p dial9-viewer -- serve --port 8080 --bucket my-trace-bucket
```

Open `http://localhost:3000` to browse traces. Enter a search prefix (e.g. `2026-04-09/1910/checkout-api`), select one or more trace segments, and click "View Selected" to open them in the trace viewer.

## CLI

The binary has two subcommands: `serve` and `agents`.

### `serve`

Starts the web server.

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | `3000` | Port to listen on |
| `--bucket` | none | Default S3 bucket (can also be set per-request in the UI) |
| `--prefix` | none | Default S3 key prefix prepended to searches |
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

The `toolkit` subcommand extracts the bundled JS modules (`analyze.js`, `decode.js`, `trace_parser.js`, `trace_analysis.js`) so agents can run trace analysis locally with `node analyze.js <trace.bin>`.

## API

| Endpoint | Description |
|----------|-------------|
| `GET /api/search?q=<prefix>&bucket=<bucket>` | List S3 objects matching the prefix |
| `GET /api/trace?keys=<k1>&keys=<k2>&bucket=<bucket>` | Fetch, gunzip, and concatenate trace segments |

The trace endpoint returns raw binary data (`application/octet-stream`) suitable for loading directly in the trace viewer via `?trace=` URL parameter. Maximum response size is 50 MB.

## S3 key layout

The viewer expects the [time-first key layout](../dial9-tokio-telemetry/design/s3-worker-design.md) used by `dial9-tokio-telemetry`'s S3 worker:

```
{prefix}/{YYYY-MM-DD}/{HHMM}/{service}/{instance}/{boot_id}/{epoch}-{index}.bin.gz
```

Search by entering prefixes that match this structure, e.g.:
- `2026-04-09/` — all traces from April 9
- `2026-04-09/1910/` — traces from the 19:10 minute bucket
- `2026-04-09/1910/checkout-api/` — traces from checkout-api at 19:10

## Development

The UI is plain HTML/JS with no build step. Edit files in `ui/` and refresh the browser.

```bash
# Run the server (serves ui/ from disk — edit and refresh)
cargo run -p dial9-viewer -- serve --bucket my-bucket

# Or use serve.py for UI-only iteration (no backend)
./dial9-viewer/serve.py
```

The existing `dial9-tokio-telemetry/serve.py` still works for iterating on the trace viewer (`viewer.html`) without the S3 browser.

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
- Pluggable backends (local filesystem, GCS)
