# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.6](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.5...dial9-tokio-telemetry-v0.3.6) - 2026-04-30

### Added

- Store S3 metadata into segement metadata ([#311](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/311))
- detect uninstrumented task spawns and surface in viewer ([#293](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/293))
- Add simple example for local execution ([#306](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/306)) — thanks @mox692!

### Fixed

- Don't register sched events on blocking pool threads ([#316](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/316))
- *(viewer)* correct schedWait unit from µs to ns ([#308](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/308))

### Other

- Fix thread CPU time measurement details in README ([#312](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/312))
- Fix ctimer test on AL2 ([#317](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/317))
- Allow opening .gz trace files in the file picker ([#315](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/315))
- [dial9-viewer] Toolkit: parallel multi-file trace analysis with caching ([#298](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/298))
- Retain parent stack trace when zooming into flamegraph frames ([#305](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/305))
- Retain selection overlay while sidebar is open ([#304](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/304))
- (viewer) Move flamegraph into sidebar instead of full-screen overlay ([#291](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/291))

## [0.3.5](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.4...dial9-tokio-telemetry-v0.3.5) - 2026-04-24

### Added

- *(viewer)* resizable sidebar and slightly improved ux ([#290](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/290))

### Fixed

- *(ci)* use single cargo package invocation for cross-crate verification

### Other

- lower expected sample threshold in ctimer cpu load test ([#292](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/292))
- Restore collect_files safety limits lost by merge-queue bug ([#276](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/276)) ([#295](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/295))
- restore ([#286](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/286))
- Fix stack overflow on large profiles ([#285](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/285))

## [0.3.4](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.3...dial9-tokio-telemetry-v0.3.4) - 2026-04-23

### Added

- add CPU profiling fallback for perf-restricted environments. This should enable CPU profiling to work in Fargate. ([#250](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/250))
- *(viewer)* replace stack trace popup with right sidebar panel ([#274](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/274))
- *(viewer)* Pop-out flamegraph with interactive features ([#269](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/269))

### Fixed

- fix sort order of polls with cpu samples ([#272](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/272))

### Other

- Make worker lanes scrollable in viewer ([#275](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/275))
- Fix docs.rs broken links by using absolute GitHub URLs ([#277](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/277))

## [0.3.3](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.2...dial9-tokio-telemetry-v0.3.3) - 2026-04-20

### Other

- tighten README prose for readability and conciseness ([#265](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/265))

## [0.3.2](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.1...dial9-tokio-telemetry-v0.3.2) - 2026-04-20

### Other

- crosslink dial9-viewer from the readme ([#262](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/262))

## [0.3.1](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.3.0...dial9-tokio-telemetry-v0.3.1) - 2026-04-19

### Added

- **Tracing layer** ([#252](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/252)): `Dial9TokioLayer` records `tracing` span enter/exit events, including field values, into the trace, showing what happened inside each poll. Enable with the `tracing-layer` feature flag.

The viewer (`dial9-viewer serve`) shows spans in a dedicated panel with filtering, percentile ranking, and click-to-highlight. The agent analysis toolkit (`dial9-viewer agents`) includes span correlation recipes and automated span checks in the red-flags scan.

```rust,ignore
use dial9_tokio_telemetry::tracing_layer::Dial9TokioLayer;
use tracing_subscriber::prelude::*;

tracing_subscriber::registry()
    .with(Dial9TokioLayer::new().with_filter(
        tracing_subscriber::filter::Targets::new()
            .with_target("my_app", tracing::Level::TRACE)
            .with_default(tracing::Level::ERROR),
    ))
    .init();
```

Tracing support means you can attach a request ID or other context to spans via `#[instrument(fields(request_id = %id))]` and then search for specific requests in the trace. You can also see what's happening inside long polls: if a single poll contains many small operations without yielding, the span breakdown shows exactly where the time went. 

Standard `tracing-subscriber` filtering rules apply. Without a filter, libraries like the AWS SDK will flood the trace with internal spans. The preceding captures only spans from `my_app`.

## [0.3.0](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.2.0...dial9-tokio-telemetry-v0.3.0) - 2026-04-17

Big release. The setup story is much better, there's support for tracing multiple runtimes, you can emit your own events into the trace, and the viewer is its own crate now. 

### `#[dial9_tokio_telemetry::main]` macro ([#212](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/212))

Drop-in replacement for `#[tokio::main]`. Point it at a config function and you're done. Unlike `#[tokio::main]`, the macro spawns your function body as a task, so top-level code shows up in traces.

```rust
use dial9_tokio_telemetry::config::{Dial9Config, Dial9ConfigBuilder};
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

fn my_config() -> Dial9Config {
    Dial9ConfigBuilder::new("trace.bin", 64 * 1024 * 1024, 256 * 1024 * 1024)
        .with_runtime(|r| r.with_task_tracking(true))
        .build()
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    let handle = TelemetryHandle::current();
    handle.spawn(async { /* wake events tracked */ }).await.unwrap();
}
```

### Multiple runtime support ([#141](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/141), [#193](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/193))

If you run separate runtimes for request handling and background IO (or thread-per-core, etc.), you can now attach them all to one telemetry session with `TelemetryCore`. Workers are grouped by runtime name in the viewer.

```rust
use dial9_tokio_telemetry::telemetry::{RotatingWriter, TelemetryCore};

let writer = RotatingWriter::builder()
    .base_path("/tmp/traces/trace.bin")
    .max_file_size(100 * 1024 * 1024)
    .max_total_size(500 * 1024 * 1024)
    .build()?;

let guard = TelemetryCore::builder().writer(writer).build()?;
guard.enable();

let mut main_builder = tokio::runtime::Builder::new_multi_thread();
main_builder.worker_threads(4).enable_all();
let (main_rt, main_handle) = guard.trace_runtime("main").build(main_builder)?;

let mut io_builder = tokio::runtime::Builder::new_multi_thread();
io_builder.worker_threads(2).enable_all();
let (io_rt, io_handle) = guard.trace_runtime("io").build(io_builder)?;
```

### `dial9-viewer` crate ([#177](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/177))

The trace viewer is its own crate now: `cargo install dial9-viewer`. It serves the interactive HTML viewer locally and can browse traces on S3.

### Custom application events ([#196](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/196), [#216](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/216), [#218](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/218))

You can emit your own events into the trace. Derive `TraceEvent`, call `record_event`. They are not currently visible in the viewer although they can be loaded from the trace via the JS parser directly. Repeated string values (HTTP methods, paths, etc.) can be interned to save space on the wire.

```rust
use dial9_trace_format::TraceEvent;
use dial9_tokio_telemetry::telemetry::{record_event, clock_monotonic_ns, TelemetryHandle};

#[derive(TraceEvent)]
struct RequestCompleted {
    #[traceevent(timestamp)]
    timestamp_ns: u64,
    status_code: u32,
    latency_us: u64,
    error_message: Option<String>,
}

record_event(
    RequestCompleted {
        timestamp_ns: clock_monotonic_ns(),
        status_code: 200,
        latency_us: 1500,
        error_message: None,
    },
    &handle,
);
```

### Trace file concatenation ([#134](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/134))

Uncompressed trace files can be concatenated (`cat trace.0.bin trace.1.bin > combined.bin`) and loaded as a single trace. The decoder resets parser state at segment boundaries via reset frames.

### Added

- Time-based rotation for `RotatingWriter`: segments rotate on wall-clock boundaries (e.g. every 60s), which gives clean S3 key paths ([#136](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/136), [#179](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/179))
- `boot_id` in default S3 key layout so segments from different process starts don't collide ([#225](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/225), [#237](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/237))
- Sampling for scheduler events: record 1-in-N context switches via `SchedEventConfig::sampling_interval` ([#233](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/233))
- Optional field type modifiers and named field decode in trace format ([#216](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/216))

### Fixed

- Segments now contain all data for their time range. Previously, thread-local buffers could drain mid-rotation, causing events from one wall-clock period to land in the wrong segment (up to 8s of timestamp overlap between adjacent files). Rotation now coordinates with the flush loop: bump epoch, drain all buffers, flush, then rotate ([#224](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/224), [#186](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/186))
- Traces now include monotonic-to-realtime clock sync frames for precise wall-clock alignment ([#210](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/210), [#214](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/214))
- `perf-self-profile` compiles on macOS ([#174](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/174))
- Background worker no longer busy-loops re-processing already gzipped segments ([#154](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/154), [#155](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/155))
- `single_file` writer uses `.active` suffix so the background worker can symbolize and gzip sealed segments ([#164](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/164))
- Empty segments are no longer sealed on finalize ([#127](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/127))
- Blocking thread TIDs are captured directly instead of guessed by name ([#120](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/120))
- Rate-limited internal tracing/logging to prevent log spam ([#209](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/209))

### Viewer

- Binary search for CPU sample attachment, faster rendering ([#201](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/201), [#143](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/143))
- Relative/absolute time toggle ([#146](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/146))
- Gzip decompression in JS parser: load `.bin.gz` files directly ([#178](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/178))
- Escape stack frame names in flamegraph tooltips ([#142](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/142))
- Handle truncated frames without crashing ([#98](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/98))

### Breaking changes

- `SamplerConfig` and `CpuProfilingConfig` now use builders instead of struct-literal construction, consistent with `SchedEventConfig` ([#244](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/244))

### Internal

- Removed recorder mutex; events encode directly into thread-local buffers ([#122](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/122), [#133](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/133), [#135](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/135))
- `graceful_shutdown` is synchronous now ([#151](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/151))
- Public API cleanup and lints ([#175](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/175), [#129](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/129))
- Bencher integration for continuous overhead tracking ([#150](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/150))

## [0.2.0](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.1.1...dial9-tokio-telemetry-v0.2.0) - 2026-03-20

0.2.0 brings two major improvements:
1. Support for publishing traces to S3
2. Migration to the new trace format (dial9-trace-format). This format is self describing, extremely compact, compressible and fast to write. This will set us up to easily add application level telemetry in the future.

For setting it up in production applications, the new `.install(true/false)` method makes it easy to have a single instantiation path for your runtime but set `install(false)` to make dial9 a complete no-op.

### Added

- Wire background symbolization into the flush/worker pipeline ([#95](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/95))
- Improve s3 writer's configuration API ([#86](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/86))
- add install() builder method for conditional telemetry install ([#85](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/85))
- Add ProcMaps frame and offline symbolizer for background symbolization ([#87](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/87))

### Fixed

- offline symbolization cleanups and optimizations ([#111](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/111))
- write segment metadata at the beginning of the file and add RotatingWriterBuilder ([#115](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/115))
- Bring back support for locations in offline symbolization ([#110](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/110))
- stop writing trailing garbage in gzip segments after graceful_shutdown ([#104](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/104))
- Fix worker spin-loop on gzip-compressed and permanently failing segments ([#102](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/102))
- *(trace_viewer)* update format name from TOKIOTRC to D9TF in landing screen ([#103](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/103))
- *(js-decoder)* handle truncated frames gracefully, read symbol frames even if >= MAX_EVENTS ([#98](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/98))
- clarify S3 key layout is the default, not the only option ([#89](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/89))
- add missing crates.io metadata ([#84](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/84))
- thread-local buffer not flushing on drop ([#54](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/54))

### Other

- *(trace-parser)* consolidate per-branch cap checks into early continue ([#116](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/116))
- fix flaky worker park test ([#117](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/117))
- Harden flush path with ArrayQueue & emit metrics ([#97](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/97))
- Update demo trace to have symbols ([#105](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/105))
- add kptr_restrict guidance for kernel symbol resolution ([#99](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/99))
- Switch to new trace format ([#91](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/91))
- Prepare to migrate to dial9-trace-format ([#76](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/76))
- Add kernel tracepoint support to perf-self-profile ([#81](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/81))
- document tokio_unstable prerequisite for downstream consumers ([#90](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/90))
- Support kernel frames in callchains when include_kernel=true ([#77](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/77))
- Enable perf_event_open tests in CI ([#78](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/78))
- S3 reporter ([#60](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/60))
- Viewer UX: hint toasts, help overlay, keyboard accessibility ([#69](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/69))
- Backport analysis algorithms from trace viewer into core ([#35](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/35)) ([#63](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/63))
- Add SegmentMetadata event (wire code 11) ([#66](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/66))
- Remove SimpleBinaryWriter, use RotatingWriter everywhere ([#65](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/65))
- Track task spawn/terminate events and show active task count in trace… ([#48](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/48))
- Track blocking pool threads in CPU profiler via ThreadRole ([#52](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/52))
- Docs improvements & fix ui paper cuts ([#57](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/57))

## [0.1.1](https://github.com/dial9-rs/dial9-tokio-telemetry/compare/dial9-tokio-telemetry-v0.1.0...dial9-tokio-telemetry-v0.1.1) - 2026-03-03

- Fix trace viewer crash when loading trace from URL parameter ([#42](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/42))
- Improve symbolization and include docs.rs links in call frames ([#39](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/39))
- Add demo trace ([#40](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/40))
- fix: take_rotated() was inside debug_assert, never ran in release builds ([#41](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/41))

## [0.1.0](https://github.com/dial9-rs/dial9-tokio-telemetry/releases/tag/dial9-tokio-telemetry-v0.1.0) - 2026-03-01

### Other

- Update readme and allow tests to pass on macOS ([#22](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/22))
- Add Cloudflare Workers configuration ([#29](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/29))
- Support Compilation on MacOS ([#16](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/16))
- Enable CPU profiling in metrics service and extract client binary ([#12](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/12))
- Integrate CPU profiling into Dial9 ([#11](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/11))
- Initial implementation of tracking task wakes ([#4](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/4))
- Convert to workspace, move crate into dial9-tokio-telemetry/ ([#5](https://github.com/dial9-rs/dial9-tokio-telemetry/pull/5))
