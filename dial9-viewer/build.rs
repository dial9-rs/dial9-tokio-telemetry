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
    println!("cargo::rerun-if-changed=ui");
    println!("cargo::rerun-if-changed=../dial9-tokio-telemetry/README.md");

    generate_toolkit(&manifest_dir, &out_dir);
    generate_setup_from_readme(&manifest_dir, &out_dir);
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

    if skills_dir.is_dir() {
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
    }
    // Fallback if header.md wasn't found
    if !code.contains("HEADER") {
        code.push_str("pub const HEADER: &str = \"\";\n");
    }
    segments.sort_by(|a, b| a.0.cmp(&b.0));

    // The setup skill is generated from the README by generate_setup_from_readme().
    // Include it here so it appears alongside the hand-written skills.
    let setup_path = Path::new(out_dir).join("setup_skill.md");
    segments.push((
        "setup".to_string(),
        "Instrumenting your app with dial9".to_string(),
        resolve_path(&setup_path),
    ));
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

/// Sections from the dial9-tokio-telemetry README to include in the setup skill.
/// These are extracted by exact `## Heading` match and concatenated in order.
/// If a heading is renamed in the README, the build fails loudly.
const SETUP_SECTIONS: &[&str] = &[
    "Prerequisites",
    "Quick start",
    "The root future is not instrumented",
    "Tracing span events (opt-in)",
    "Wake event tracking",
];

/// Extract sections from the crate README and write them as `setup.md` in the
/// skills directory. This keeps the README as the single source of truth for
/// instrumentation docs.
fn generate_setup_from_readme(manifest_dir: &str, out_dir: &str) {
    let readme_path = Path::new(manifest_dir)
        .join("../dial9-tokio-telemetry/README.md");
    let readme = fs::read_to_string(&readme_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", readme_path.display()));

    let mut output = String::from("# Instrumenting your app with dial9\n\n");
    output.push_str("*This content is extracted from the [dial9-tokio-telemetry README](https://github.com/dial9-rs/dial9-tokio-telemetry).*\n\n");

    for &heading in SETUP_SECTIONS {
        let section = extract_section(&readme, heading)
            .unwrap_or_else(|| panic!("README section '## {heading}' not found; was it renamed?"));
        output.push_str(&section);
        output.push('\n');
    }

    let dest = Path::new(out_dir).join("setup_skill.md");
    fs::write(&dest, &output).unwrap();
}

/// Extract a `## Heading` section from markdown, including everything up to the
/// next `## ` heading (or end of file).
fn extract_section(markdown: &str, heading: &str) -> Option<String> {
    let target = format!("## {heading}");
    let lines: Vec<&str> = markdown.lines().collect();
    let start = lines.iter().position(|l| l.trim() == target)?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.starts_with("## "))
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());
    Some(lines[start..end].join("\n"))
}
