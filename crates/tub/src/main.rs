//! tub - an interactive Rust terminal client for DeepSeek Harness.
//!
//! See README.md for the product story; ARCHITECTURE.md for the design.

use std::process::ExitCode;

use clap::Parser;
use tub::app;
use tub::cli::{Cli, Command, SharedArgs, TuiArgs};
use tub::headless;

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let outcome = match cli.command {
        Some(Command::Run(args)) => headless::run(args).await,
        Some(Command::Tui(args)) => app::run(args).await,
        None => {
            app::run(TuiArgs {
                shared: SharedArgs::from_env(),
                session: None,
            })
            .await
        }
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tub: {error:#}");
            ExitCode::FAILURE
        }
    }
}
