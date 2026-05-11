use std::env;
use std::fs;
use std::path::Path;

/// Generates files in OUT_DIR for the new Agent Skills directory structure:
///
/// `skills.rs` — Constants for all skills:
///   - `SKILL_DIRS: &[SkillDir]` with name, description, skill_md content, and file lists
///   - `HEADER: &str` auto-generated overview from skill frontmatter
///   - `TOOLKIT_FILES: &[(&str, &str)]` for the `agents toolkit` command
fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let out_dir = env::var("OUT_DIR").unwrap();

    println!("cargo::rerun-if-changed=skills");
    println!("cargo::rerun-if-changed=ui");
    println!("cargo::rerun-if-changed=README_TELEMETRY.md");

    let skills_dir = Path::new(&manifest_dir).join("skills");
    let mut skills: Vec<SkillInfo> = Vec::new();

    // Walk each subdirectory in skills/
    if skills_dir.is_dir() {
        let mut entries: Vec<_> = fs::read_dir(&skills_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();
        entries.sort_by_key(|e| e.file_name());

        for entry in entries {
            let dir_path = entry.path();
            let skill_md_path = dir_path.join("SKILL.md");
            if !skill_md_path.exists() {
                continue;
            }
            let skill_md = fs::read_to_string(&skill_md_path).unwrap();
            let (name, description) = parse_frontmatter(&skill_md);
            if name.is_empty() || description.is_empty() {
                panic!(
                    "SKILL.md in {:?} has invalid frontmatter: name and description are required",
                    dir_path
                );
            }
            let body = strip_frontmatter(&skill_md);

            // Collect all files in the skill directory (recursively)
            let mut files: Vec<(String, String)> = Vec::new(); // (relative_path, absolute_path)
            collect_files(&dir_path, &dir_path, &mut files);

            skills.push(SkillInfo {
                name,
                description,
                body,
                files,
            });
        }
    }

    // Generate the setup skill from README
    let setup_body = generate_setup_from_readme(&manifest_dir, &out_dir);
    skills.push(SkillInfo {
        name: "dial9-setup".to_string(),
        description: "How to instrument your app with dial9-tokio-telemetry. Covers prerequisites, macro and manual setup, the tracing layer, and wake event tracking.".to_string(),
        body: setup_body,
        files: vec![("SKILL.md".to_string(), resolve_path(&Path::new(&out_dir).join("dial9-setup-SKILL.md")))],
    });
    skills.sort_by(|a, b| a.name.cmp(&b.name));

    // Generate the header from skill descriptions
    let header = generate_header(&skills);
    let header_path = Path::new(&out_dir).join("header.md");
    fs::write(&header_path, &header).unwrap();

    // Generate skills.rs
    let dest = Path::new(&out_dir).join("skills.rs");
    let mut code = String::new();

    // HEADER constant
    code.push_str(&format!(
        "pub const HEADER: &str = include_str!({:?});\n\n",
        resolve_path(&header_path)
    ));

    // Write stripped body files to OUT_DIR for the `skill` command
    for skill in &skills {
        let body_path = Path::new(&out_dir).join(format!("{}-body.md", skill.name));
        fs::write(&body_path, &skill.body).unwrap();
    }

    // SKILL_DIRS array
    code.push_str("pub struct SkillDir {\n");
    code.push_str("    pub name: &'static str,\n");
    code.push_str("    pub description: &'static str,\n");
    code.push_str("    pub body: &'static str,\n");
    code.push_str("    pub files: &'static [(&'static str, &'static str)],\n");
    code.push_str("}\n\n");
    code.push_str("pub const SKILL_DIRS: &[SkillDir] = &[\n");
    for skill in &skills {
        let body_path = Path::new(&out_dir).join(format!("{}-body.md", skill.name));
        code.push_str("    SkillDir {\n");
        code.push_str(&format!("        name: {:?},\n", skill.name));
        code.push_str(&format!("        description: {:?},\n", skill.description));
        code.push_str(&format!(
            "        body: include_str!({:?}),\n",
            resolve_path(&body_path)
        ));
        code.push_str("        files: &[\n");
        for (rel, abs) in &skill.files {
            code.push_str(&format!(
                "            ({:?}, include_str!({:?})),\n",
                rel, abs
            ));
        }
        code.push_str("        ],\n");
        code.push_str("    },\n");
    }
    code.push_str("];\n\n");

    // TOOLKIT_FILES: collect scripts from dial9-toolkit skill
    let toolkit_skill = skills.iter().find(|s| s.name == "dial9-toolkit");
    code.push_str("pub const TOOLKIT_FILES: &[(&str, &str)] = &[\n");
    if let Some(tk) = toolkit_skill {
        for (rel, abs) in &tk.files {
            if rel.starts_with("scripts/") {
                let filename = rel.strip_prefix("scripts/").unwrap();
                code.push_str(&format!("    ({:?}, include_str!({:?})),\n", filename, abs));
            }
        }
    }
    code.push_str("];\n");

    fs::write(dest, code).unwrap();
}

struct SkillInfo {
    name: String,
    description: String,
    body: String,
    files: Vec<(String, String)>, // (relative_path, resolved_absolute_path)
}

fn resolve_path(path: &Path) -> String {
    fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Recursively collect all files in a directory, resolving symlinks.
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let mut entries: Vec<_> = fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).collect();
    entries.sort_by_key(|e| e.file_name());

    for entry in entries {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap()
            .to_string_lossy()
            .to_string();

        if path.is_dir() && !path.is_symlink() {
            collect_files(base, &path, out);
        } else if path.is_file() || path.is_symlink() {
            out.push((rel, resolve_path(&path)));
        }
    }
}

