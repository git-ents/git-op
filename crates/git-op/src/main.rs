mod cli;
mod exe;
mod log;

use clap::Parser;

/// Parse command-line arguments and execute the selected operation.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    exe::run(cli::Cli::parse().command)
}
