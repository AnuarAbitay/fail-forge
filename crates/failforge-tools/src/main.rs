//! Offline regression and measurement workflows for the production executables.
mod benchmark;
mod common;
mod installer;
mod regression;
mod semantic;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "FailForge developer regression and measurement tools")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compare repeated Runner calls with fresh CLI processes.
    Benchmark(benchmark::Options),
    /// Install the exact CI Flutter SDK after SHA-256 verification.
    InstallTestFlutter {
        #[arg(long)]
        destination: PathBuf,
    },
    /// Exercise all 19 Dart semantic operators with real VM observations.
    SemanticE2e(semantic::Options),
    /// Run sequential public CLI/profile, classification and cancellation fixtures.
    Regression(regression::Options),
}

fn main() -> std::process::ExitCode {
    let _signals = match common::install_interrupts() {
        Ok(guard) => guard,
        Err(error) => {
            eprintln!("failforge-tools: cannot install signal handlers: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let outcome = match Cli::parse().command {
        Commands::Benchmark(options) => benchmark::run(options),
        Commands::InstallTestFlutter { destination } => installer::install(&destination),
        Commands::SemanticE2e(options) => semantic::run(options),
        Commands::Regression(options) => regression::run(options),
    };
    match outcome {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("failforge-tools: {error}");
            common::interrupt_signal().map_or(std::process::ExitCode::FAILURE, |signal| {
                std::process::ExitCode::from((128 + signal) as u8)
            })
        }
    }
}