/// Parse YAML frontmatter to extract name and description.
fn parse_frontmatter(content: &str) -> (String, String) {
    let mut name = String::new();
    let mut description = String::new();

    let skip = if content.starts_with("---\r\n") {
        5
    } else if content.starts_with("---\n") {
        4
    } else {
        return (name, description);
    };

    let rest = &content[skip..];
    if let Some(end) = rest.find("\n---") {
        let frontmatter = &rest[..end];
        for line in frontmatter.lines() {
            if let Some(val) = line.strip_prefix("name:") {
                name = val.trim().to_string();
            } else if let Some(val) = line.strip_prefix("description:") {
                description = val.trim().to_string();
            }
        }
    }
    (name, description)
}

/// Strip YAML frontmatter, returning just the markdown body.
fn strip_frontmatter(content: &str) -> String {
    let skip = if content.starts_with("---\r\n") {
        5
    } else if content.starts_with("---\n") {
        4
    } else {
        return content.to_string();
    };

    let rest = &content[skip..];
    if let Some(end) = rest.find("\n---") {
        let after = &rest[end + 4..]; // skip "\n---"
        // Skip the newline after closing ---
        after
            .strip_prefix("\r\n")
            .or_else(|| after.strip_prefix('\n'))
            .unwrap_or(after)
            .to_string()
    } else {
        content.to_string()
    }
}

/// Generate the overview header from skill metadata.
fn generate_header(skills: &[SkillInfo]) -> String {
    let mut out = String::from("# dial9 Trace Analysis Skills\n\n");
    out.push_str("dial9 traces capture the internal behavior of a Tokio async runtime: task polling, worker thread activity, queue depths, CPU profiling samples, scheduling delays, and task lifecycle events.\n\n");
    out.push_str("## Quick start\n\n");
    out.push_str("```bash\n");
    out.push_str("# Extract the JS analysis toolkit\n");
    out.push_str("dial9-viewer agents toolkit /tmp/d9-toolkit\n");
    out.push_str("node /tmp/d9-toolkit/analyze.js <trace.bin or directory>\n\n");
    out.push_str("# Unpack all skills as an Agent Skills directory\n");
    out.push_str("dial9-viewer agents skills /tmp/d9-skills\n");
    out.push_str("```\n\n");
    out.push_str("## Available skills\n\n");
    out.push_str("| Skill | Description |\n");
    out.push_str("|-------|-------------|\n");
    for skill in skills {
        let desc = if skill.description.len() > 120 {
            let boundary = skill
                .description
                .char_indices()
                .take_while(|(i, _)| *i < 117)
                .last()
                .map(|(i, c)| i + c.len_utf8())
                .unwrap_or(117);
            format!("{}...", &skill.description[..boundary])
        } else {
            skill.description.clone()
        };
        out.push_str(&format!("| `{}` | {} |\n", skill.name, desc));
    }
    out.push_str("\n## CLI commands\n\n");
    out.push_str("| Command | Description |\n");
    out.push_str("|---------|-------------|\n");
    out.push_str("| `agents` | Print this overview |\n");
    out.push_str("| `agents skill <name>` | Print a specific skill's instructions |\n");
    out.push_str("| `agents toolkit <dir>` | Extract JS analysis scripts to a directory |\n");
    out.push_str("| `agents skills <dir>` | Unpack all skills (Agent Skills spec layout) |\n");
    out
}

/// Sections from the dial9-tokio-telemetry README to include in the setup skill.
const SETUP_SECTIONS: &[&str] = &[
    "Quick Start",
    "Tokio events",
    "Tracing span events (opt-in)",
];

/// Generate the setup skill from the crate README.
fn generate_setup_from_readme(manifest_dir: &str, out_dir: &str) -> String {
    let readme_path = Path::new(manifest_dir).join("README_TELEMETRY.md");
    let readme = fs::read_to_string(&readme_path)
        .unwrap_or_else(|e| panic!("failed to read {}: {e}", readme_path.display()));

    let mut body = String::from("# Instrumenting your app with dial9\n\n");

    for &heading in SETUP_SECTIONS {
        let section = extract_section(&readme, heading)
            .unwrap_or_else(|| panic!("README section '{heading}' not found; was it renamed?"));
        body.push_str(&section);
        body.push('\n');
    }

    // Write the full SKILL.md (with frontmatter) for the unpack command
    let mut full = String::from(
        "---\nname: dial9-setup\ndescription: How to instrument your app with dial9-tokio-telemetry. Covers quick start, Tokio events, and the tracing layer.\n---\n\n",
    );
    full.push_str(&body);

    let dest = Path::new(out_dir).join("dial9-setup-SKILL.md");
    fs::write(&dest, &full).unwrap();

    body
}

/// Extract a markdown section by heading text.
fn extract_section(markdown: &str, heading: &str) -> Option<String> {
    let lines: Vec<&str> = markdown.lines().collect();
    let start = lines.iter().position(|l| {
        let trimmed = l.trim();
        trimmed.starts_with('#') && trimmed.trim_start_matches('#').trim_start() == heading
    })?;
    let level = lines[start].chars().take_while(|&c| c == '#').count();
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.starts_with('#') && l.chars().take_while(|&c| c == '#').count() <= level)
        .map(|i| start + 1 + i)
        .unwrap_or(lines.len());
    Some(lines[start..end].join("\n"))
}
