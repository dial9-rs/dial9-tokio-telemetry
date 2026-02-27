/// Returns true when running in CI (GitHub Actions sets CI=true).
#[allow(dead_code)]
pub fn is_ci() -> bool {
    std::env::var("CI").is_ok()
}

/// Constructs a sampler from the given expression, returning early if
/// `perf_event_open` is unavailable (e.g. CI without perf permissions).
/// Panics with a useful message in non-CI environments.
///
/// NOTE: only useful with constructors that actually call `perf_event_open`,
/// i.e. `PerfSampler::start`. `PerfSampler::new_per_thread` only builds an
/// attr struct and never fails with PermissionDenied — use `is_ci()` at the
/// top of those tests and `require_perf_ok!` for the `track_current_thread`
/// calls instead.
///
/// Usage:
///   let mut sampler = require_sampler!(PerfSampler::start(config));
#[macro_export]
macro_rules! require_sampler {
    ($e:expr) => {
        match $e {
            Ok(s) => s,
            Err(_) if std::env::var("CI").is_ok() => {
                eprintln!("Skipping test: perf_event_open unavailable in CI");
                return;
            }
            Err(e) => panic!("failed to start sampler: {}", e),
        }
    };
}

/// Unwraps a `Result<()>` from a perf operation, returning early if
/// `perf_event_open` is unavailable in CI. Panics with a useful message
/// in non-CI environments.
///
/// Usage:
///   require_perf_ok!(sampler.track_current_thread());
#[macro_export]
macro_rules! require_perf_ok {
    ($e:expr) => {
        match $e {
            Ok(()) => {}
            Err(_) if std::env::var("CI").is_ok() => {
                eprintln!("Skipping test: perf operation unavailable in CI");
                return;
            }
            Err(e) => panic!("perf operation failed: {}", e),
        }
    };
}
