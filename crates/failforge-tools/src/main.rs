//! Offline regression and measurement workflows for the production executables.
#[cfg(unix)]
mod benchmark;
mod common;
mod installer;
mod package;
mod package_smoke;
mod public_regression;
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
    #[cfg(unix)]
    Benchmark(benchmark::Options),
    /// Install the pinned or latest stable Flutter SDK after SHA-256 verification.
    InstallTestFlutter {
        #[arg(long)]
        destination: PathBuf,
        #[arg(long, default_value = installer::VERSION, value_parser = [installer::VERSION, "stable"])]
        version: String,
    },
    /// Exercise all 19 Dart semantic operators with real VM observations.
    SemanticE2e(semantic::Options),
    /// Run sequential public CLI/profile, classification and cancellation fixtures.
    Regression(regression::Options),
    /// Repeat explicitly selected archived public-project scopes.
    PublicRegression(public_regression::Options),
    /// Build a native-host release archive with verified license material.
    Package(package::Options),
    /// Verify a relocated installation with the real Dart SDK.
    PackageSmoke(package_smoke::Options),
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
        #[cfg(unix)]
        Commands::Benchmark(options) => benchmark::run(options),
        Commands::InstallTestFlutter {
            destination,
            version,
        } => installer::install(&destination, &version),
        Commands::SemanticE2e(options) => semantic::run(options),
        Commands::Regression(options) => regression::run(options),
        Commands::PublicRegression(options) => public_regression::run(options),
        Commands::Package(options) => package::run(options),
        Commands::PackageSmoke(options) => package_smoke::run(options),
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
