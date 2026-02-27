/// Returns true when running in CI (GitHub Actions sets CI=true).
pub fn is_ci() -> bool {
    std::env::var("CI").is_ok()
}
