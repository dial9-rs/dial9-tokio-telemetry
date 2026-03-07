#[test]
fn expand_derive_macro() {
    macrotest::expand("tests/expand/*.rs");
}
