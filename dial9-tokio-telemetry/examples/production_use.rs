//! This example discusses things to consider when using and setting up dial9 in production
//!
//! It also provides an opinionated set of knobs and environment variables to configure your application.
//!
//! Many applications can run with dial9 enabled all the time. Some applications will have worse performance, especially applications that perform
//! a very small amount of "useful work" per poll. Applications that use an extremely large number of worker threads will experience higher memory usage.
//! For an average production service, Dial9 produces 50-100GB / day.
//!
//! Since dial9 is recording an event on each poll (~50ns), if your polls are very short then this fix cost of overhead will impact your application performance.
//!
//! ### Enabling and Disabling
//! - If you use `Dial9ConfigBuilder::disabled()` dial9 is fully unused in your application and you have a normal, unmodified Tokio runtime.
//! - You can also install dial9 but leave it disabled. This has a slightly higher amount of overhead. All methods that would write dial9 data do a relaxed read on an atomic. This _will_
//!   install the runtime hooks but they will all be no-ops. You could then set up a background task that reads dynamic configuration to enable dial9 later. This is a much larger surface area of code
//!   that is enabled, so it is higher risk.
//!
//! > Note! dial9 must be created _before_ your async runtime. dial9 relies on installing itself into the runtime telemetry hooks to produce Tokio events.
//!
//! ### The overhead of running dial9
//!
//! 1. Dial9 allocates a 1MB buffer for each thread that record events. If you are recording events from a huge number of threads, this can bloat memory.
//! 2. When Tokio telemetry is enabled, 2 dial9 events will be emitted by every poll. If your poll times are extremely short and your application is CPU bound then this overhead can be significant.
//!
//! Dial9 has many possible components you can enable. The more components, the more data you will produce and the more overhead your application will have.
//!
//! #### CPU Profiling
//! Dial9 has two types of CPU profiling available:
//! 1. CPU profiling (this is what you would normally consider CPU profiling): Dial9 is sampling stack traces that are running on the CPU.
//! 2. Schedule Profiling: dial9 subscribes to the sched-switch linux kernel event and can capture a stack trace when your application is descheduled by the kernel. In order
//!    to subscribe to these events, dial9 must open one perf fd per worker thread.
//!
//! #### Tokio Telemetry
//! The basic Tokio telemetry will install a few hooks:
//! 1. `on_before_poll` and `on_after_poll` callbacks: This have a nanosecond level of overhead on each poll, just from the dynamic dispatch.
//! 2. `on_worker_park/unpark`: When these events happen, dial9 reads a few kernel APIs (~1us) to help to understand how Tokio is interacting with the OS.
//!
//! When you use dial9's spawn method, your futures are wrapped in a future that tracks `wake` events. This is done by instrumenting the waker. This is optional but
//! allows you to understand scheduling delay of the tasks you are running.
//!
//! #### Tracing
//! Dial9 can capture Tracing spans via the `TracingLayer`. On the scale of tracing, this is fairly low overhead, however, if you have a large amount of deeply nested spans, this can produce a huge amount
//! of data. We recommend using a very fine-grained filter.
//!
//! The rest of this example shows an opinionated way to wire dial9
//! so it can be enabled and tuned with CLI flags or environment variables
//! (via [`clap`]). The same binary can then run in dev, staging, and prod
//! with different tracing behavior, and tooling (CDK, Docker, k8s, etc.)
//! can flip knobs without a rebuild.
//!
//! ### Getting Useful Data
//!
//! To get the most use out of dial9, you need application-specific events in your traces to make sense of your data. The best way to do this is to emit some sort
//! of request id into your dial9 traces. There are a couple of ways to do this:
//!
//! 1. Use the `Dial9TokioLayer`, which is a tracing layer that allows dial9 to capture your tracing spans directly. Typically, you will
//!    select a narrow set of top-level spans to track.
//! 2. Emit an event when your request starts and when your request stops. Because these should _normally_ always be on the same Tokio Task, we can
//!    correlate post-hoc to figure exactly which polls belonged to which requests.
//!
//! # Configuration (CLI flags / environment variables)
//!
//! Every option can be set as a `--flag` or via its environment variable.
//! Run with `--help` for full usage.
//!
//! | Name                              | Default                         | Meaning                                                       |
//! | --------------------------------- | ------------------------------- | ------------------------------------------------------------- |
//! | `DIAL9_ENABLED`                   | `false`                         | Master switch. `true`/`false` only (Rust `bool::from_str`).   |
//! | `DIAL9_TRACE_DIR`                 | `/tmp/dial9-traces`             | Directory to write rotated trace segments into.               |
//! | `DIAL9_ROTATION_SECS`             | `60`                            | Wall-clock rotation period in seconds.                        |
//! | `DIAL9_MAX_DISK_USAGE_MB`         | `1024`                          | Upper bound on total on-disk bytes (old files evicted).       |
//! | `DIAL9_S3_BUCKET`                 | unset / empty                   | When set, sealed segments are gzip-uploaded to this bucket.   |
//! | `DIAL9_SERVICE_NAME`              | binary name                     | Service name used in the S3 key layout (required with S3).    |
//! | `DIAL9_CPU_PROFILE_ENABLED`       | `true` on Linux, `false` else   | Enables `perf_event_open`-based CPU sampling.                 |
//! | `DIAL9_CPU_SAMPLE_HZ`             | `99`                            | Sampling frequency for CPU profiling.                         |
//! | `DIAL9_SCHEDULE_PROFILE_ENABLED`  | `true` on Linux, `false` else   | Enables per-worker scheduler event capture (context switches).|
//!
//! # Invalid configuration
//!
//! Parsing is performed once, eagerly, before the runtime is built. On
//! invalid input (unknown boolean, non-numeric duration, etc.) debug builds
//! print clap's error and **exit** so the mistake is loud during development,
//! while release builds log a warning and fall back to a plain Tokio runtime
//! with telemetry disabled — a bad trace config must never take down prod.
//!
//! # Running the example
//!
//! ```sh
//! # plain run: telemetry disabled
//! cargo run --example production_use
//!
//! # basic local tracing (env var)
//! DIAL9_ENABLED=true cargo run --example production_use
//!
//! # basic local tracing (CLI flag)
//! cargo run --example production_use -- --enabled
//!
//! # with CPU profiling + schedule events (Linux, requires feature flag)
//! DIAL9_ENABLED=true \
//!   cargo run --features cpu-profiling --example production_use
//!
//! # with S3 upload (requires feature flag and AWS creds in env)
//! cargo run --features worker-s3 --example production_use -- \
//!   --enabled --s3-bucket my-trace-bucket --service-name my-service
//! ```

