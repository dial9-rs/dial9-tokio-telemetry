//! Unified configuration for the `#[dial9_tokio_telemetry::main]` macro.
//!
//! Start with [`Dial9Config::builder()`] and chain setters to produce a
//! config that the macro (or [`crate::TracedRuntime`]) consumes. The
//! builder stages a [`tokio::runtime::Builder`] and accumulates
//! [`TracedRuntimeBuilder`] configurators eagerly; use
//! [`Dial9ConfigBuilder::with_tokio`] and
//! [`Dial9ConfigBuilder::with_runtime`] to reach any knob those builders
//! expose.
//!
//! Two finish functions cover the strict / lenient axis:
//!
//! - [`Dial9ConfigBuilder::build`] — strict. Returns a
//!   `Result<Dial9Config, Dial9ConfigBuilderError>`. Both required-field
//!   validation and the writer's I/O probing happen here, so any error
//!   surfaces at config-build time before the runtime is touched.
//! - [`Dial9ConfigBuilder::build_or_disabled`] — lenient. Returns a
//!   [`Dial9Config`] that is *infallible at build time*: validation or
//!   I/O failures are logged at `error!` level and downgraded to a
//!   disabled config that still carries the user's `with_tokio`
//!   configurators.
//!
//! To run without telemetry while preserving tokio knobs, call
//! `.enabled(false)` — the builder then skips required-field validation
//! and any queued runtime configurators are ignored.

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::telemetry::recorder::{
    HasTracePath, PipelineUnset, TelemetryGuard, TracedRuntime, TracedRuntimeBuilder,
};
use crate::telemetry::writer::RotatingWriter;

/// Type-erased terminal step for a [`TracedRuntimeBuilder`]: hides the
/// pipeline-mode marker `M` so [`Inner::Enabled`] can stay non-generic.
pub(crate) trait BuildTracedRuntime: Send {
    fn build_and_start(
        self: Box<Self>,
        tokio_builder: tokio::runtime::Builder,
        writer: RotatingWriter,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)>;
}

impl<M: Send + 'static> BuildTracedRuntime for TracedRuntimeBuilder<HasTracePath, M> {
    fn build_and_start(
        self: Box<Self>,
        tokio_builder: tokio::runtime::Builder,
        writer: RotatingWriter,
    ) -> std::io::Result<(tokio::runtime::Runtime, TelemetryGuard)> {
        TracedRuntimeBuilder::<HasTracePath, M>::build_and_start(*self, tokio_builder, writer)
    }
}

// ---------------------------------------------------------------------------
// Dial9ConfigBuilderError — unified error for builder validation and writer I/O
// ---------------------------------------------------------------------------

/// Errors produced while building a [`Dial9Config`].
#[derive(Debug)]
#[non_exhaustive]
pub enum Dial9ConfigBuilderError {
    /// Telemetry is enabled (the default) but one or more required writer
    /// fields were never set on the builder.
    Validation(ValidationError),
    /// Failure constructing the [`RotatingWriter`] backing telemetry — for
    /// example, an unwritable `base_path`.
    Io(std::io::Error),
}

/// Opaque payload for [`Dial9ConfigBuilderError::Validation`].
#[derive(Debug)]
pub struct ValidationError {
    fields: Vec<&'static str>,
}

impl ValidationError {
    /// The names of the required builder setters that were not called.
    pub fn fields(&self) -> &[&'static str] {
        &self.fields
    }
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "missing required Dial9Config fields: {}",
            self.fields.join(", ")
        )
    }
}

impl std::fmt::Display for Dial9ConfigBuilderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Dial9ConfigBuilderError::Validation(v) => write!(f, "{v}"),
            Dial9ConfigBuilderError::Io(e) => write!(f, "rotating writer: {e}"),
        }
    }
}

impl std::error::Error for Dial9ConfigBuilderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Dial9ConfigBuilderError::Validation(_) => None,
            Dial9ConfigBuilderError::Io(e) => Some(e),
        }
    }
}

// ---------------------------------------------------------------------------
// Dial9Config — opaque value the macro consumes
// ---------------------------------------------------------------------------

/// Finalized configuration consumed by the `#[main]` macro.
///
/// Constructed via [`Dial9Config::builder()`] followed by either:
///
/// - [`Dial9ConfigBuilder::build`] — strict; returns
///   `Result<Dial9Config, Dial9ConfigBuilderError>`. The
///   [`RotatingWriter`] is probed eagerly inside `build`, so any I/O
///   failure surfaces here rather than later when the runtime is built.
/// - [`Dial9ConfigBuilder::build_or_disabled`] — lenient; never reports
///   a build error, downgrades to a disabled config that preserves the
///   user's `with_tokio` configurators on validation or I/O failure.
#[derive(Debug)]
pub struct Dial9Config(pub(crate) Inner);

/// A configurator closure that customizes a [`tokio::runtime::Builder`].
///
/// Stored as `Arc<dyn Fn ...>` so that the configurator vector is cheaply
/// cloneable — the `build_or_disabled` path needs to preserve the
/// configurators on the disabled-fallback variant when validation or
/// writer-I/O setup fails.
pub(crate) type TokioConfigurator =
    Arc<dyn Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static>;

#[allow(clippy::large_enum_variant)]
pub(crate) enum Inner {
    Enabled {
        writer: RotatingWriter,
        tokio_configurators: Vec<TokioConfigurator>,
        runtime_builder: Box<dyn BuildTracedRuntime>,
    },
    Disabled {
        tokio_configurators: Vec<TokioConfigurator>,
    },
}

impl fmt::Debug for Inner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Inner::Enabled {
                writer,
                tokio_configurators,
                runtime_builder: _,
            } => f
                .debug_struct("Enabled")
                .field("writer", writer)
                .field("tokio_configurators", &tokio_configurators.len())
                .finish(),
            Inner::Disabled {
                tokio_configurators,
            } => f
                .debug_struct("Disabled")
                .field("tokio_configurators", &tokio_configurators.len())
                .finish(),
        }
    }
}

pub(crate) fn materialize_tokio_builder(
    configurators: &[TokioConfigurator],
) -> tokio::runtime::Builder {
    let mut b = default_tokio_builder();
    for c in configurators {
        c(&mut b);
    }
    b
}

