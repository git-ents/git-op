use clap::{Parser, Subcommand};
use std::io::{self, Read};

#[derive(Debug, Parser)]
#[command(
    name = "git-op",
    version,
    about = "Record Git repository metadata snapshots"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Process a Git reference-transaction hook invocation.
    ReferenceTransaction {
        /// The phase supplied by Git: prepared, committed, or aborted.
        phase: String,
    },
    /// Install the reference-transaction hook globally or in one repository.
    Install {
        /// Install only in the current repository instead of Git's global template.
        #[arg(long)]
        local: bool,
    },
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Command::ReferenceTransaction { phase } => {
            let mut input = Vec::new();
            io::stdin().read_to_end(&mut input)?;
            let repo = gix::discover(".")?;
            git_op::reference_transaction(&repo, &phase, &input)?;
        }
        Command::Install { local: true } => {
            let repo = gix::discover(".")?;
            git_op::install_local(&repo)?;
        }
        Command::Install { local: false } => git_op::install_global()?,
    }
    Ok(())
}
