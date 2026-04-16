use std::env;
use std::fs;
use std::path::Path;

/// Generates two files in OUT_DIR:
///
/// `toolkit_files.rs` — `TOOLKIT_FILES: &[(&str, &str)]` mapping filename → content
/// for every file in `toolkit/`. Symlinks are resolved so `include_str!` reads the
/// real file.
///
/// `skill_files.rs` — `HEADER: &str` for `header.md`, plus
/// `SKILL_FILES: &[(&str, &str, &str)]` mapping (segment name, title, content)
/// for every other `.md` file in `skills/`. The title is extracted from the first
/// `# Heading` line. Non-`.md` files (like `analyze.js`) are skipped.
fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();

    println!("cargo::rerun-if-changed=toolkit");
    println!("cargo::rerun-if-changed=skills");

    generate_toolkit(&manifest_dir, &out_dir);
    generate_skills(&manifest_dir, &out_dir);
}

fn resolve_path(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

fn generate_toolkit(manifest_dir: &str, out_dir: &str) {
    let toolkit_dir = Path::new(manifest_dir).join("toolkit");
    let dest = Path::new(out_dir).join("toolkit_files.rs");

    let mut entries: Vec<(String, String)> = Vec::new();
    if toolkit_dir.is_dir() {
        for entry in fs::read_dir(&toolkit_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_file() || path.is_symlink() {
                let name = entry.file_name().to_string_lossy().to_string();
                entries.push((name, resolve_path(&path)));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut code = String::from("pub const TOOLKIT_FILES: &[(&str, &str)] = &[\n");
    for (name, path) in &entries {
        code.push_str(&format!("    ({:?}, include_str!({:?})),\n", name, path));
    }
    code.push_str("];\n");
    fs::write(dest, code).unwrap();
}

fn generate_skills(manifest_dir: &str, out_dir: &str) {
    let skills_dir = Path::new(manifest_dir).join("skills");
    let dest = Path::new(out_dir).join("skill_files.rs");

    let mut code = String::new();
    let mut segments: Vec<(String, String, String)> = Vec::new(); // (name, title, path)

    for entry in fs::read_dir(&skills_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.ends_with(".md") {
            continue;
        }
        let canonical = resolve_path(&path);
        if name == "header.md" {
            code.push_str(&format!(
                "pub const HEADER: &str = include_str!({:?});\n",
                canonical
            ));
            continue;
        }
        let stem = name.trim_end_matches(".md").to_string();
        let title = extract_title(&path).unwrap_or_else(|| stem.clone());
        segments.push((stem, title, canonical));
    }
    segments.sort_by(|a, b| a.0.cmp(&b.0));

    code.push_str("pub const SKILL_FILES: &[(&str, &str, &str)] = &[\n");
    for (name, title, path) in &segments {
        code.push_str(&format!(
            "    ({:?}, {:?}, include_str!({:?})),\n",
            name, title, path
        ));
    }
    code.push_str("];\n");
    fs::write(dest, code).unwrap();
}

/// Extract the first `# Heading` from a markdown file.
fn extract_title(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    content
        .lines()
        .find(|l| l.starts_with("# "))
        .map(|l| l.trim_start_matches("# ").to_string())
}
