//! Command-line parsing for `git-op`.

use clap::{Parser, Subcommand};

/// Command-line arguments accepted by `git-op`.
#[derive(Debug, Parser)]
#[command(
    name = "git-op",
    version,
    about = "Record Git repository metadata snapshots"
)]
pub(crate) struct Cli {
    /// The operation selected by the caller.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Operations supported by `git-op`.
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Process a Git reference-transaction hook invocation.
    #[command(hide = true)]
    ReferenceTransaction {
        /// The phase supplied by Git: prepared, committed, or aborted.
        phase: String,
    },
    /// Install the reference-transaction hook in the current repository.
    Install {
        /// Install in Git's global template instead of the current repository.
        #[arg(long)]
        global: bool,
    },
    /// Remove the reference-transaction hook from the current repository.
    Uninstall {
        /// Remove it from Git's global template instead of the current repository.
        #[arg(long)]
        global: bool,
    },
    /// Record the current repository state onto the operation log.
    Snap,
    /// Show the recorded operation-log snapshots, most recent first.
    Log {
        /// Limit output to the first N snapshots.
        #[arg(short = 'n', long = "max-count")]
        max_count: Option<usize>,
        /// Show the oldest snapshots first.
        #[arg(long)]
        reverse: bool,
        /// List every changed ref instead of the first few.
        #[arg(short = 'v', long, conflicts_with_all = ["oneline", "json"])]
        verbose: bool,
        /// Do not pipe terminal output through Git's configured pager.
        #[arg(long)]
        no_pager: bool,
        /// Show each snapshot as one line: abbreviated id and message summary.
        #[arg(long, conflicts_with = "json")]
        oneline: bool,
        /// Show each snapshot as one JSON object per line (JSON Lines).
        #[arg(long)]
        json: bool,
    },
    /// Restore refs, repository config, description, working tree, and index from an operation snapshot.
    Restore {
        /// The operation commit to restore.
        oid: Option<String>,
        /// Show the action without changing the repository or operation log.
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Restore the state before the latest logical operation.
    Undo {
        /// Show the action without changing the repository or operation log.
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Reapply the next logical operation after an undo.
    Redo {
        /// Show the action without changing the repository or operation log.
        #[arg(short = 'n', long)]
        dry_run: bool,
    },
    /// Decode one operation-log entry.
    Show {
        /// The operation commit to decode, defaulting to the log tip.
        oid: Option<String>,
    },
}

#[cfg(test)]
#[expect(
    clippy::assertions_on_result_states,
    clippy::panic,
    reason = "tests use panics and direct indexing to express failed expectations"
)]
#[path = "tests/cli.rs"]
mod tests;
