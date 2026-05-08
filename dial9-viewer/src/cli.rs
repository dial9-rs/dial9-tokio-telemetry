use clap::{Parser, Subcommand};
use std::path::PathBuf;

mod skills {
    include!(concat!(env!("OUT_DIR"), "/skills.rs"));

    pub fn get(name: &str) -> Option<&'static str> {
        SKILL_DIRS.iter().find(|s| s.name == name).map(|s| s.body)
    }
}

#[derive(Parser, Debug)]
#[command(
    name = "dial9",
    about = "Trace browser and viewer for dial9-tokio-telemetry"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Port to listen on (when running without a subcommand)
    #[arg(long, default_value = "3000", global = true)]
    port: u16,

    /// S3 bucket name (when running without a subcommand)
    #[arg(long, global = true)]
    bucket: Option<String>,

    /// S3 key prefix (when running without a subcommand)
    #[arg(long, global = true)]
    prefix: Option<String>,

    /// Serve traces from a local directory instead of S3
    #[arg(long, global = true, conflicts_with = "bucket")]
    local_dir: Option<PathBuf>,

    /// Dev mode: serve UI files from disk for faster iteration
    #[arg(long, global = true)]
    dev: bool,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Agent skill documentation and analysis toolkit
    Agents {
        #[command(subcommand)]
        action: Option<AgentsAction>,
    },
    /// Start the web server
    Serve {},
}

#[derive(Subcommand, Debug)]
enum AgentsAction {
    /// Copy the analysis toolkit (JS modules) to a directory
    Toolkit {
        /// Directory to write toolkit files into (created if missing)
        path: PathBuf,
    },
    /// Print a specific skill's instructions
    Skill {
        /// Skill name (e.g. dial9-trace-loading, dial9-red-flags)
        name: String,
    },
    /// Unpack all skills as an Agent Skills spec directory
    Skills {
        /// Directory to write skills into (created if missing)
        path: PathBuf,
    },
}

/// Run the CLI. Call this from your binary's `main()`.
pub async fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Agents { action }) => match action {
            None => print!("{}", skills::HEADER),
            Some(AgentsAction::Toolkit { path }) => {
                std::fs::create_dir_all(&path)?;
                for (name, content) in skills::TOOLKIT_FILES {
                    std::fs::write(path.join(name), content)?;
                }
                let abs = std::fs::canonicalize(&path)?;
                eprintln!("Toolkit written to {}", abs.display());
                eprintln!(
                    "Run: node {}/analyze.js <trace.bin or directory>",
                    abs.display()
                );
            }
            Some(AgentsAction::Skill { name }) => match skills::get(&name) {
                Some(content) => print!("{}", content),
                None => {
                    eprintln!("Unknown skill: {name}");
                    eprintln!("Available skills:");
                    for skill in skills::SKILL_DIRS {
                        eprintln!("  {:24} {}", skill.name, skill.description);
                    }
                    std::process::exit(1);
                }
            },
            Some(AgentsAction::Skills { path }) => {
                for skill in skills::SKILL_DIRS {
                    let skill_dir = path.join(skill.name);
                    for (rel_path, content) in skill.files {
                        let file_path = skill_dir.join(rel_path);
                        if let Some(parent) = file_path.parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        std::fs::write(&file_path, content)?;
                    }
                }
                let abs = std::fs::canonicalize(&path)?;
                eprintln!("Skills unpacked to {}", abs.display());
                eprintln!("Add to .kiro/skills/ or point your agent at this directory.");
            }
        },
        Some(Commands::Serve {}) | None => {
            return crate::serve(cli.port, cli.bucket, cli.prefix, cli.local_dir, cli.dev).await;
        }
    }
    Ok(())
}
