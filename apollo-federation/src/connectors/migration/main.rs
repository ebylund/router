//! `connect-migrate` — CLI for moving Apollo Connectors customers
//! between `connect/v0.X` specs.
//!
//! Built behind the `connect-migrate` feature of the `apollo-federation`
//! crate so the dependency on `clap` doesn't enter the default build
//! graph for library consumers:
//!
//!     cargo build --release --bin connect-migrate --features connect-migrate
//!
//! The library helpers the binary calls into live in the sibling
//! `mod.rs` and are part of the `apollo_federation::connectors::migration`
//! public surface.

use std::fs::File;
use std::io::BufWriter;
use std::io::{self};
use std::path::PathBuf;
use std::process::ExitCode;

use apollo_federation::connectors::migration::AGENT_GUIDE;
use apollo_federation::connectors::migration::analyze;
use clap::Parser;
use clap::Subcommand;
use clap::ValueEnum;

/// Binary version reported by `connect-migrate --version`.
///
/// Decoupled from `apollo-federation`'s package version because
/// `connect-migrate` ships from a separate release repo with its own
/// cadence. The release CI overrides this via the
/// `CONNECT_MIGRATE_VERSION` env var at compile time (set from the
/// pushed git tag); local dev builds get the `-dev` suffix.
const VERSION: &str = match option_env!("CONNECT_MIGRATE_VERSION") {
    Some(v) => v,
    None => "0.0.0-dev",
};

#[derive(Parser, Debug)]
#[command(
    name = "connect-migrate",
    about = "Help upgrade Apollo Connectors schemas across connect/v0.X spec versions",
    long_about = None,
    version = VERSION,
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Print the developer-facing migration guide that an agent can
    /// follow when walking a customer through a v0.3 → v0.4 upgrade.
    ///
    /// The guide is embedded in the binary at compile time. Print it
    /// and pipe to your agent of choice, or read it manually.
    AgentGuide,

    /// Scan a project for `@connect(selection: …)` sites that change
    /// meaning between `connect/v0.3` and `connect/v0.4`.
    ///
    /// Walks the given paths (defaults to `.`), parses every
    /// `.graphql` file, and emits a `recommendations.md` (or JSONL via
    /// `--format=json`) describing each site that needs a decision
    /// before upgrading.
    Analyze {
        /// Paths to scan. May be files or directories. Directories
        /// are walked recursively for `.graphql` files. Defaults to
        /// the current directory.
        #[arg(default_value = ".")]
        paths: Vec<PathBuf>,

        /// Output path. Use `-` to write to stdout. Defaults to
        /// `recommendations.md` in the current directory (or stdout
        /// when `--format=json`).
        #[arg(long, short)]
        output: Option<PathBuf>,

        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Markdown)]
        format: OutputFormat,
    },
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum OutputFormat {
    /// Human-readable recommendations.md with structured identity
    /// comments. The format `apply` reads back. Default.
    Markdown,
    /// One JSON record per site, separated by newlines. Useful for
    /// tool-consuming agents that prefer typed records over markdown.
    Json,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::AgentGuide => {
            print!("{}", AGENT_GUIDE);
            ExitCode::SUCCESS
        }
        Command::Analyze { paths, output, format } => match run_analyze(paths, output, format) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("connect-migrate analyze: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

fn run_analyze(
    paths: Vec<PathBuf>,
    output: Option<PathBuf>,
    format: OutputFormat,
) -> io::Result<()> {
    let project_root = std::env::current_dir()?;
    let sites = analyze::analyze(&paths, &project_root);

    // Decide where to write.
    let default_path = match format {
        OutputFormat::Markdown => Some(PathBuf::from("recommendations.md")),
        OutputFormat::Json => None,
    };
    let target = output.or(default_path);

    let stdout = io::stdout();
    let mut sink: Box<dyn std::io::Write> = match target.as_deref() {
        Some(p) if p.as_os_str() != "-" => Box::new(BufWriter::new(File::create(p)?)),
        _ => Box::new(BufWriter::new(stdout.lock())),
    };

    match format {
        OutputFormat::Markdown => {
            analyze::write_markdown(&mut sink, &sites, &project_root, VERSION)?;
        }
        OutputFormat::Json => {
            analyze::write_jsonl(&mut sink, &sites)?;
        }
    }

    let total = sites.len();
    let kept = sites
        .iter()
        .filter(|s| matches!(s.recommendation, analyze::Recommendation::KeepV03))
        .count();
    let embraced = sites
        .iter()
        .filter(|s| matches!(s.recommendation, analyze::Recommendation::EmbraceV04))
        .count();
    let ambiguous = sites
        .iter()
        .filter(|s| matches!(s.recommendation, analyze::Recommendation::Ambiguous))
        .count();
    eprintln!(
        "scanned project; {total} site(s) need attention ({kept} keep-v0.3 · {embraced} embrace-v0.4 · {ambiguous} ambiguous)"
    );
    if let Some(path) = target.as_deref() {
        if path.as_os_str() != "-" {
            eprintln!("wrote: {}", path.display());
        }
    }

    Ok(())
}