// ---------------------------------------------------------------------------
// Dial9ConfigBuilder — single bon-generated fluent entry point
// ---------------------------------------------------------------------------

type RuntimeConfigurator = Box<
    dyn FnOnce(TracedRuntimeBuilder<HasTracePath, PipelineUnset>) -> Box<dyn BuildTracedRuntime>,
>;

const ENV_DIAL9_ENABLED: &str = "DIAL9_ENABLED";
const ENV_DIAL9_TRACE_DIR: &str = "DIAL9_TRACE_DIR";
const ENV_DIAL9_ROTATION_SECS: &str = "DIAL9_ROTATION_SECS";
const ENV_DIAL9_MAX_DISK_USAGE_MB: &str = "DIAL9_MAX_DISK_USAGE_MB";
const ENV_DIAL9_MAX_FILE_SIZE_MB: &str = "DIAL9_MAX_FILE_SIZE_MB";
const ENV_DIAL9_TASK_TRACKING_ENABLED: &str = "DIAL9_TASK_TRACKING_ENABLED";
const ENV_DIAL9_RUNTIME_NAME: &str = "DIAL9_RUNTIME_NAME";
const ENV_DIAL9_S3_BUCKET: &str = "DIAL9_S3_BUCKET";
const ENV_DIAL9_SERVICE_NAME: &str = "DIAL9_SERVICE_NAME";
const ENV_DIAL9_S3_PREFIX: &str = "DIAL9_S3_PREFIX";
const ENV_DIAL9_CPU_PROFILE_ENABLED: &str = "DIAL9_CPU_PROFILE_ENABLED";
const ENV_DIAL9_CPU_SAMPLE_HZ: &str = "DIAL9_CPU_SAMPLE_HZ";
const ENV_DIAL9_SCHEDULE_PROFILE_ENABLED: &str = "DIAL9_SCHEDULE_PROFILE_ENABLED";
const ENV_DIAL9_TASK_DUMP_ENABLED: &str = "DIAL9_TASK_DUMP_ENABLED";
const ENV_DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS: &str = "DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS";

const DEFAULT_ENABLED: bool = false;
const DEFAULT_TRACE_DIR: &str = "/tmp/dial9-traces";
const DEFAULT_MAX_DISK_USAGE_MB: u64 = 1024;
const DEFAULT_TASK_TRACKING_ENABLED: bool = true;
const DEFAULT_CPU_PROFILE_ENABLED: bool = cfg!(all(target_os = "linux", feature = "cpu-profiling"));
const DEFAULT_SCHEDULE_PROFILE_ENABLED: bool =
    cfg!(all(target_os = "linux", feature = "cpu-profiling"));
const DEFAULT_TASK_DUMP_ENABLED: bool = false;

const BYTES_PER_MIB: u64 = 1024 * 1024;
const MIN_MAX_FILE_SIZE: u64 = 16 * BYTES_PER_MIB;

trait EnvSource {
    fn get(&self, name: &str) -> Result<String, std::env::VarError>;
}

struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, name: &str) -> Result<String, std::env::VarError> {
        std::env::var(name)
    }
}

impl<S: EnvSource + ?Sized> EnvSource for &S {
    fn get(&self, name: &str) -> Result<String, std::env::VarError> {
        (*self).get(name)
    }
}

#[derive(Debug)]
struct ParsedEnvConfig {
    enabled: Option<bool>,
    trace_dir: Option<PathBuf>,
    rotation_period: Option<Duration>,
    max_total_size: Option<u64>,
    max_file_size: Option<u64>,
    task_tracking_enabled: Option<bool>,
    runtime_name: Option<String>,
    s3: Option<ParsedS3Config>,
    cpu_profile_enabled: Option<bool>,
    cpu_sample_hz: Option<u64>,
    schedule_profile_enabled: Option<bool>,
    task_dump_enabled: Option<bool>,
    task_dump_idle_threshold: Option<Duration>,
}

#[derive(Debug)]
#[cfg_attr(not(feature = "worker-s3"), allow(dead_code))]
struct ParsedS3Config {
    bucket: String,
    service_name: Option<String>,
    prefix: Option<String>,
}

#[derive(Debug)]
struct ResolvedEnvConfig {
    enabled: bool,
    trace_dir: PathBuf,

    // None means the underlying RotatingWriter builder owns the default.
    rotation_period: Option<Duration>,

    max_total_size: u64,
    max_file_size: u64,
    task_tracking_enabled: bool,

    // Optional config: None means do not set a runtime name.
    runtime_name: Option<String>,

    // Optional integration: None means do not configure S3 upload.
    s3: Option<ParsedS3Config>,

    cpu_profile_enabled: bool,

    // None means CpuProfilingConfig::default() owns the sample rate.
    cpu_sample_hz: Option<u64>,

    schedule_profile_enabled: bool,
    task_dump_enabled: bool,

    // None means TaskDumpConfig::default() owns the idle threshold.
    task_dump_idle_threshold: Option<Duration>,
}

struct RuntimeEnvConfig {
    task_tracking_enabled: bool,
    runtime_name: Option<String>,
    cpu_profile_enabled: bool,
    #[cfg_attr(not(feature = "cpu-profiling"), allow(dead_code))]
    cpu_sample_hz: Option<u64>,
    schedule_profile_enabled: bool,
    task_dump_enabled: bool,
    task_dump_idle_threshold: Option<Duration>,
}

