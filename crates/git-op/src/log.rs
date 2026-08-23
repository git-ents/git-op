//! `git op log`: render the operation log directly, without shelling out to
//! `git log`.
//!
//! Walking the log ourselves means every accepted argument is a typed `clap`
//! field, so `clap` rejects anything else (`--all`, revision ranges, and so
//! on) before the repository is even opened.

use std::fmt;
use std::io::{self, IsTerminal, Write};
use std::process::{Child, Command, Stdio};

use anstream::AutoStream;
use anstyle::{AnsiColor, Style};
use gix::prelude::ObjectIdExt;
use gix::refs::Target;

use crate::exe::open_repository;

/// Style for the `●` glyph marking the most recent snapshot.
const HEAD_GLYPH_STYLE: Style = AnsiColor::Green.on_default().bold();
/// Style for the `○` glyph marking every other snapshot, and for the `│`
/// graph connector between entries.
const DIM_STYLE: Style = Style::new().dimmed();
/// Style for the abbreviated commit id, in both the default and oneline
/// formats.
const ID_STYLE: Style = AnsiColor::Yellow.on_default().bold();
/// Style for the names of changed parts and refs.
const CHANGED_STYLE: Style = AnsiColor::Cyan.on_default();

/// The number of changed refs listed per snapshot before the rest are
/// summarized; `--verbose` lists them all.
const MAX_REF_LINES: usize = 10;

/// One rendered operation-log entry, extracted from a snapshot commit.
///
/// Extraction needs repository access (for example to abbreviate the id
/// unambiguously); rendering does not, so it is kept in pure functions that
/// take entries already built by [`extract_entries`].
struct Entry {
    id: String,
    abbreviated_id: String,
    time: gix::date::Time,
    /// Whether this is the most recent snapshot (`refs/op` itself), drawn
    /// with the `●` graph glyph. Set once at extraction time so it survives
    /// `--reverse` reordering the display.
    is_head: bool,
    /// The changes relative to the parent snapshot, or `None` for the initial
    /// snapshot, which has no parent to compare against.
    changed: Option<Changed>,
    message: Vec<u8>,
}

/// What one snapshot changed, with ref targets already resolved for output.
struct Changed {
    /// Each changed ref, ordered by name.
    refs: Vec<RefLine>,
    /// The changed metadata files, in capture order.
    files: Vec<&'static str>,
}

impl Changed {
    /// The names of the changed parts, in capture order.
    fn names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if !self.refs.is_empty() {
            names.push("refs");
        }
        names.extend(self.files.iter().copied());
        names
    }
}

/// One changed ref, ready to render.
struct RefLine {
    name: String,
    transition: Transition,
}

/// A changed ref's targets, as rendered strings.
enum Transition {
    Created(RefTarget),
    Deleted(RefTarget),
    Updated { old: RefTarget, new: RefTarget },
}

impl Transition {
    /// The abbreviated target before the change, or a placeholder for a ref
    /// that did not exist yet.
    fn before(&self) -> &str {
        match self {
            Transition::Created(_) => "(new)",
            Transition::Deleted(old) | Transition::Updated { old, .. } => &old.short,
        }
    }

    /// The abbreviated target after the change, or a placeholder for a ref
    /// that no longer exists.
    fn after(&self) -> &str {
        match self {
            Transition::Deleted(_) => "(deleted)",
            Transition::Created(new) | Transition::Updated { new, .. } => &new.short,
        }
    }

    /// The targets as JSON values: the full ref-file contents on each side,
    /// and `null` for the side where the ref does not exist.
    fn json(&self) -> (Option<&str>, Option<&str>) {
        match self {
            Transition::Created(new) => (None, Some(&new.full)),
            Transition::Deleted(old) => (Some(&old.full), None),
            Transition::Updated { old, new } => (Some(&old.full), Some(&new.full)),
        }
    }
}

