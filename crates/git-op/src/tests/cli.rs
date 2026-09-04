use clap::{CommandFactory, Parser};

use super::{Cli, Command};

/// Verify that `snap` takes no arguments and is listed as a visible command.
#[test]
fn snap_parses_without_arguments_and_is_listed() {
    let cli = Cli::try_parse_from(["git-op", "snap"]).expect("parse snap");
    assert!(matches!(cli.command, Command::Snap));
    assert!(Cli::try_parse_from(["git-op", "snap", "extra"]).is_err());
    assert!(
        Cli::command()
            .get_subcommands()
            .any(|command| command.get_name() == "snap" && !command.is_hide_set()),
        "snap should be a visible subcommand"
    );
}

#[test]
fn undo_is_zero_argument_and_accepts_dry_run() {
    let cli = Cli::try_parse_from(["git-op", "undo", "-n"]).expect("parse undo dry-run");
    let Command::Undo { dry_run } = cli.command else {
        panic!("expected the undo command");
    };
    assert!(dry_run);
    assert!(Cli::try_parse_from(["git-op", "undo", "abc123"]).is_err());
}

#[test]
fn install_and_uninstall_default_to_local_and_accept_global() {
    let cli = Cli::try_parse_from(["git-op", "install"]).expect("parse install");
    let Command::Install { global } = cli.command else {
        panic!("expected the install command");
    };
    assert!(!global);

    let cli =
        Cli::try_parse_from(["git-op", "uninstall", "--global"]).expect("parse uninstall --global");
    let Command::Uninstall { global } = cli.command else {
        panic!("expected the uninstall command");
    };
    assert!(global);
}

#[test]
fn redo_accepts_dry_run() {
    let cli = Cli::try_parse_from(["git-op", "redo", "--dry-run"]).expect("parse redo");
    let Command::Redo { dry_run } = cli.command else {
        panic!("expected the redo command");
    };
    assert!(dry_run);
}

#[test]
fn restore_accepts_an_optional_operation_and_dry_run() {
    let cli =
        Cli::try_parse_from(["git-op", "restore", "abc123", "-n"]).expect("parse restore dry-run");
    let Command::Restore { oid, dry_run } = cli.command else {
        panic!("expected the restore command");
    };
    assert_eq!(oid.as_deref(), Some("abc123"));
    assert!(dry_run);
}

#[test]
fn show_takes_an_optional_operation() {
    let cli = Cli::try_parse_from(["git-op", "show"]).expect("parse show");
    let Command::Show { oid } = cli.command else {
        panic!("expected the show command");
    };
    assert_eq!(oid, None);
    let cli = Cli::try_parse_from(["git-op", "show", "abc123"]).expect("parse show with oid");
    let Command::Show { oid } = cli.command else {
        panic!("expected the show command");
    };
    assert_eq!(oid.as_deref(), Some("abc123"));
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