fn parse_env_config(env: &impl EnvSource) -> ParsedEnvConfig {
    let env = EnvSourceParser::new(env);

    let max_total_size = env
        .get_positive_u64(ENV_DIAL9_MAX_DISK_USAGE_MB)
        .map(|mb| mb.saturating_mul(BYTES_PER_MIB));
    let max_file_size = env
        .get_positive_u64(ENV_DIAL9_MAX_FILE_SIZE_MB)
        .map(|mb| mb.saturating_mul(BYTES_PER_MIB));
    let s3 = env
        .get_string(ENV_DIAL9_S3_BUCKET)
        .map(|bucket| ParsedS3Config {
            bucket,
            service_name: env.get_string(ENV_DIAL9_SERVICE_NAME),
            prefix: env.get_string(ENV_DIAL9_S3_PREFIX),
        });

    ParsedEnvConfig {
        enabled: env.get_bool(ENV_DIAL9_ENABLED),
        trace_dir: env.get_string(ENV_DIAL9_TRACE_DIR).map(PathBuf::from),
        rotation_period: env
            .get_positive_u64(ENV_DIAL9_ROTATION_SECS)
            .map(Duration::from_secs),
        max_total_size,
        max_file_size,
        task_tracking_enabled: env.get_bool(ENV_DIAL9_TASK_TRACKING_ENABLED),
        runtime_name: env.get_string(ENV_DIAL9_RUNTIME_NAME),
        s3,
        cpu_profile_enabled: env.get_bool(ENV_DIAL9_CPU_PROFILE_ENABLED),
        cpu_sample_hz: env.get_positive_u64(ENV_DIAL9_CPU_SAMPLE_HZ),
        schedule_profile_enabled: env.get_bool(ENV_DIAL9_SCHEDULE_PROFILE_ENABLED),
        task_dump_enabled: env.get_bool(ENV_DIAL9_TASK_DUMP_ENABLED),
        task_dump_idle_threshold: env
            .get_positive_u64(ENV_DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS)
            .map(Duration::from_millis),
    }
}

fn resolve_env_config(parsed: ParsedEnvConfig) -> ResolvedEnvConfig {
    let max_total_size = parsed
        .max_total_size
        .unwrap_or_else(|| DEFAULT_MAX_DISK_USAGE_MB.saturating_mul(BYTES_PER_MIB));
    let max_file_size = parsed
        .max_file_size
        .unwrap_or_else(|| derive_max_file_size(max_total_size));

    ResolvedEnvConfig {
        enabled: parsed.enabled.unwrap_or(DEFAULT_ENABLED),
        trace_dir: parsed
            .trace_dir
            .unwrap_or_else(|| PathBuf::from(DEFAULT_TRACE_DIR)),
        rotation_period: parsed.rotation_period,
        max_total_size,
        max_file_size,
        task_tracking_enabled: parsed
            .task_tracking_enabled
            .unwrap_or(DEFAULT_TASK_TRACKING_ENABLED),
        runtime_name: parsed.runtime_name,
        s3: parsed.s3,
        cpu_profile_enabled: parsed
            .cpu_profile_enabled
            .unwrap_or(DEFAULT_CPU_PROFILE_ENABLED),
        cpu_sample_hz: parsed.cpu_sample_hz,
        schedule_profile_enabled: parsed
            .schedule_profile_enabled
            .unwrap_or(DEFAULT_SCHEDULE_PROFILE_ENABLED),
        task_dump_enabled: parsed
            .task_dump_enabled
            .unwrap_or(DEFAULT_TASK_DUMP_ENABLED),
        task_dump_idle_threshold: parsed.task_dump_idle_threshold,
    }
}

struct EnvSourceParser<S>(S);

impl<S> EnvSourceParser<S> {
    fn new(source: S) -> Self {
        Self(source)
    }
}

impl<S: EnvSource> EnvSourceParser<S> {
    fn get_bool(&self, name: &'static str) -> Option<bool> {
        let value = match self.0.get(name) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return None,
            Err(std::env::VarError::NotUnicode(_)) => {
                warn_not_unicode(name);
                return None;
            }
        };
        let value = value.trim();
        if value.is_empty() {
            warn(format_args!(
                "dial9: {name} is blank; expected an explicit boolean value; ignoring"
            ));
            return None;
        }

        match value.to_ascii_lowercase().as_str() {
            "t" | "true" | "1" | "y" | "yes" | "on" => Some(true),
            "f" | "false" | "0" | "n" | "no" | "off" => Some(false),
            _ => {
                warn(format_args!(
                    "dial9: {name}={value:?} is invalid; valid values are t,true,1,y,yes,on,f,false,0,n,no,off; ignoring"
                ));
                None
            }
        }
    }

    fn get_positive_u64(&self, name: &'static str) -> Option<u64> {
        let value = match self.0.get(name) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return None,
            Err(std::env::VarError::NotUnicode(_)) => {
                warn_not_unicode(name);
                return None;
            }
        };
        let value = value.trim();
        if value.is_empty() {
            warn(format_args!(
                "dial9: {name} is blank; expected a positive integer; ignoring"
            ));
            return None;
        }

        match value.parse::<u64>() {
            Ok(n) if n > 0 => Some(n),
            _ => {
                warn(format_args!(
                    "dial9: {name}={value:?} is invalid; expected a positive integer; ignoring"
                ));
                None
            }
        }
    }

    fn get_string(&self, name: &'static str) -> Option<String> {
        let value = match self.0.get(name) {
            Ok(value) => value,
            Err(std::env::VarError::NotPresent) => return None,
            Err(std::env::VarError::NotUnicode(_)) => {
                warn_not_unicode(name);
                return None;
            }
        };
        let value = value.trim();
        if value.is_empty() {
            warn(format_args!(
                "dial9: {name} is blank; expected a non-empty value; ignoring"
            ));
            return None;
        }
        Some(value.to_string())
    }
}

fn derive_max_file_size(max_total_size: u64) -> u64 {
    // Keep size-based rotation as a safety valve: roughly four large segments
    // fit in the disk budget, but avoid tiny files on very small budgets.
    (max_total_size / 4).max(MIN_MAX_FILE_SIZE)
}

#[cfg(feature = "worker-s3")]
fn default_service_name() -> String {
    if let Ok(path) = std::env::current_exe()
        && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        && !stem.trim().is_empty()
    {
        return stem.to_string();
    }

    "unknown-service".to_string()
}

fn warn(message: fmt::Arguments<'_>) {
    if tracing::dispatcher::has_been_set() {
        tracing::warn!(target: "dial9_telemetry", "{message}");
    } else {
        eprintln!("{message}");
    }
}

fn warn_not_unicode(name: &'static str) {
    warn(format_args!("dial9: {name} is not valid Unicode; ignoring"));
}