/// A ref target rendered both for machines and for display.
struct RefTarget {
    /// Git's loose-ref file contents without the trailing newline: an object
    /// ID, or `ref: ` followed by a symbolic target.
    full: String,
    /// The unambiguously abbreviated object ID, or the symbolic target name.
    short: String,
}

impl RefTarget {
    fn new(repo: &gix::Repository, target: &Target) -> Self {
        match target {
            Target::Object(id) => Self {
                full: id.to_string(),
                short: id.attach(repo).shorten_or_id().to_string(),
            },
            Target::Symbolic(name) => {
                let name = name.as_bstr().to_string();
                Self {
                    full: format!("ref: {name}"),
                    short: name,
                }
            }
        }
    }
}

impl Entry {
    /// The message's first line, as raw bytes.
    fn summary(&self) -> &[u8] {
        self.message
            .split(|&byte| byte == b'\n')
            .next()
            .unwrap_or(&[])
    }

    /// The message body without the operation's invoking-command trailer.
    fn body(&self) -> &[u8] {
        let message = self.message.strip_suffix(b"\n").unwrap_or(&self.message);
        self.invoked_by()
            .and_then(|_| {
                message
                    .windows(2)
                    .rposition(|window| window == b"\n\n")
                    .map(|separator| &message[..separator])
            })
            .unwrap_or(message)
    }

    /// The invoking command stored in the final `Invoked-by` paragraph.
    fn invoked_by(&self) -> Option<&[u8]> {
        let message = self.message.strip_suffix(b"\n").unwrap_or(&self.message);
        let separator = message.windows(2).rposition(|window| window == b"\n\n")?;
        let trailer = &message[separator + 2..];
        let value = trailer.strip_prefix(b"Invoked-by: ")?;
        (!value.is_empty() && !value.contains(&b'\n')).then_some(value)
    }
}

/// The output format selected by `git op log`'s flags.
#[derive(Clone, Copy)]
enum Format {
    Default,
    Oneline,
    Json,
}

/// Run `git op log` with the given typed options.
pub(crate) fn run(
    max_count: Option<usize>,
    reverse: bool,
    verbose: bool,
    no_pager: bool,
    oneline: bool,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let format = if json {
        Format::Json
    } else if oneline {
        Format::Oneline
    } else {
        Format::Default
    };

    let repo = open_repository()?;
    let Some(tip) = repo.try_find_reference(git_op::OP_REF)? else {
        let mut out = io::stdout().lock();
        if !matches!(format, Format::Json) {
            ignore_broken_pipe(writeln!(out, "No operation snapshots recorded."))?;
        }
        return Ok(());
    };

    let mut entries = extract_entries(&repo, tip.id().detach(), max_count)?;
    if reverse {
        entries.reverse();
    }

    if !no_pager && io::stdout().is_terminal() {
        render_paged(&entries, format, verbose)?;
    } else {
        render_unpaged(&entries, format, verbose)?;
    }
    Ok(())
}

fn render_unpaged(
    entries: &[Entry],
    format: Format,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    match format {
        Format::Json => {
            let mut out = io::stdout().lock();
            ignore_broken_pipe(render(&mut out, entries, format, verbose))?;
        }
        Format::Default | Format::Oneline => {
            let mut out = AutoStream::auto(io::stdout().lock());
            ignore_broken_pipe(render(&mut out, entries, format, verbose))?;
        }
    }
    Ok(())
}

fn render_paged(
    entries: &[Entry],
    format: Format,
    verbose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let Some(mut pager) = start_pager()? else {
        return render_unpaged(entries, format, verbose);
    };
    let color_choice = AutoStream::choice(&io::stdout());
    let result = match format {
        Format::Json => {
            let mut stdin = pager.stdin.take().ok_or_else(|| {
                io::Error::other("configured pager did not provide standard input")
            })?;
            ignore_broken_pipe(render(&mut stdin, entries, format, verbose))
        }
        Format::Default | Format::Oneline => {
            let stdin = pager.stdin.take().ok_or_else(|| {
                io::Error::other("configured pager did not provide standard input")
            })?;
            let mut out = AutoStream::new(Box::new(stdin) as Box<dyn Write>, color_choice);
            ignore_broken_pipe(render(&mut out, entries, format, verbose))
        }
    };
    let status = pager.wait()?;
    result?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!("pager exited with {status}")).into())
    }
}