use std::time::Duration;

use clap::Parser;
use dial9_tokio_telemetry::config::{Dial9Config, Dial9ConfigBuilder};
use dial9_tokio_telemetry::telemetry::TelemetryHandle;

const LINUX: bool = cfg!(target_os = "linux");

/// Opinionated configuration for a production dial9 deployment.
///
/// All fields can be set via environment variables (shown in help) or CLI flags.
#[derive(Debug, Parser)]
struct Dial9Opts {
    /// Master switch for dial9 telemetry.
    #[arg(long, env = "DIAL9_ENABLED", default_value_t = false)]
    enabled: bool,

    /// Directory to write rotated trace segments into.
    #[arg(long, env = "DIAL9_TRACE_DIR", default_value = "/tmp/dial9-traces")]
    trace_dir: String,

    /// Wall-clock rotation period in seconds.
    #[arg(long, env = "DIAL9_ROTATION_SECS", default_value_t = 60)]
    rotation_secs: u64,

    /// Upper bound on total on-disk usage in MiB (old files evicted).
    #[arg(long, env = "DIAL9_MAX_DISK_USAGE_MB", default_value_t = 1024)]
    max_disk_usage_mb: u64,

    /// S3 bucket for uploading sealed segments (requires --service-name).
    #[arg(long, env = "DIAL9_S3_BUCKET", requires = "service_name")]
    s3_bucket: Option<String>,

    /// Service name used in the S3 key layout.
    #[arg(long, env = "DIAL9_SERVICE_NAME")]
    service_name: Option<String>,

    /// Enable perf_event_open-based CPU sampling (Linux only).
    #[arg(long, env = "DIAL9_CPU_PROFILE_ENABLED", default_value_t = LINUX)]
    cpu_profile_enabled: bool,

    /// Sampling frequency for CPU profiling.
    #[arg(long, env = "DIAL9_CPU_SAMPLE_HZ", default_value_t = 99)]
    cpu_sample_hz: u64,

    /// Enable per-worker scheduler event capture (Linux only).
    #[arg(long, env = "DIAL9_SCHEDULE_PROFILE_ENABLED", default_value_t = LINUX)]
    schedule_profile_enabled: bool,
}

impl Dial9Opts {
    fn rotation(&self) -> Duration {
        Duration::from_secs(self.rotation_secs)
    }

    fn max_disk_usage_bytes(&self) -> u64 {
        self.max_disk_usage_mb.saturating_mul(1024 * 1024)
    }

