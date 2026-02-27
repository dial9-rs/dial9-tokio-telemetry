/// Constructs a sampler from the given expression, returning early if
/// `perf_event_open` is unavailable (e.g. CI without perf permissions).
/// Panics with a useful message in non-CI environments.
///
/// Usage:
///   let mut sampler = require_sampler!(PerfSampler::start(config));
///   let sampler = require_sampler!(PerfSampler::new_per_thread(config));
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
