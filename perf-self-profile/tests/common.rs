pub fn is_ci() -> bool {
    std::env::var("CI").is_ok()
}
