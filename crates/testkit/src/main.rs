//! Sandbox administration, generator, and independent oracle CLI.

#![deny(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use bigcp_testkit::generator::Scenario;
use bigcp_testkit::sandbox::initialize_empty;
use bigcp_testkit::{SandboxRoot, check_trees, generate};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "bigcp-testkit", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Marks an empty directory as a dedicated test sandbox.
    Init {
        /// Empty directory to dedicate to tests.
        sandbox: PathBuf,
    },
    /// Generates one bounded YAML scenario.
    Gen {
        /// Marked sandbox root.
        sandbox: PathBuf,
        /// New relative tree root.
        root: PathBuf,
        /// Scenario YAML.
        scenario: PathBuf,
    },
    /// Runs the independent oracle.
    Check {
        /// Marked sandbox root.
        sandbox: PathBuf,
        /// Relative source tree.
        source: PathBuf,
        /// Relative destination tree.
        destination: PathBuf,
    },
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("bigcp-testkit: {error:#}");
            ExitCode::from(1)
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    match cli.command {
        Command::Init { sandbox } => {
            let sandbox = initialize_empty(&sandbox)?;
            println!("{}", sandbox.path().display());
            Ok(0)
        }
        Command::Gen {
            sandbox,
            root,
            scenario,
        } => {
            let sandbox = SandboxRoot::open(&sandbox)?;
            let file = std::fs::File::open(&scenario)
                .with_context(|| format!("open {}", scenario.display()))?;
            let scenario: Scenario =
                serde_yaml::from_reader(file).context("parse scenario YAML")?;
            let report = generate(&sandbox, &root, &scenario)?;
            serde_json::to_writer_pretty(std::io::stdout(), &report)?;
            println!();
            Ok(0)
        }
        Command::Check {
            sandbox,
            source,
            destination,
        } => {
            let sandbox = SandboxRoot::open(&sandbox)?;
            let report = check_trees(&sandbox, &source, &destination)?;
            serde_json::to_writer_pretty(std::io::stdout(), &report)?;
            println!();
            Ok(if report.mismatches == 0 { 0 } else { 2 })
        }
    }
}
