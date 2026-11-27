//! CLI argument definitions using clap derive.
//!
//! # Design notes
//!
//! **Chosen:** clap 4.x with `derive` feature. The subcommands map 1:1 to
//! the library's entry points. Exit codes are mapped in `cli/mod.rs`.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Nadir — regression testing for local HTTP workflows.
#[derive(Parser, Debug)]
#[command(name = "nadir", version = env!("CARGO_PKG_VERSION"))]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Validate an experiment YAML file (check schema and static rules).
    Validate {
        /// Path to the experiment YAML file.
        experiment: PathBuf,
    },

    /// Run an experiment: enumerated cases, plus random cases with strategy: fuzz.
    Run {
        /// Path to the experiment YAML file.
        experiment: PathBuf,
        /// Corpus import is currently unavailable.
        #[arg(long, hide = true)]
        corpus: Option<PathBuf>,
    },

    /// Repeat a saved regression case against the local service.
    Replay {
        /// Path to the experiment YAML file.
        experiment: PathBuf,
        /// Path to the saved case JSON file.
        #[arg(long)]
        case: PathBuf,
    },
}