    /// Parse args/env or fall back to disabled configuration.
    ///
    /// Debug builds panic on invalid input so mistakes are obvious; release
    /// builds log a warning and fall back to disabled. A bad trace config
    /// should not prevent your service from running.
    fn parse_or_fallback() -> Self {
        match Self::try_parse() {
            Ok(opts) => opts,
            Err(e) if cfg!(debug_assertions) => e.exit(),
            Err(e) => {
                eprintln!("warning: invalid dial9 configuration: {e}; disabling telemetry");
                Self::parse_from::<[&str; 0], &str>([])
            }
        }
    }
}

/// Translate parsed options into a [`Dial9Config`] the `#[main]` macro can consume.
fn configure_dial9(opts: &Dial9Opts) -> Dial9Config {
    if !opts.enabled {
        return Dial9ConfigBuilder::disabled().build();
    }

    if let Err(e) = std::fs::create_dir_all(&opts.trace_dir) {
        eprintln!("warning: could not create {}: {e}", opts.trace_dir);
    }

    let base_path = format!("{}/trace.bin", opts.trace_dir.trim_end_matches('/'));
    let max_disk = opts.max_disk_usage_bytes();
    let max_file_size = (max_disk / 4).max(16 * 1024 * 1024);

    let builder = Dial9ConfigBuilder::new(base_path, max_file_size, max_disk)
        .rotation_period(opts.rotation())
        .with_runtime(|r| r.with_task_tracking(true));

    apply_s3(apply_cpu_profiling(builder, opts), opts).build()
}

/// CPU profiling is only compiled in with `--features cpu-profiling`. Without
/// it, the relevant env vars are ignored.
#[cfg(feature = "cpu-profiling")]
fn apply_cpu_profiling(builder: Dial9ConfigBuilder, opts: &Dial9Opts) -> Dial9ConfigBuilder {
    use dial9_tokio_telemetry::telemetry::cpu_profile::{CpuProfilingConfig, SchedEventConfig};
    let (cpu, sched, hz) = (
        opts.cpu_profile_enabled,
        opts.schedule_profile_enabled,
        opts.cpu_sample_hz,
    );
    builder.with_runtime(move |mut r| {
        if cpu {
            r = r.with_cpu_profiling(CpuProfilingConfig::default().frequency_hz(hz));
        }
        if sched {
            r = r.with_sched_events(SchedEventConfig::default());
        }
        r
    })
}

#[cfg(not(feature = "cpu-profiling"))]
fn apply_cpu_profiling(builder: Dial9ConfigBuilder, opts: &Dial9Opts) -> Dial9ConfigBuilder {
    if opts.cpu_profile_enabled || opts.schedule_profile_enabled {
        eprintln!(
            "warning: cpu/schedule profiling requested but --features cpu-profiling not enabled; ignoring"
        );
    }
    builder
}

#[cfg(feature = "worker-s3")]
fn apply_s3(builder: Dial9ConfigBuilder, opts: &Dial9Opts) -> Dial9ConfigBuilder {
    use dial9_tokio_telemetry::background_task::s3::S3Config;
    let (Some(bucket), Some(service_name)) = (opts.s3_bucket.clone(), opts.service_name.clone())
    else {
        return builder;
    };
    let s3 = S3Config::builder()
        .bucket(bucket)
        .service_name(service_name)
        .build();
    builder.with_runtime(|r| r.with_s3_uploader(s3))
}

#[cfg(not(feature = "worker-s3"))]
fn apply_s3(builder: Dial9ConfigBuilder, opts: &Dial9Opts) -> Dial9ConfigBuilder {
    if opts.s3_bucket.is_some() {
        eprintln!(
            "warning: DIAL9_S3_BUCKET set but --features worker-s3 not enabled; traces only on local disk"
        );
    }
    builder
}

fn my_config() -> Dial9Config {
    let opts = Dial9Opts::parse_or_fallback();
    eprintln!(
        "dial9 telemetry: {}",
        if opts.enabled {
            format!("enabled, writing to {}", opts.trace_dir)
        } else {
            "disabled (set DIAL9_ENABLED=true or --enabled to enable)".into()
        }
    );
    configure_dial9(&opts)
}

async fn workload_task(id: usize) {
    for i in 0..5 {
        tokio::time::sleep(Duration::from_millis(5)).await;
        if i == 0 && id.is_multiple_of(25) {
            println!("task {id} working");
        }
    }
}

#[dial9_tokio_telemetry::main(config = my_config)]
async fn main() {
    // `TelemetryHandle::try_current` returns `None` when telemetry is disabled,
    // so the same code path works in both modes.
    let tasks: Vec<_> = (0..100)
        .map(|i| match TelemetryHandle::try_current() {
            Some(handle) => handle.spawn(workload_task(i)),
            None => tokio::spawn(workload_task(i)),
        })
        .collect();
    for task in tasks {
        let _ = task.await;
    }
    println!("workload finished");
}