fn start_pager() -> Result<Option<Child>, Box<dyn std::error::Error>> {
    if std::env::var_os("GIT_PAGER").is_some_and(|pager| pager.is_empty()) {
        return Ok(None);
    }
    let mut pager_command = Command::new("git");
    pager_command.args(["var", "GIT_PAGER"]);
    if let Some(value) = std::env::var_os("GIT_PAGER") {
        pager_command.env("GIT_PAGER", value);
    }
    let pager = pager_command.output()?;
    if !pager.status.success() {
        return Err(io::Error::other("unable to determine Git's configured pager").into());
    }
    let command = String::from_utf8(pager.stdout)?.trim().to_owned();
    if command.is_empty() {
        return Ok(None);
    }
    let mut process = Command::new("sh");
    process
        .args(["-c", &command])
        .env("GIT_PAGER", "")
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if std::env::var_os("LESS").is_none() {
        process.env("LESS", "FRX");
    }
    Ok(Some(process.spawn()?))
}

/// Walk `refs/op` by first parent from `tip`, extracting at most `max_count`
/// entries (newest first).
fn extract_entries(
    repo: &gix::Repository,
    tip: gix::ObjectId,
    max_count: Option<usize>,
) -> Result<Vec<Entry>, Box<dyn std::error::Error>> {
    let mut entries = Vec::new();
    let mut current = Some(tip);
    while let Some(id) = current {
        if max_count.is_some_and(|max| entries.len() >= max) {
            break;
        }
        let commit = repo.find_commit(id)?;
        let time = commit.committer()?.time()?;
        let changed = match git_op::changes(repo, id)? {
            Some(changes) => Some(Changed {
                refs: if changes.r#refs {
                    ref_lines(repo, id)?
                } else {
                    Vec::new()
                },
                files: changes.file_names(),
            }),
            None => None,
        };
        current = commit.parent_ids().next().map(|parent| parent.detach());
        entries.push(Entry {
            id: id.to_string(),
            abbreviated_id: commit.id().shorten_or_id().to_string(),
            time,
            is_head: entries.is_empty(),
            changed,
            message: commit.message_raw_sloppy().to_vec(),
        });
    }
    Ok(entries)
}

/// Resolve the refs one snapshot changed into renderable lines.
fn ref_lines(
    repo: &gix::Repository,
    commit: gix::ObjectId,
) -> Result<Vec<RefLine>, Box<dyn std::error::Error>> {
    Ok(git_op::ref_changes(repo, commit)?
        .unwrap_or_default()
        .into_iter()
        .map(|change| RefLine {
            name: change.name,
            transition: match change.kind {
                git_op::RefChangeKind::Created(new) => {
                    Transition::Created(RefTarget::new(repo, &new))
                }
                git_op::RefChangeKind::Deleted(old) => {
                    Transition::Deleted(RefTarget::new(repo, &old))
                }
                git_op::RefChangeKind::Updated { old, new } => Transition::Updated {
                    old: RefTarget::new(repo, &old),
                    new: RefTarget::new(repo, &new),
                },
            },
        })
        .collect())
}

