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
        action: operation_action(message.as_bytes()),
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
fn render_with_color(entries: &[Entry], format: Format, choice: anstream::ColorChoice) -> String {
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
                files: vec![git_op::MetadataFile::Config],
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
            files: vec![git_op::MetadataFile::Config],
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

    let rendered = render_with_color(&entries, Format::Default, anstream::ColorChoice::AlwaysAnsi);

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
            files: vec![git_op::MetadataFile::Config],
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
                files: vec![git_op::MetadataFile::Config],
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
    std::fs::write(repo.common_dir().join("description"), b"second\n").expect("write description");
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
