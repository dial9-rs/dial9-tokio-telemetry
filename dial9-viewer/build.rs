use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let toolkit_dir = Path::new(&manifest_dir).join("toolkit");
    let out_dir = env::var("OUT_DIR").unwrap();
    let dest = Path::new(&out_dir).join("toolkit_files.rs");

    println!("cargo::rerun-if-changed=toolkit");

    let mut entries: Vec<(String, String)> = Vec::new();
    if toolkit_dir.is_dir() {
        for entry in fs::read_dir(&toolkit_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_file() || path.is_symlink() {
                let name = entry.file_name().to_string_lossy().to_string();
                // Use the canonical path so include_str! follows symlinks
                let canonical = fs::canonicalize(&path)
                    .unwrap_or_else(|_| path.clone())
                    .to_string_lossy()
                    .to_string();
                entries.push((name, canonical));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut code = String::from("pub const TOOLKIT_FILES: &[(&str, &str)] = &[\n");
    for (name, path) in &entries {
        code.push_str(&format!("    ({:?}, include_str!({:?})),\n", name, path));
    }
    code.push_str("];\n");

    fs::write(&dest, code).unwrap();
}
