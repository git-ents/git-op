//! `git op log`: render the operation log directly, without shelling out to
//! `git log`.
//!
//! Walking the log ourselves means every accepted argument is a typed `clap`
//! field, so `clap` rejects anything else (`--all`, revision ranges, and so
//! on) before the repository is even opened.

use std::io::{self, Write};

use crate::exe::open_repository;

/// One rendered operation-log entry, extracted from a snapshot commit.
///
/// Extraction needs repository access (for example to abbreviate the id
/// unambiguously); rendering does not, so it is kept in pure functions that
/// take entries already built by [`extract_entries`].
struct Entry {
    id: String,
    abbreviated_id: String,
    time: gix::date::Time,
    /// The parts changed relative to the parent snapshot, or `None` for the
    /// initial snapshot, which has no parent to compare against.
    changed: Option<Vec<&'static str>>,
    message: Vec<u8>,
}

impl Entry {
    /// The message's first line, as raw bytes.
    fn summary(&self) -> &[u8] {
        self.message
            .split(|&byte| byte == b'\n')
            .next()
            .unwrap_or(&[])
    }

    /// The message body with at most one trailing newline removed, matching
    /// how Git stores commit messages.
    fn body(&self) -> &[u8] {
        self.message.strip_suffix(b"\n").unwrap_or(&self.message)
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

    let mut out = io::stdout().lock();
    ignore_broken_pipe(render(&mut out, &entries, format))?;
    Ok(())
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
        let changed = git_op::changes(repo, id)?.map(|changes| changes.names());
        current = commit.parent_ids().next().map(|parent| parent.detach());
        entries.push(Entry {
            id: id.to_string(),
            abbreviated_id: commit.id().shorten_or_id().to_string(),
            time,
            changed,
            message: commit.message_raw_sloppy().to_vec(),
        });
    }
    Ok(entries)
}

/// Treat a broken pipe (for example `git op log | head`) as a normal exit.
fn ignore_broken_pipe(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

fn render(out: &mut impl Write, entries: &[Entry], format: Format) -> io::Result<()> {
    match format {
        Format::Default => render_default(out, entries),
        Format::Oneline => render_oneline(out, entries),
        Format::Json => render_json(out, entries),
    }
}

fn render_default(out: &mut impl Write, entries: &[Entry]) -> io::Result<()> {
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 {
            writeln!(out)?;
        }
        writeln!(out, "operation {}", entry.abbreviated_id)?;
        writeln!(
            out,
            "Date:    {}",
            entry.time.format_or_unix(gix::date::time::format::ISO8601)
        )?;
        if let Some(changed) = &entry.changed {
            writeln!(out, "Changed: {}", changed.join(", "))?;
        }
        writeln!(out)?;
        for line in entry.body().split(|&byte| byte == b'\n') {
            writeln!(out, "    {}", String::from_utf8_lossy(line))?;
        }
    }
    Ok(())
}

fn render_oneline(out: &mut impl Write, entries: &[Entry]) -> io::Result<()> {
    for entry in entries {
        writeln!(
            out,
            "{} {}",
            entry.abbreviated_id,
            String::from_utf8_lossy(entry.summary())
        )?;
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
            Some(names) => {
                write!(out, "[")?;
                for (index, name) in names.iter().enumerate() {
                    if index > 0 {
                        write!(out, ",")?;
                    }
                    write!(out, "\"{name}\"")?;
                }
                write!(out, "]")?;
            }
            None => write!(out, "null")?,
        }
        write!(out, ",\"summary\":\"")?;
        write_json_escaped(out, entry.summary())?;
        writeln!(out, "\"}}")?;
    }
    Ok(())
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

    fn entry(
        id: &str,
        abbreviated_id: &str,
        changed: Option<Vec<&'static str>>,
        message: &str,
    ) -> Entry {
        Entry {
            id: id.to_owned(),
            abbreviated_id: abbreviated_id.to_owned(),
            time: gix::date::Time {
                seconds: 1_787_421_791,
                offset: -4 * 3600,
            },
            changed,
            message: message.as_bytes().to_vec(),
        }
    }

    #[test]
    fn default_format_omits_changed_for_initial_snapshot() {
        let entries = vec![
            entry(
                "3a7f2c1d9e4b000000000000000000000000000000",
                "3a7f2c1d9e4b",
                Some(vec!["refs", "config"]),
                "op: update refs and config\n",
            ),
            entry(
                "9b1e0042ac310000000000000000000000000000000",
                "9b1e0042ac31",
                None,
                "op: capture initial repository state\n",
            ),
        ];

        let mut out = Vec::new();
        render(&mut out, &entries, Format::Default).expect("render default format");
        let rendered = String::from_utf8(out).expect("output is UTF-8");

        assert_eq!(
            rendered,
            "operation 3a7f2c1d9e4b\n\
             Date:    2026-08-22 14:03:11 -0400\n\
             Changed: refs, config\n\
             \n\
             \x20\x20\x20\x20op: update refs and config\n\
             \n\
             operation 9b1e0042ac31\n\
             Date:    2026-08-22 14:03:11 -0400\n\
             \n\
             \x20\x20\x20\x20op: capture initial repository state\n"
        );
    }

    #[test]
    fn oneline_format_is_id_and_summary_only() {
        let entries = vec![entry(
            "3a7f2c1d9e4b",
            "3a7f2c1",
            Some(vec!["refs", "config"]),
            "op: update refs and config\n\nlonger body ignored",
        )];

        let mut out = Vec::new();
        render(&mut out, &entries, Format::Oneline).expect("render oneline format");
        assert_eq!(
            String::from_utf8(out).expect("output is UTF-8"),
            "3a7f2c1 op: update refs and config\n"
        );
    }

    #[test]
    fn json_format_is_one_object_per_line() {
        let entries = vec![
            entry(
                "3a7f2c1d9e4b",
                "3a7f2c1",
                Some(vec!["refs", "config"]),
                "op: update refs and config\n",
            ),
            entry(
                "9b1e0042ac31",
                "9b1e004",
                None,
                "op: capture initial repository state\n",
            ),
        ];

        let mut out = Vec::new();
        render(&mut out, &entries, Format::Json).expect("render json format");
        let rendered = String::from_utf8(out).expect("output is UTF-8");
        let mut lines = rendered.lines();

        assert_eq!(
            lines.next().expect("first line"),
            "{\"id\":\"3a7f2c1d9e4b\",\"time\":\"2026-08-22T14:03:11-04:00\",\"changed\":[\"refs\",\"config\"],\"summary\":\"op: update refs and config\"}"
        );
        assert_eq!(
            lines.next().expect("second line"),
            "{\"id\":\"9b1e0042ac31\",\"time\":\"2026-08-22T14:03:11-04:00\",\"changed\":null,\"summary\":\"op: capture initial repository state\"}"
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
            entry("new", "new", None, "newest"),
            entry("old", "old", None, "oldest"),
        ];
        entries.reverse();
        assert_eq!(entries[0].message, b"oldest");
        assert_eq!(entries[1].message, b"newest");
    }
}