fn apply_runtime_env<M>(
    mut runtime: TracedRuntimeBuilder<HasTracePath, M>,
    config: RuntimeEnvConfig,
) -> TracedRuntimeBuilder<HasTracePath, M> {
    if let Some(name) = config.runtime_name {
        runtime = runtime.with_runtime_name(name);
    }
    runtime = runtime.with_task_tracking(config.task_tracking_enabled);

    if config.task_dump_enabled {
        let task_dump_config = match config.task_dump_idle_threshold {
            Some(threshold) => crate::telemetry::TaskDumpConfig::builder()
                .idle_threshold(threshold)
                .build(),
            None => crate::telemetry::TaskDumpConfig::default(),
        };
        runtime = runtime.with_task_dumps(task_dump_config);
    }

    #[cfg(feature = "cpu-profiling")]
    {
        use crate::telemetry::cpu_profile::{CpuProfilingConfig, SchedEventConfig};

        if config.cpu_profile_enabled {
            let cpu_config = match config.cpu_sample_hz {
                Some(hz) => CpuProfilingConfig::default().frequency_hz(hz),
                None => CpuProfilingConfig::default(),
            };
            runtime = runtime.with_cpu_profiling(cpu_config);
        }
        if config.schedule_profile_enabled {
            runtime = runtime.with_sched_events(SchedEventConfig::default());
        }
    }

    #[cfg(not(feature = "cpu-profiling"))]
    if config.cpu_profile_enabled || config.schedule_profile_enabled {
        warn(format_args!(
            "dial9: CPU/schedule profiling requested but `cpu-profiling` feature is not enabled; ignoring"
        ));
    }

    runtime
}

#[cfg(feature = "worker-s3")]
fn build_s3_config(config: ParsedS3Config) -> crate::background_task::s3::S3Config {
    crate::background_task::s3::S3Config::builder()
        .bucket(config.bucket)
        .service_name(config.service_name.unwrap_or_else(default_service_name))
        .maybe_prefix(config.prefix)
        .build()
}

fn default_tokio_builder() -> tokio::runtime::Builder {
    let mut b = tokio::runtime::Builder::new_multi_thread();
    b.enable_all();
    b
}

impl Dial9Config {
    /// Build a production-oriented config from standard `DIAL9_*` environment variables.
    ///
    /// Supported local trace writer variables:
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_ENABLED` | `false` | Master switch for installing telemetry. |
    /// | `DIAL9_TRACE_DIR` | `/tmp/dial9-traces` | Directory for rotated trace segments. |
    /// | `DIAL9_ROTATION_SECS` | `60` | Wall-clock rotation period in seconds. |
    /// | `DIAL9_MAX_DISK_USAGE_MB` | `1024` | Total on-disk trace budget in MiB. |
    /// | `DIAL9_MAX_FILE_SIZE_MB` | `max(total / 4, 16)` | Per-file trace segment size in MiB. |
    ///
    /// Supported runtime variables:
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_TASK_TRACKING_ENABLED` | `true` | Track tasks spawned through dial9 handles. |
    /// | `DIAL9_RUNTIME_NAME` | unset | Human-readable runtime name in trace metadata. |
    ///
    /// Supported S3 variables (`worker-s3` feature required):
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_S3_BUCKET` | unset | Upload sealed trace segments to this bucket. |
    /// | `DIAL9_SERVICE_NAME` | binary name | Service name used in S3 keys and metadata. |
    /// | `DIAL9_S3_PREFIX` | unset | Optional S3 object key prefix. |
    ///
    /// Supported CPU profiling variables (`cpu-profiling` feature required):
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_CPU_PROFILE_ENABLED` | `true` on Linux with `cpu-profiling`, `false` otherwise | Enable CPU stack sampling. |
    /// | `DIAL9_CPU_SAMPLE_HZ` | `99` | CPU sampling frequency in Hz. |
    /// | `DIAL9_SCHEDULE_PROFILE_ENABLED` | `true` on Linux with `cpu-profiling`, `false` otherwise | Enable per-worker scheduler event capture. |
    ///
    /// Supported task dump variables (capture requires the `taskdump` feature):
    ///
    /// | Variable | Default | Meaning |
    /// | --- | --- | --- |
    /// | `DIAL9_TASK_DUMP_ENABLED` | `false` | Capture async task dumps at idle yield points. |
    /// | `DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS` | `10` | Mean idle duration for task dump sampling. |
    ///
    /// Missing variables use defaults. Blank, invalid, or non-Unicode values
    /// emit a warning and are treated as missing. Some numeric defaults come
    /// from the underlying config builders and are listed here as the current
    /// `from_env()` behavior. The returned config is built with
    /// [`Dial9ConfigBuilder::build_or_disabled`], so writer setup failures are
    /// logged and downgraded to a plain Tokio runtime.
    pub fn from_env() -> Self {
        Self::from_env_source(&ProcessEnv)
    }

    fn from_env_source(env: &impl EnvSource) -> Self {
        let ResolvedEnvConfig {
            enabled,
            trace_dir,
            rotation_period,
            max_total_size,
            max_file_size,
            task_tracking_enabled,
            runtime_name,
            s3,
            cpu_profile_enabled,
            cpu_sample_hz,
            schedule_profile_enabled,
            task_dump_enabled,
            task_dump_idle_threshold,
        } = resolve_env_config(parse_env_config(env));

        let runtime_config = RuntimeEnvConfig {
            task_tracking_enabled,
            runtime_name,
            cpu_profile_enabled,
            cpu_sample_hz,
            schedule_profile_enabled,
            task_dump_enabled,
            task_dump_idle_threshold,
        };

        let builder = Self::builder()
            .enabled(enabled)
            .base_path(trace_dir.join("trace.bin"))
            .max_file_size(max_file_size)
            .max_total_size(max_total_size)
            .maybe_rotation_period(rotation_period);

        #[cfg(feature = "worker-s3")]
        let builder = match s3 {
            Some(s3) => builder.with_runtime(move |runtime| {
                apply_runtime_env(
                    runtime.with_s3_uploader(build_s3_config(s3)),
                    runtime_config,
                )
            }),
            None => builder.with_runtime(move |runtime| apply_runtime_env(runtime, runtime_config)),
        };

        #[cfg(not(feature = "worker-s3"))]
        let builder = {
            if s3.is_some() {
                warn(format_args!(
                    "dial9: S3 upload requested but `worker-s3` feature is not enabled; ignoring"
                ));
            }
            builder.with_runtime(move |runtime| apply_runtime_env(runtime, runtime_config))
        };

        builder.build_or_disabled()
    }
}

