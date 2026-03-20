# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
