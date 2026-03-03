# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