#[bon::bon]
impl Dial9Config {
    /// Start a fluent configuration chain.
    ///
    /// When telemetry is enabled (the default), the following setters are
    /// required: [`base_path`](Dial9ConfigBuilder::base_path),
    /// [`max_file_size`](Dial9ConfigBuilder::max_file_size),
    /// [`max_total_size`](Dial9ConfigBuilder::max_total_size). They may be
    /// omitted if `.enabled(false)` is set, in which case a plain tokio
    /// runtime is built without telemetry.
    #[builder(
        builder_type = Dial9ConfigBuilder,
        finish_fn = build,
        state_mod = dial9_config_builder,
    )]
    pub fn builder(
        #[builder(field)] tokio_configurators: Vec<TokioConfigurator>,

        #[builder(field)] runtime_finalizer: Option<RuntimeConfigurator>,

        /// Defaults to `true`. When `false`, required writer fields are
        /// ignored and the runtime is built without telemetry.
        #[builder(default = true)]
        enabled: bool,
        /// Trace output path.
        #[builder(into)]
        base_path: Option<PathBuf>,
        /// Per-file rotation threshold in bytes.
        max_file_size: Option<u64>,
        /// Total disk budget in bytes.
        max_total_size: Option<u64>,
        /// Wall-clock rotation period for the writer.
        rotation_period: Option<Duration>,
    ) -> Result<Dial9Config, Dial9ConfigBuilderError> {
        assemble(AssembleArgs {
            tokio_configurators,
            runtime_finalizer,
            enabled,
            base_path,
            max_file_size,
            max_total_size,
            rotation_period,
        })
        .map(Dial9Config)
    }
}

struct AssembleArgs {
    tokio_configurators: Vec<TokioConfigurator>,
    runtime_finalizer: Option<RuntimeConfigurator>,
    enabled: bool,
    base_path: Option<PathBuf>,
    max_file_size: Option<u64>,
    max_total_size: Option<u64>,
    rotation_period: Option<Duration>,
}

/// Shared finish-fn body: builder-staged fields > [`Inner`].
///
/// Used by both `build()` (which propagates the error) and
/// `build_or_disabled()` (which substitutes [`Inner::Disabled`] on error).
fn assemble(args: AssembleArgs) -> Result<Inner, Dial9ConfigBuilderError> {
    let AssembleArgs {
        tokio_configurators,
        runtime_finalizer,
        enabled,
        base_path,
        max_file_size,
        max_total_size,
        rotation_period,
    } = args;

    if !enabled {
        return Ok(Inner::Disabled {
            tokio_configurators,
        });
    }

    let required_fields = (base_path, max_file_size, max_total_size);
    let (base_path, max_file_size, max_total_size) = match required_fields {
        (Some(bp), Some(mfs), Some(mts)) => (bp, mfs, mts),
        (bp, mfs, mts) => {
            let missing = [
                ("base_path", bp.is_none()),
                ("max_file_size", mfs.is_none()),
                ("max_total_size", mts.is_none()),
            ]
            .into_iter()
            .filter_map(|(name, missing)| missing.then_some(name))
            .collect();
            return Err(Dial9ConfigBuilderError::Validation(ValidationError {
                fields: missing,
            }));
        }
    };

    let writer = RotatingWriter::builder()
        .base_path(base_path.clone())
        .max_file_size(max_file_size)
        .max_total_size(max_total_size)
        .maybe_rotation_period(rotation_period)
        .build()
        .map_err(Dial9ConfigBuilderError::Io)?;

    let runtime_builder: TracedRuntimeBuilder<HasTracePath, PipelineUnset> =
        TracedRuntime::builder().with_trace_path(base_path);
    let runtime_builder: Box<dyn BuildTracedRuntime> = match runtime_finalizer {
        Some(configure) => configure(runtime_builder),
        None => Box::new(runtime_builder),
    };

    Ok(Inner::Enabled {
        writer,
        tokio_configurators,
        runtime_builder,
    })
}

impl<S: dial9_config_builder::State> Dial9ConfigBuilder<S> {
    /// Queue a configurator for the underlying [`tokio::runtime::Builder`].
    ///
    /// The closure receives a fresh builder by mutable reference — use any
    /// tokio knob (`worker_threads`, `thread_name`, `thread_stack_size`,
    /// `global_queue_interval`, etc.). The builder is pre-seeded with
    /// `new_multi_thread()` and `enable_all()`. To switch flavors, replace
    /// the whole builder inside the closure:
    /// `*t = tokio::runtime::Builder::new_current_thread(); t.enable_all();`.
    ///
    /// Can be called multiple times; configurators are applied in call
    /// order each time the runtime is materialized. The closure must be
    /// `Fn + Send + Sync + 'static` so that
    /// [`build_or_disabled`](Self::build_or_disabled) can preserve the
    /// configurators on the disabled-fallback variant.
    pub fn with_tokio<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut tokio::runtime::Builder) + Send + Sync + 'static,
    {
        self.tokio_configurators.push(Arc::new(f));
        self
    }

    /// Set the configurator for the dial9 [`TracedRuntimeBuilder`].
    ///
    /// The closure receives the staged builder by value and must return it.
    /// Use this to access runtime configuration methods like
    /// `with_runtime_name`, `with_task_tracking`, `with_s3_uploader`, or
    /// `with_custom_pipeline`; see [`TracedRuntimeBuilder`] for the full list.
    ///
    /// The closure may transition the builder's pipeline-mode marker
    /// (e.g. by calling `.with_s3_uploader(...)` or
    /// `.with_custom_pipeline(...)`); the resulting mode is preserved
    /// through to runtime construction.
    ///
    /// The configurator is applied during `build()` once `base_path` is
    /// known. When `.enabled(false)` is set the configurator is ignored.
    /// Calling this method more than once replaces the prior closure.
    pub fn with_runtime<F, N>(mut self, f: F) -> Self
    where
        F: FnOnce(
                TracedRuntimeBuilder<HasTracePath, PipelineUnset>,
            ) -> TracedRuntimeBuilder<HasTracePath, N>
            + 'static,
        N: Send + 'static,
    {
        self.runtime_finalizer = Some(Box::new(move |b| Box::new(f(b))));
        self
    }
}