/// Treat a broken pipe (for example `git op log | head`) as a normal exit.
fn ignore_broken_pipe(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

fn render(
    out: &mut impl Write,
    entries: &[Entry],
    format: Format,
    verbose: bool,
) -> io::Result<()> {
    match format {
        Format::Default => render_default(out, entries, verbose),
        Format::Oneline => render_oneline(out, entries),
        Format::Json => render_json(out, entries),
    }
}

/// Write `value` wrapped in `style`'s start and reset escape sequences.
///
/// Callers always emit these unconditionally; the surrounding [`AutoStream`]
/// strips them back out when the destination cannot or should not show
/// color, so this stays independent of any TTY detection.
fn write_styled(out: &mut impl Write, style: Style, value: impl fmt::Display) -> io::Result<()> {
    write!(out, "{style}{value}{style:#}")
}

/// Render `entries` as a `jj`-style graph: a `●`/`○` glyph column connected
/// by `│`, with each snapshot's date, changed refs, changed metadata files,
/// and message body to its right.
fn render_default(out: &mut impl Write, entries: &[Entry], verbose: bool) -> io::Result<()> {
    for (index, entry) in entries.iter().enumerate() {
        let is_last = index + 1 == entries.len();
        let (glyph, glyph_style) = if entry.is_head {
            ("●", HEAD_GLYPH_STYLE)
        } else {
            ("○", DIM_STYLE)
        };
        // No node follows the last entry, so its continuation lines get a
        // blank column instead of a `│` connector.
        let connector = if is_last { ' ' } else { '│' };

        write_styled(out, glyph_style, glyph)?;
        write!(out, "  ")?;
        write_styled(out, ID_STYLE, &entry.abbreviated_id)?;
        write!(out, "  ")?;
        write_styled(
            out,
            DIM_STYLE,
            entry.time.format_or_unix(gix::date::time::format::ISO8601),
        )?;
        writeln!(out)?;

        for line in entry.body().split(|&byte| byte == b'\n') {
            write_styled(out, DIM_STYLE, connector)?;
            writeln!(out, "  {}", String::from_utf8_lossy(line))?;
        }

        if let Some(changed) = &entry.changed {
            render_ref_lines(out, &changed.refs, connector, verbose)?;
            if !changed.files.is_empty() {
                write_styled(out, DIM_STYLE, connector)?;
                write!(out, "  ")?;
                write_styled(out, DIM_STYLE, "Changed:")?;
                write!(out, " ")?;
                for (index, name) in changed.files.iter().enumerate() {
                    if index > 0 {
                        write!(out, ", ")?;
                    }
                    write_styled(out, CHANGED_STYLE, *name)?;
                }
                writeln!(out)?;
            }
        }

        if let Some(invoked_by) = entry.invoked_by() {
            write_styled(out, DIM_STYLE, connector)?;
            writeln!(out)?;
            write_styled(out, DIM_STYLE, connector)?;
            write!(out, "  ")?;
            write_styled(out, DIM_STYLE, "Invoked by:")?;
            write!(out, " ")?;
            write_styled(out, CHANGED_STYLE, String::from_utf8_lossy(invoked_by))?;
            writeln!(out)?;
        }

        if !is_last {
            write_styled(out, DIM_STYLE, connector)?;
            writeln!(out)?;
        }
    }
    Ok(())
}

/// Render one aligned `name  old → new` line per changed ref, truncating the
/// list unless `verbose` is set.
fn render_ref_lines(
    out: &mut impl Write,
    refs: &[RefLine],
    connector: char,
    verbose: bool,
) -> io::Result<()> {
    let shown = if verbose {
        refs
    } else {
        &refs[..refs.len().min(MAX_REF_LINES)]
    };
    let width = |value: &str| value.chars().count();
    let name_width = shown
        .iter()
        .map(|line| width(&line.name))
        .max()
        .unwrap_or(0);
    let old_width = shown
        .iter()
        .map(|line| width(line.transition.before()))
        .max()
        .unwrap_or(0);

    for line in shown {
        write_styled(out, DIM_STYLE, connector)?;
        write!(out, "  ")?;
        write_styled(out, CHANGED_STYLE, format!("{:name_width$}", line.name))?;
        write!(out, "  ")?;
        write_target(out, line.transition.before(), old_width)?;
        write!(out, " ")?;
        write_styled(out, DIM_STYLE, "→")?;
        write!(out, " ")?;
        write_target(out, line.transition.after(), 0)?;
        writeln!(out)?;
    }

    let remaining = refs.len() - shown.len();
    if remaining > 0 {
        write_styled(out, DIM_STYLE, connector)?;
        write!(out, "  ")?;
        write_styled(out, DIM_STYLE, format!("… and {remaining} more"))?;
        writeln!(out)?;
    }
    Ok(())
}

/// Write one ref target padded to `width`, dimming the `(new)` and `(deleted)`
/// placeholders that stand in for a missing side.
fn write_target(out: &mut impl Write, target: &str, width: usize) -> io::Result<()> {
    let style = if target.starts_with('(') {
        DIM_STYLE
    } else {
        ID_STYLE
    };
    write_styled(out, style, format!("{target:width$}"))
}

fn render_oneline(out: &mut impl Write, entries: &[Entry]) -> io::Result<()> {
    for entry in entries {
        write_styled(out, ID_STYLE, &entry.abbreviated_id)?;
        writeln!(out, " {}", String::from_utf8_lossy(entry.summary()))?;
    }
    Ok(())
}

fn render_json(out: &mut impl Write, entries: &[Entry]) -> io::Result<()> {
    for entry in entries {
        write!(
            out,
            "{{\"id\":\"{}\",\"time\":\"{}\",\"changed\":",
            entry.id,
            entry
                .time
                .format_or_unix(gix::date::time::format::ISO8601_STRICT),
        )?;
        match &entry.changed {
            Some(changed) => {
                write!(out, "[")?;
                for (index, name) in changed.names().iter().enumerate() {
                    if index > 0 {
                        write!(out, ",")?;
                    }
                    write!(out, "\"{name}\"")?;
                }
                write!(out, "]")?;
            }
            None => write!(out, "null")?,
        }
        write!(out, ",\"refs\":")?;
        match &entry.changed {
            Some(changed) => render_json_refs(out, &changed.refs)?,
            None => write!(out, "null")?,
        }
        write!(out, ",\"summary\":\"")?;
        write_json_escaped(out, entry.summary())?;
        writeln!(out, "\"}}")?;
    }
    Ok(())
}

/// Write the changed refs as a JSON array of `name`, `old`, and `new`, where
/// each target is Git's loose-ref file format and a missing side is `null`.
fn render_json_refs(out: &mut impl Write, refs: &[RefLine]) -> io::Result<()> {
    write!(out, "[")?;
    for (index, line) in refs.iter().enumerate() {
        if index > 0 {
            write!(out, ",")?;
        }
        let (old, new) = line.transition.json();
        write!(out, "{{\"name\":\"")?;
        write_json_escaped(out, line.name.as_bytes())?;
        write!(out, "\",\"old\":")?;
        write_json_target(out, old)?;
        write!(out, ",\"new\":")?;
        write_json_target(out, new)?;
        write!(out, "}}")?;
    }
    write!(out, "]")
}

/// Write one side of a ref transition as a JSON string, or `null` when the ref
/// does not exist on that side.
fn write_json_target(out: &mut impl Write, target: Option<&str>) -> io::Result<()> {
    match target {
        Some(target) => {
            write!(out, "\"")?;
            write_json_escaped(out, target.as_bytes())?;
            write!(out, "\"")
        }
        None => write!(out, "null"),
    }
}

/// Write `bytes` as the escaped contents of a JSON string, excluding the
/// surrounding quotes.
///
/// `bytes` need not be valid UTF-8: invalid sequences are replaced following
/// [`String::from_utf8_lossy`], since a JSON string is Unicode text and
/// commit messages carry no encoding guarantee.
fn write_json_escaped(out: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    for ch in String::from_utf8_lossy(bytes).chars() {
        match ch {
            '"' => write!(out, "\\\"")?,
            '\\' => write!(out, "\\\\")?,
            '\n' => write!(out, "\\n")?,
            '\t' => write!(out, "\\t")?,
            '\r' => write!(out, "\\r")?,
            control if (control as u32) < 0x20 => write!(out, "\\u{:04x}", control as u32)?,
            other => write!(out, "{other}")?,
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ref target whose short form is `id` and whose full form is `id`
    /// zero-padded to an object ID.
    fn target(id: &str) -> RefTarget {
        RefTarget {
            full: format!("{id}{}", "0".repeat(40 - id.len())),
            short: id.to_owned(),
        }
    }

    fn ref_line(name: &str, transition: Transition) -> RefLine {
        RefLine {
            name: name.to_owned(),
            transition,
        }
    }

    fn entry(
        id: &str,
        abbreviated_id: &str,
        is_head: bool,
        changed: Option<Changed>,
        message: &str,
    ) -> Entry {
        Entry {
            id: id.to_owned(),
            abbreviated_id: abbreviated_id.to_owned(),
            time: gix::date::Time {
                seconds: 1_787_421_791,
                offset: -4 * 3600,
            },
            is_head,
            changed,
            message: message.as_bytes().to_vec(),
        }
    }

    /// Render through an [`AutoStream`] forced to a fixed [`anstream::ColorChoice`],
    /// then hand back the plain bytes it wrote, matching how `run` picks a
    /// stream for the default and oneline formats.
    fn render_with_color(
        entries: &[Entry],
        format: Format,
        choice: anstream::ColorChoice,
    ) -> String {
        let mut out = AutoStream::new(Vec::new(), choice);
        render(&mut out, entries, format, false).expect("render");
        String::from_utf8(out.into_inner()).expect("output is UTF-8")
    }

    #[test]
    fn default_format_lists_changed_refs_and_omits_changes_for_the_initial_snapshot() {
        let entries = vec![
            entry(
                "3a7f2c1d9e4b000000000000000000000000000000",
                "3a7f2c1d9e4b",
                true,
                Some(Changed {
                    refs: vec![
                        ref_line(
                            "refs/heads/main",
                            Transition::Updated {
                                old: target("1e2ea16"),
                                new: target("dc80af7"),
                            },
                        ),
                        ref_line("refs/heads/topic", Transition::Created(target("8087efa"))),
                        ref_line("refs/tags/v0.1.0", Transition::Deleted(target("047ae16"))),
                    ],
                    files: vec!["config"],
                }),
                "op: update refs and config\n",
            ),
            entry(
                "9b1e0042ac310000000000000000000000000000000",
                "9b1e0042ac31",
                false,
                None,
                "op: capture initial repository state\n",
            ),
        ];

        let rendered = render_with_color(&entries, Format::Default, anstream::ColorChoice::Never);

        assert_eq!(
            rendered,
            "●  3a7f2c1d9e4b  2026-08-22 14:03:11 -0400\n\
             │  op: update refs and config\n\
             │  refs/heads/main   1e2ea16 → dc80af7\n\
             │  refs/heads/topic  (new)   → 8087efa\n\
             │  refs/tags/v0.1.0  047ae16 → (deleted)\n\
             │  Changed: config\n\
             │\n\
             ○  9b1e0042ac31  2026-08-22 14:03:11 -0400\n\
             \x20\x20\x20op: capture initial repository state\n"
        );
    }

    #[test]
    fn entry_splits_invoked_by_trailer_from_body() {
        let entry = entry(
            "id",
            "id",
            true,
            None,
            "op: update refs\n\nInvoked-by: git -C repo commit\n",
        );

        assert_eq!(entry.body(), b"op: update refs");
        assert_eq!(entry.invoked_by(), Some(&b"git -C repo commit"[..]));
    }

    #[test]
    fn entry_keeps_messages_without_invoked_by_trailer() {
        let entry = entry("id", "id", true, None, "summary\n\nbody\n");

        assert_eq!(entry.body(), b"summary\n\nbody");
        assert_eq!(entry.invoked_by(), None);
    }

    #[test]
    fn default_format_renders_invoked_by_after_refs() {
        let entries = vec![entry(
            "3a7f2c1d9e4b",
            "3a7f2c1",
            true,
            Some(Changed {
                refs: vec![ref_line(
                    "refs/heads/main",
                    Transition::Updated {
                        old: target("1e2ea16"),
                        new: target("dc80af7"),
                    },
                )],
                files: vec!["config"],
            }),
            "op: update refs\n\nInvoked-by: git commit\n",
        )];

        let rendered = render_with_color(&entries, Format::Default, anstream::ColorChoice::Never);

        assert!(rendered.starts_with("●  3a7f2c1  2026-08-22 14:03:11 -0400\n"));
        assert!(rendered.contains("   op: update refs\n"));
        assert!(rendered.contains("   refs/heads/main  1e2ea16 → dc80af7\n"));
        assert!(rendered.contains("   Changed: config\n"));
        assert!(rendered.ends_with(" \n   Invoked by: git commit\n"));
        assert!(!rendered.contains("Invoked-by:"));
    }

    #[test]
    fn default_format_truncates_long_ref_lists_unless_verbose() {
        let refs = (0..MAX_REF_LINES + 2)
            .map(|index| {
                ref_line(
                    &format!("refs/heads/branch-{index}"),
                    Transition::Created(target("8087efa")),
                )
            })
            .collect();
        let entries = vec![entry(
            "3a7f2c1d9e4b",
            "3a7f2c1",
            true,
            Some(Changed {
                refs,
                files: Vec::new(),
            }),
            "op: update refs\n",
        )];

        let truncated = render_with_color(&entries, Format::Default, anstream::ColorChoice::Never);
        assert_eq!(
            truncated.matches("refs/heads/branch-").count(),
            MAX_REF_LINES
        );
        assert!(truncated.contains("… and 2 more"));

        let mut out = AutoStream::new(Vec::new(), anstream::ColorChoice::Never);
        render(&mut out, &entries, Format::Default, true).expect("render verbose");
        let verbose = String::from_utf8(out.into_inner()).expect("output is UTF-8");
        assert_eq!(
            verbose.matches("refs/heads/branch-").count(),
            MAX_REF_LINES + 2
        );
        assert!(!verbose.contains("more"));
    }

    #[test]
    fn default_format_styles_the_head_glyph_and_id_when_color_is_forced() {
        let entries = vec![entry(
            "3a7f2c1d9e4b",
            "3a7f2c1",
            true,
            None,
            "op: capture initial repository state\n",
        )];

        let rendered =
            render_with_color(&entries, Format::Default, anstream::ColorChoice::AlwaysAnsi);

        assert!(
            rendered.contains("\x1b["),
            "expected ANSI escape codes in forced-color output, got {rendered:?}"
        );
        assert!(rendered.contains('●'));
    }

    #[test]
    fn oneline_format_is_id_and_summary_only() {
        let entries = vec![entry(
            "3a7f2c1d9e4b",
            "3a7f2c1",
            true,
            Some(Changed {
                refs: vec![ref_line(
                    "refs/heads/main",
                    Transition::Updated {
                        old: target("1e2ea16"),
                        new: target("dc80af7"),
                    },
                )],
                files: vec!["config"],
            }),
            "op: update refs and config\n\nlonger body ignored",
        )];

        let rendered = render_with_color(&entries, Format::Oneline, anstream::ColorChoice::Never);
        assert_eq!(rendered, "3a7f2c1 op: update refs and config\n");
    }

    #[test]
    fn json_format_is_one_object_per_line() {
        let entries = vec![
            entry(
                "3a7f2c1d9e4b",
                "3a7f2c1",
                true,
                Some(Changed {
                    refs: vec![
                        ref_line(
                            "refs/heads/main",
                            Transition::Updated {
                                old: target("1e2ea16"),
                                new: target("dc80af7"),
                            },
                        ),
                        ref_line(
                            "refs/heads/alias",
                            Transition::Created(RefTarget {
                                full: "ref: refs/heads/main".to_owned(),
                                short: "refs/heads/main".to_owned(),
                            }),
                        ),
                    ],
                    files: vec!["config"],
                }),
                "op: update refs and config\n",
            ),
            entry(
                "9b1e0042ac31",
                "9b1e004",
                false,
                None,
                "op: capture initial repository state\n",
            ),
        ];

        let mut out = Vec::new();
        render(&mut out, &entries, Format::Json, false).expect("render json format");
        let rendered = String::from_utf8(out).expect("output is UTF-8");
        let mut lines = rendered.lines();

        assert_eq!(
            lines.next().expect("first line"),
            "{\"id\":\"3a7f2c1d9e4b\",\"time\":\"2026-08-22T14:03:11-04:00\",\"changed\":[\"refs\",\"config\"],\
             \"refs\":[\
             {\"name\":\"refs/heads/main\",\"old\":\"1e2ea16000000000000000000000000000000000\",\"new\":\"dc80af7000000000000000000000000000000000\"},\
             {\"name\":\"refs/heads/alias\",\"old\":null,\"new\":\"ref: refs/heads/main\"}\
             ],\"summary\":\"op: update refs and config\"}"
        );
        assert_eq!(
            lines.next().expect("second line"),
            "{\"id\":\"9b1e0042ac31\",\"time\":\"2026-08-22T14:03:11-04:00\",\"changed\":null,\
             \"refs\":null,\"summary\":\"op: capture initial repository state\"}"
        );
        assert_eq!(lines.next(), None);
    }

    #[test]
    fn json_escaper_handles_quotes_backslashes_and_control_characters() {
        let mut out = Vec::new();
        write_json_escaped(&mut out, b"say \"hi\"\\ then\nindent\ttab\x01done")
            .expect("escape message");
        assert_eq!(
            String::from_utf8(out).expect("output is UTF-8"),
            r#"say \"hi\"\\ then\nindent\ttab\u0001done"#
        );
    }

    #[test]
    fn json_escaper_replaces_invalid_utf8() {
        let mut out = Vec::new();
        write_json_escaped(&mut out, b"broken \xFF byte").expect("escape message");
        assert_eq!(
            String::from_utf8(out).expect("output is UTF-8"),
            "broken \u{FFFD} byte"
        );
    }

    #[test]
    fn extract_entries_respects_max_count() {
        let root = std::env::temp_dir().join(format!(
            "git-op-log-max-count-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock is after the Unix epoch")
                .as_nanos()
        ));
        std::fs::create_dir(&root).expect("create temporary repository directory");
        let repo = gix::init(&root).expect("initialize repository");
        git_op::append(&repo, "first").expect("append first snapshot");
        std::fs::write(repo.common_dir().join("description"), b"second\n")
            .expect("write description");
        let tip = git_op::append(&repo, "second").expect("append second snapshot");

        let all = extract_entries(&repo, tip, None).expect("extract all entries");
        assert_eq!(all.len(), 2);
        assert!(all[1].changed.is_none());

        let limited = extract_entries(&repo, tip, Some(1)).expect("extract limited entries");
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].message, b"second\n");

        std::fs::remove_dir_all(root).expect("remove temporary repository");
    }

    #[test]
    fn reverse_flips_extracted_order() {
        let mut entries = [
            entry("new", "new", true, None, "newest"),
            entry("old", "old", false, None, "oldest"),
        ];
        entries.reverse();
        assert_eq!(entries[0].message, b"oldest");
        assert_eq!(entries[1].message, b"newest");
    }
}
