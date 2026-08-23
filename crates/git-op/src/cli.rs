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
    /// Install the reference-transaction hook globally or in one repository.
    Install {
        /// Install only in the current repository instead of Git's global template.
        #[arg(long)]
        local: bool,
    },
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
        oid: String,
    },
    /// Restore the state before an operation-log commit, including the working tree and index.
    Undo {
        /// The operation commit to undo; defaults to the latest operation.
        oid: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{Cli, Command};

    #[test]
    fn undo_parses_an_optional_operation_hash() {
        let cli =
            Cli::try_parse_from(["git-op", "undo", "abc123"]).expect("parse undo operation hash");
        let Command::Undo { oid } = cli.command else {
            panic!("expected the undo command");
        };
        assert_eq!(oid.as_deref(), Some("abc123"));

        let cli = Cli::try_parse_from(["git-op", "undo"]).expect("parse undo without hash");
        let Command::Undo { oid } = cli.command else {
            panic!("expected the undo command");
        };
        assert_eq!(oid, None);
    }

    #[test]
    fn log_parses_max_count_and_reverse() {
        let cli = Cli::try_parse_from(["git-op", "log", "-n", "2", "--reverse"])
            .expect("parse log with max-count and reverse");
        let Command::Log {
            max_count,
            reverse,
            verbose,
            no_pager,
            oneline,
            json,
        } = cli.command
        else {
            panic!("expected the log command");
        };
        assert_eq!(max_count, Some(2));
        assert!(reverse);
        assert!(!verbose);
        assert!(!no_pager);
        assert!(!oneline);
        assert!(!json);
    }

    #[test]
    fn log_parses_no_pager() {
        let cli = Cli::try_parse_from(["git-op", "log", "--no-pager"]).expect("parse --no-pager");
        let Command::Log { no_pager, .. } = cli.command else {
            panic!("expected the log command");
        };
        assert!(no_pager);
    }

    #[test]
    fn log_rejects_oneline_and_json_together() {
        let error = Cli::try_parse_from(["git-op", "log", "--oneline", "--json"])
            .expect_err("--oneline and --json must conflict");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn log_rejects_unknown_git_log_flags() {
        let error = Cli::try_parse_from(["git-op", "log", "--all"])
            .expect_err("--all is not a git-op log flag");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }

    #[test]
    fn log_rejects_revision_arguments() {
        let error = Cli::try_parse_from(["git-op", "log", "HEAD~1..HEAD"])
            .expect_err("git-op log takes no positional revision argument");
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}