impl<S: dial9_config_builder::IsComplete> Dial9ConfigBuilder<S> {
    /// Finish into a [`Dial9Config`] that never reports a build error.
    ///
    /// On any [`Dial9ConfigBuilderError`] (validation failure or writer
    /// I/O probe failure) logs an error and returns a [`Dial9Config`]
    /// in its disabled state with the user's `with_tokio` configurators
    /// preserved. The resulting config builds a plain tokio runtime
    /// when handed to [`crate::TracedRuntime::try_new`].
    ///
    /// # Panics (debug builds only)
    ///
    /// Panics on missing required fields to surface misconfigurations
    /// during development.
    ///
    /// Lenient counterpart to [`build`](Self::build). Use
    /// [`build`](Self::build) instead when you want validation and
    /// writer-I/O failures to surface as
    /// [`Dial9ConfigBuilderError`].
    pub fn build_or_disabled(self) -> Dial9Config {
        let cfgs_for_fallback = self.tokio_configurators.clone();
        match self.build() {
            Ok(cfg) => cfg,
            Err(e) => {
                let is_validation = matches!(e, Dial9ConfigBuilderError::Validation(_));

                debug_assert!(!is_validation, "dial9 config validation failed: {e}");

                let msg = format!(
                    "dial9: telemetry config build failed; falling back to plain tokio runtime: {e}"
                );
                if tracing::dispatcher::has_been_set() {
                    tracing::error!(target: "dial9_telemetry", "{msg}");
                } else {
                    eprintln!("{msg}");
                }

                Dial9Config(Inner::Disabled {
                    tokio_configurators: cfgs_for_fallback,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::TracedRuntime;

    use super::*;

    fn tmp_base_path() -> PathBuf {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak the TempDir so it isn't deleted while the test runs.
        let path = dir.path().join("trace.bin");
        std::mem::forget(dir);
        path
    }

    /// A path under a directory that does not exist; RotatingWriter::build()
    /// will fail to create the trace file there.
    fn unwritable_base_path() -> PathBuf {
        PathBuf::from("/this/dir/does/not/exist/dial9_test_trace.bin")
    }

    #[derive(Default)]
    struct FakeEnv {
        vars: BTreeMap<String, FakeEnvValue>,
    }

    enum FakeEnvValue {
        Unicode(String),
        NonUnicode,
    }

    impl FakeEnv {
        fn with(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
            self.vars
                .insert(name.into(), FakeEnvValue::Unicode(value.into()));
            self
        }

        fn with_non_unicode(mut self, name: impl Into<String>) -> Self {
            self.vars.insert(name.into(), FakeEnvValue::NonUnicode);
            self
        }
    }

    impl EnvSource for FakeEnv {
        fn get(&self, name: &str) -> Result<String, std::env::VarError> {
            match self.vars.get(name) {
                Some(FakeEnvValue::Unicode(value)) => Ok(value.clone()),
                Some(FakeEnvValue::NonUnicode) => Err(std::env::VarError::NotUnicode(
                    OsString::from("not unicode"),
                )),
                None => Err(std::env::VarError::NotPresent),
            }
        }
    }

    #[test]
    fn env_missing_values_are_unset() {
        let parsed = parse_env_config(&FakeEnv::default());

        assert_eq!(parsed.enabled, None);
        assert_eq!(parsed.trace_dir, None);
        assert_eq!(parsed.rotation_period, None);
        assert_eq!(parsed.max_total_size, None);
        assert_eq!(parsed.max_file_size, None);
        assert_eq!(parsed.task_tracking_enabled, None);
        assert_eq!(parsed.runtime_name, None);
        assert!(parsed.s3.is_none());
        assert_eq!(parsed.cpu_profile_enabled, None);
        assert_eq!(parsed.cpu_sample_hz, None);
        assert_eq!(parsed.schedule_profile_enabled, None);
        assert_eq!(parsed.task_dump_enabled, None);
        assert_eq!(parsed.task_dump_idle_threshold, None);
    }

    #[test]
    fn env_resolution_applies_only_from_env_owned_defaults() {
        let resolved = resolve_env_config(parse_env_config(&FakeEnv::default()));
        let supported_profiling = cfg!(all(target_os = "linux", feature = "cpu-profiling"));

        assert_eq!(resolved.enabled, DEFAULT_ENABLED);
        assert_eq!(resolved.trace_dir, PathBuf::from(DEFAULT_TRACE_DIR));
        assert_eq!(
            resolved.max_total_size,
            DEFAULT_MAX_DISK_USAGE_MB * BYTES_PER_MIB
        );
        assert_eq!(
            resolved.max_file_size,
            derive_max_file_size(resolved.max_total_size)
        );
        assert_eq!(
            resolved.task_tracking_enabled,
            DEFAULT_TASK_TRACKING_ENABLED
        );
        assert_eq!(resolved.cpu_profile_enabled, supported_profiling);
        assert_eq!(resolved.schedule_profile_enabled, supported_profiling);
        assert_eq!(resolved.task_dump_enabled, DEFAULT_TASK_DUMP_ENABLED);

        // Optional config/integrations remain absent unless explicitly requested.
        assert_eq!(resolved.runtime_name, None);
        assert!(resolved.s3.is_none());

        // Delegated defaults remain unset so their underlying config types own them.
        assert_eq!(resolved.rotation_period, None);
        assert_eq!(resolved.cpu_sample_hz, None);
        assert_eq!(resolved.task_dump_idle_threshold, None);
    }

    #[test]
    fn env_parses_trimmed_values() {
        let parsed = parse_env_config(
            &FakeEnv::default()
                .with("DIAL9_ENABLED", " YES ")
                .with("DIAL9_TRACE_DIR", " /var/tmp/dial9 ")
                .with("DIAL9_ROTATION_SECS", "15")
                .with("DIAL9_MAX_DISK_USAGE_MB", "2048"),
        );

        assert_eq!(parsed.enabled, Some(true));
        assert_eq!(parsed.trace_dir, Some(PathBuf::from("/var/tmp/dial9")));
        assert_eq!(parsed.rotation_period, Some(Duration::from_secs(15)));
        assert_eq!(parsed.max_total_size, Some(2048 * 1024 * 1024));
        assert_eq!(parsed.max_file_size, None);
    }

    #[test]
    fn env_parses_runtime_storage_s3_cpu_and_taskdump_values() {
        let parsed = parse_env_config(
            &FakeEnv::default()
                .with("DIAL9_TASK_TRACKING_ENABLED", "off")
                .with("DIAL9_RUNTIME_NAME", " api-runtime ")
                .with("DIAL9_MAX_FILE_SIZE_MB", "128")
                .with("DIAL9_S3_BUCKET", " traces-bucket ")
                .with("DIAL9_SERVICE_NAME", " checkout ")
                .with("DIAL9_S3_PREFIX", " prod/traces ")
                .with("DIAL9_CPU_PROFILE_ENABLED", "false")
                .with("DIAL9_CPU_SAMPLE_HZ", "199")
                .with("DIAL9_SCHEDULE_PROFILE_ENABLED", "false")
                .with("DIAL9_TASK_DUMP_ENABLED", "true")
                .with("DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS", "25"),
        );

        assert_eq!(parsed.task_tracking_enabled, Some(false));
        assert_eq!(parsed.runtime_name.as_deref(), Some("api-runtime"));
        assert_eq!(parsed.max_file_size, Some(128 * 1024 * 1024));
        let s3 = parsed.s3.expect("s3 config should be parsed");
        assert_eq!(s3.bucket, "traces-bucket");
        assert_eq!(s3.service_name.as_deref(), Some("checkout"));
        assert_eq!(s3.prefix.as_deref(), Some("prod/traces"));
        assert_eq!(parsed.cpu_profile_enabled, Some(false));
        assert_eq!(parsed.cpu_sample_hz, Some(199));
        assert_eq!(parsed.schedule_profile_enabled, Some(false));
        assert_eq!(parsed.task_dump_enabled, Some(true));
        assert_eq!(
            parsed.task_dump_idle_threshold,
            Some(Duration::from_millis(25))
        );
    }

    #[test]
    fn env_allows_s3_bucket_without_service_name() {
        let parsed = parse_env_config(&FakeEnv::default().with("DIAL9_S3_BUCKET", "b"));

        let s3 = parsed.s3.expect("s3 config should be parsed");
        assert_eq!(s3.bucket, "b");
        assert_eq!(s3.service_name, None);
        assert_eq!(s3.prefix, None);
    }

    #[cfg(feature = "worker-s3")]
    #[test]
    fn env_s3_config_defaults_service_name_when_bucket_is_set() {
        let config = build_s3_config(ParsedS3Config {
            bucket: "b".to_string(),
            service_name: None,
            prefix: None,
        });

        let metadata: BTreeMap<_, _> = config.as_metadata().collect();
        assert_eq!(metadata.get("bucket"), Some(&"b"));
        assert!(
            metadata
                .get("service_name")
                .is_some_and(|service_name| !service_name.is_empty())
        );
    }

    #[test]
    fn env_ignores_blank_or_invalid_values() {
        let parsed = parse_env_config(
            &FakeEnv::default()
                .with("DIAL9_ENABLED", "maybe")
                .with("DIAL9_TRACE_DIR", "   ")
                .with("DIAL9_ROTATION_SECS", "0")
                .with("DIAL9_MAX_DISK_USAGE_MB", "wat")
                .with("DIAL9_MAX_FILE_SIZE_MB", "0")
                .with("DIAL9_RUNTIME_NAME", "   ")
                .with("DIAL9_S3_BUCKET", "   ")
                .with("DIAL9_CPU_SAMPLE_HZ", "0")
                .with("DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS", "wat"),
        );

        assert_eq!(parsed.enabled, None);
        assert_eq!(parsed.trace_dir, None);
        assert_eq!(parsed.rotation_period, None);
        assert_eq!(parsed.max_total_size, None);
        assert_eq!(parsed.max_file_size, None);
        assert_eq!(parsed.runtime_name, None);
        assert!(parsed.s3.is_none());
        assert_eq!(parsed.cpu_sample_hz, None);
        assert_eq!(parsed.task_dump_idle_threshold, None);
    }

    #[test]
    fn env_treats_non_unicode_values_as_invalid() {
        let parsed = parse_env_config(
            &FakeEnv::default()
                .with_non_unicode("DIAL9_TRACE_DIR")
                .with_non_unicode("DIAL9_ROTATION_SECS"),
        );

        assert_eq!(parsed.trace_dir, None);
        assert_eq!(parsed.rotation_period, None);
    }

    #[test]
    fn env_config_builds_disabled_by_default() {
        let cfg = Dial9Config::from_env_source(&FakeEnv::default());

        assert!(matches!(cfg.0, Inner::Disabled { .. }));
    }

    #[test]
    fn env_config_builds_enabled_with_local_trace_defaults() {
        let dir = tempfile::tempdir().expect("tempdir");
        let trace_dir = dir.path().to_str().expect("utf8 tempdir");
        let env = FakeEnv::default()
            .with("DIAL9_ENABLED", "true")
            .with("DIAL9_TRACE_DIR", trace_dir);

        let cfg = Dial9Config::from_env_source(&env);

        match cfg.0 {
            Inner::Enabled { writer, .. } => {
                assert_eq!(writer.base_path(), dir.path().join("trace.bin"));
            }
            Inner::Disabled { .. } => panic!("expected enabled config"),
        }
    }

    #[test]
    fn env_config_applies_runtime_name_and_task_dumps() {
        let dir = tempfile::tempdir().expect("tempdir");
        let trace_dir = dir.path().to_str().expect("utf8 tempdir");
        let env = FakeEnv::default()
            .with("DIAL9_ENABLED", "true")
            .with("DIAL9_TRACE_DIR", trace_dir)
            .with("DIAL9_RUNTIME_NAME", " api-runtime ")
            .with("DIAL9_TASK_DUMP_ENABLED", "true")
            .with("DIAL9_TASK_DUMP_IDLE_THRESHOLD_MS", "25");

        let cfg = Dial9Config::from_env_source(&env);
        let rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        let shared = rt.guard().shared().expect("telemetry should be enabled");
        let contexts = shared.contexts.lock().unwrap();

        assert_eq!(contexts.len(), 1);
        assert_eq!(contexts[0].runtime_name.as_deref(), Some("api-runtime"));
        assert!(shared.task_dumps_enabled.load(Ordering::Relaxed));
        assert_eq!(
            shared.task_dump_idle_threshold_ns.load(Ordering::Relaxed),
            25_000_000
        );
    }

    #[test]
    fn builder_accepts_required_fields() {
        let _ = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build()
            .expect("build should succeed");
    }

    #[test]
    fn missing_required_fields_errors_with_all_missing_names() {
        match Dial9Config::builder().build() {
            Err(Dial9ConfigBuilderError::Validation(v)) => {
                assert_eq!(v.fields(), ["base_path", "max_file_size", "max_total_size"]);
            }
            Ok(_) => panic!("expected Validation error, got Ok"),
            Err(other) => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn missing_some_required_fields_lists_only_missing() {
        match Dial9Config::builder().max_file_size(1024).build() {
            Err(Dial9ConfigBuilderError::Validation(v)) => {
                assert_eq!(v.fields(), ["base_path", "max_total_size"]);
            }
            Ok(_) => panic!("expected Validation error, got Ok"),
            Err(other) => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn explicitly_enabled_still_requires_fields() {
        match Dial9Config::builder().enabled(true).build() {
            Err(Dial9ConfigBuilderError::Validation(_)) => {}
            Ok(_) => panic!("expected Validation error, got Ok"),
            Err(other) => panic!("expected Validation error, got {other:?}"),
        }
    }

    #[test]
    fn strict_build_returns_io_error_for_unwritable_base_path() {
        let result = Dial9Config::builder()
            .base_path(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build();
        match result {
            Err(Dial9ConfigBuilderError::Io(_)) => {}
            Ok(_) => panic!("expected Io error, got Ok"),
            Err(other) => panic!("expected Io error, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // build_or_disabled
    // ---------------------------------------------------------------

    #[test]
    #[cfg_attr(debug_assertions, should_panic = "dial9 config validation failed")]
    fn build_or_disabled_from_incomplete_builder_yields_disabled_runtime() {
        let cfg = Dial9Config::builder().build_or_disabled();
        let rt = TracedRuntime::try_new(cfg).expect("fallback runtime should build");
        assert!(
            !rt.guard().is_enabled(),
            "fallback path must yield an inert guard"
        );
        let v = rt.block_on(async { 7u32 });
        assert_eq!(v, 7);
    }

    #[test]
    fn build_or_disabled_from_complete_builder_yields_enabled_runtime() {
        let cfg = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build_or_disabled();
        let rt = TracedRuntime::try_new(cfg).expect("enabled runtime should build");
        assert!(
            rt.guard().is_enabled(),
            "valid config must keep telemetry enabled"
        );
    }

    #[test]
    fn build_or_disabled_downgrades_on_writer_io_failure() {
        let cfg = Dial9Config::builder()
            .base_path(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .build_or_disabled();
        let rt =
            TracedRuntime::try_new(cfg).expect("writer I/O failure should downgrade to disabled");
        assert!(
            !rt.guard().is_enabled(),
            "downgrade path must yield an inert guard"
        );
        let v = rt.block_on(async { 42u32 });
        assert_eq!(v, 42);
    }

    #[test]
    fn build_or_disabled_preserves_with_tokio_configurators_on_io_failure() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .base_path(unwritable_base_path())
            .max_file_size(1024)
            .max_total_size(4096)
            .with_tokio(move |b| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                b.worker_threads(1);
            })
            .build_or_disabled();
        let rt = TracedRuntime::try_new(cfg).expect("downgrade should produce a runtime");
        assert!(!rt.guard().is_enabled());
        let calls = counter.load(Ordering::SeqCst);
        assert!(
            calls >= 1,
            "with_tokio configurator must run on the disabled fallback runtime build (was {calls})"
        );
    }

    // ---------------------------------------------------------------
    // Strict-path configurators
    // ---------------------------------------------------------------

    #[test]
    fn strict_build_runs_with_tokio_configurator_on_success() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .with_tokio(move |b| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                b.worker_threads(2);
            })
            .build()
            .expect("strict build should succeed");
        let _rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "with_tokio configurator must run exactly once on strict success path"
        );
    }

    #[test]
    fn strict_build_runs_with_runtime_configurator_on_success() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .with_runtime(move |r| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                r
            })
            .build()
            .expect("strict build should succeed");
        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "with_runtime configurator must run exactly once during build()"
        );
        let _rt = TracedRuntime::try_new(cfg).expect("runtime should build");
    }

    #[test]
    fn enabled_false_drops_with_runtime_configurators() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_closure = Arc::clone(&counter);
        let cfg = Dial9Config::builder()
            .enabled(false)
            .with_runtime(move |r| {
                counter_for_closure.fetch_add(1, Ordering::SeqCst);
                r
            })
            .build()
            .expect("disabled build should succeed without required fields");
        let rt = TracedRuntime::try_new(cfg).expect("disabled runtime should build");
        assert!(
            !rt.guard().is_enabled(),
            "disabled config must yield an inert guard"
        );
        assert_eq!(
            counter.load(Ordering::SeqCst),
            0,
            "with_runtime configurators must be ignored when enabled(false)"
        );
    }

    #[test]
    fn multiple_with_tokio_applied_in_declared_order() {
        let order: Arc<std::sync::Mutex<Vec<u32>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let order_first = Arc::clone(&order);
        let order_second = Arc::clone(&order);
        let cfg = Dial9Config::builder()
            .base_path(tmp_base_path())
            .max_file_size(1024 * 1024)
            .max_total_size(4 * 1024 * 1024)
            .with_tokio(move |_b| {
                order_first.lock().unwrap().push(1);
            })
            .with_tokio(move |_b| {
                order_second.lock().unwrap().push(2);
            })
            .build()
            .expect("strict build should succeed");
        let _rt = TracedRuntime::try_new(cfg).expect("runtime should build");
        let recorded = order.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![1, 2],
            "with_tokio configurators must run in declared order"
        );
    }

    #[test]
    fn dial9_config_builder_error_io_display_and_source_chain() {
        let inner = std::io::Error::other("boom");
        let err = Dial9ConfigBuilderError::Io(inner);
        let display = format!("{err}");
        assert!(
            display.contains("rotating writer:"),
            "Display should label the variant, got: {display}"
        );
        assert!(
            display.contains("boom"),
            "Display should include the inner io::Error message, got: {display}"
        );
        let source = std::error::Error::source(&err);
        assert!(source.is_some(), "source() must return the inner io::Error");
    }
}
