//! Behavioral checks at the terminal, matching, and launch-policy boundaries.
use super::*;
use chrono::Utc;
use ratatui::{backend::TestBackend, Terminal as TestTerminal};
use std::sync::Arc;

fn options(mode: OpenMode) -> Options {
    Options {
        query: None,
        tool: None,
        profile: None,
        here: false,
        include_non_conversations: false,
        mode,
        to_profile: None,
        extra_args: Vec::new(),
    }
}

pub(super) fn conversation() -> Conversation {
    Conversation {
        tool: "codex".into(),
        profile: "nit".into(),
        id: "opaque-session-id".into(),
        native_name: Some("Fix login timeout".into()),
        title: "Fix login timeout".into(),
        first_prompt: Some("The callback hangs".into()),
        cwd: PathBuf::from("/projects/browseros"),
        started_at: None,
        updated_at: Utc::now(),
        enabled: true,
        bypass: false,
        transcript_path: PathBuf::from("/private/rollout.jsonl"),
    }
}

fn app() -> App {
    let conversation = Arc::new(conversation());
    let key = crate::conversations::ConversationKey::from(conversation.as_ref()).encode();
    let mut app = App::new(
        options(OpenMode::Fork),
        PathBuf::from("/projects/browseros"),
    );
    app.generation = 1;
    app.apply(Snapshot {
        generation: 1,
        loaded: true,
        indexed: 1,
        total: 1,
        selected: Some(key.clone()),
        rows: vec![search::Row {
            key: key.clone(),
            conversation,
            highlights: vec![4, 5, 6, 7, 8],
        }],
        preview: worker::Preview {
            key: Some(key),
            excerpts: vec![search::Excerpt {
                label: "You".into(),
                text: "The login callback hangs.\nAfter switching accounts.".into(),
                highlights: Vec::new(),
            }],
            launch: launch::Launch {
                model: Some("gpt-6-astra".into()),
                effort: Some("xhigh".into()),
                arguments: String::new(),
                destinations: vec!["eng".into(), "nik".into()],
            },
            ..worker::Preview::default()
        },
        ..Snapshot::default()
    });
    app
}

#[test]
fn enter_respects_invocation_and_explicit_keys_always_win() {
    for mode in [OpenMode::Fork, OpenMode::Resume] {
        let mut app = App::new(options(mode), PathBuf::from("/project"));
        assert!(
            matches!(app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Effect::Accept(value) if value == mode)
        );
        assert!(matches!(
            app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            Effect::Accept(OpenMode::Resume)
        ));
        assert!(matches!(
            app.key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            Effect::Accept(OpenMode::Fork)
        ));
    }
}

#[test]
fn stale_results_cannot_replace_selection_or_enable_launch() {
    let mut app = app();
    let old = app.snapshot.clone();
    app.edit("different query");
    app.apply(old);
    assert!(!app.ready());
    assert!(app.query.selected.is_none());
    let mut wrong_generation = app.snapshot.clone();
    wrong_generation.revision = app.query.revision;
    app.generation += 1;
    app.apply(wrong_generation);
    assert!(!app.ready());
}

#[test]
fn query_editing_preserves_unicode_and_paste_cannot_inject_controls() {
    let mut app = App::new(options(OpenMode::Fork), PathBuf::new());
    app.edit("café東京");
    app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
    app.key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    app.edit("ê\n\x1b");
    assert_eq!(app.query.text, "caféê  京");
    assert!(app.query.text.is_char_boundary(app.cursor));
    app.key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert!(
        app.query.text.contains('1'),
        "plain numbers belong to the query"
    );
}

#[test]
fn old_dialogue_matches_keep_unicode_offsets_and_paragraphs() {
    let index = search::Index::new(
        &conversation(),
        vec![
            (
                "user".into(),
                "東京 cafe\u{301} 👩‍💻\nThe unusual_old_needle is here.".into(),
            ),
            (
                "assistant".into(),
                "A recent answer without the searched term.".into(),
            ),
        ],
    );
    let search = search::Search::new("'unusual_old_needle");
    let matches = index.matching_messages(&search, &|| false);
    let (excerpts, _) = index.conversation(24, 24, matches.first().copied(), &search);
    assert_eq!(excerpts.len(), 2);
    assert_eq!(excerpts[0].label, "You");
    assert!(excerpts[0].text.contains('\n'));
    let highlighted: String = excerpts[0]
        .text
        .chars()
        .enumerate()
        .filter(|(i, _)| excerpts[0].highlights.contains(&(*i as u32)))
        .map(|(_, ch)| ch)
        .collect();
    assert_eq!(highlighted, "unusual_old_needle");
    assert!(search::Search::new("login browseros")
        .matches(&conversation(), Some(&index))
        .is_some());
}

#[test]
fn requested_launch_uses_runtime_overrides_and_does_not_guess_native_defaults() {
    let config = crate::config::Config::parse(
        r#"
[tools.codex]
command = ["codex"]
args = ["-m", "gpt-old", "-c", "model_reasoning_effort=max"]
[tools.codex.profiles.nit]
"#,
    )
    .unwrap();
    let extra = vec![
        "--model=gpt-6-astra".into(),
        "-c".into(),
        "model_reasoning_effort=\"xhigh\"".into(),
    ];
    let requested = launch::Launch::resolve(&config, &conversation(), &extra);
    assert_eq!(requested.model.as_deref(), Some("gpt-6-astra"));
    assert_eq!(requested.effort.as_deref(), Some("xhigh"));
    assert!(!requested.arguments.contains("gpt-old"));
    let native = launch::Launch::resolve(&crate::config::Config::default(), &conversation(), &[]);
    assert!(native.model.is_none());
    assert!(native
        .line(&conversation(), OpenMode::Resume, None)
        .contains("native model"));
    let command = launch::copy_command(&conversation(), OpenMode::Resume, &extra, None);
    assert!(command.starts_with("rtr resume opaque-session-id --tool codex --profile nit -- "));
}

#[test]
fn matches_include_repeated_messages_and_queries_spanning_exchanges() {
    let index = search::Index::new(
        &conversation(),
        vec![
            ("user".into(), "first_unique search_needle".into()),
            ("assistant".into(), "second_unique search_needle".into()),
        ],
    );
    let search = search::Search::new("'search_needle");
    assert_eq!(index.matching_messages(&search, &|| false), vec![1, 0]);
    let (repeated, _) = index.conversation(24, 24, Some(1), &search);
    assert_eq!(repeated.len(), 2);
    assert_eq!(repeated[0].label, "You");
    assert_eq!(repeated[1].label, "Assistant");
    let search = search::Search::new("'first_unique 'second_unique");
    let (spanning, _) = index.conversation(24, 24, Some(1), &search);
    assert_eq!(spanning.len(), 2);
    assert!(!spanning[0].highlights.is_empty());
    assert!(!spanning[1].highlights.is_empty());
}

#[test]
fn help_can_scroll_independently_of_a_short_conversation_preview() {
    let mut app = app();
    let mut terminal = TestTerminal::new(TestBackend::new(44, 24)).unwrap();
    app.key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE));
    app.key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
    assert!(app.help_scroll > 0);
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.help);
    assert_eq!(app.scroll, 0);
}

#[test]
fn wide_and_narrow_frames_keep_titles_actions_and_preview_without_raw_ids() {
    for (width, height) in [(140, 36), (80, 32), (44, 24)] {
        let mut app = app();
        let mut terminal = TestTerminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
        let buffer = terminal.backend().buffer();
        let screen = buffer
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("Fix login"), "{width}: {screen}");
        assert!(screen.contains("Enter fork"), "{width}: {screen}");
        assert!(screen.contains("Conversation"), "{width}: {screen}");
        assert!(
            !screen.contains("opaque-session-id"),
            "{width}: identity belongs in Details"
        );
        assert!(
            !screen.contains("/private/rollout"),
            "{width}: path belongs in Details"
        );
        if width >= 80 {
            assert!(screen.contains("gpt-6-astra"), "{width}: {screen}");
            assert!(screen.contains("The login callback"), "{width}: {screen}");
        }

        let rendered = buffer
            .content
            .chunks(width as usize)
            .map(|row| {
                row.iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/picker-snapshots/{width}x{height}.txt"));
        if std::env::var_os("RTR_UPDATE_PICKER_SNAPSHOTS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &rendered).unwrap();
        }
        assert_eq!(
            rendered,
            std::fs::read_to_string(path).unwrap(),
            "review the layout before updating its snapshot"
        );
    }
}

#[test]
fn ctrl_agent_filter_works_without_alt() {
    let mut app = App::new(options(OpenMode::Fork), PathBuf::new());
    app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert_eq!(app.query.filters.tool.as_deref(), Some("claude"));
    assert!(app.query.text.is_empty());
}

#[test]
fn ctrl_filters_cycle_without_editing_the_query_or_launch_mode() {
    for mut app in [App::new(options(OpenMode::Fork), PathBuf::new()), app()] {
        app.edit("café query");
        app.snapshot.profiles = vec!["eng".into(), "nit".into()];
        let cursor = app.cursor;
        for here in [true, false] {
            let revision = app.query.revision;
            app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
            assert_eq!(app.query.filters.here, here);
            assert!(app.query.revision > revision);
            assert!(app.query.selected.is_none());
        }
        for profile in [Some("eng"), Some("nit"), None] {
            app.key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL));
            assert_eq!(app.query.filters.profile.as_deref(), profile);
        }
        for tool in [Some("claude"), Some("codex"), None] {
            app.query.filters.profile = Some("eng".into());
            app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
            assert_eq!(app.query.filters.tool.as_deref(), tool);
            assert!(app.query.filters.profile.is_none());
        }
        assert_eq!(app.query.text, "café query");
        assert_eq!(app.cursor, cursor);
        assert_eq!(app.mode, OpenMode::Fork);
    }
}

#[test]
fn ctrl_preview_actions_work_without_alt() {
    let mut app = app();
    app.snapshot.preview.matches = 2;
    for index in [1, 0] {
        app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL));
        assert_eq!(app.query.match_index, index);
    }
    app.query.match_index = 1;
    for _ in 0..2 {
        app.key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL));
        assert_eq!(app.query.match_index, 0);
    }
    for visible in [false, true] {
        app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.show_preview, visible);
    }
    assert!(matches!(
        app.key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
        Effect::Refresh
    ));
    // Assert the input-to-effect boundary without writing the user's clipboard.
    assert!(matches!(
        app.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::CONTROL)),
        Effect::Copy
    ));
    assert!(app.query.text.is_empty());
    assert_eq!(app.mode, OpenMode::Fork);
}

#[test]
fn tiny_frame_and_filter_shortcuts_do_not_panic_or_change_launch_mode() {
    let mut app = app();
    let mut terminal = TestTerminal::new(TestBackend::new(20, 5)).unwrap();
    terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
    app.key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT));
    assert!(app.query.filters.here);
    app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT));
    assert_eq!(app.query.filters.tool.as_deref(), Some("claude"));
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.query.tab, Tab::Details);
    assert_eq!(app.mode, OpenMode::Fork);
}

#[test]
fn explicit_fork_destination_stays_visible_and_survives_copy_command() {
    let mut app = app();
    app.to_profile = Some("destination team".into());
    assert!(matches!(
        app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        Effect::None
    ));
    assert!(app
        .notice
        .as_deref()
        .unwrap()
        .contains("Ctrl-R is unavailable"));
    assert!(matches!(
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Effect::Accept(OpenMode::Fork)
    ));
    let line = app.snapshot.preview.launch.line(
        &conversation(),
        OpenMode::Fork,
        app.to_profile.as_deref(),
    );
    assert!(line.contains("→ destination team"));
    let command = launch::copy_command(
        &conversation(),
        OpenMode::Fork,
        &["--model".into(), "gpt-test".into()],
        app.to_profile.as_deref(),
    );
    assert!(
        command.contains("--profile nit --to-profile 'destination team' -- --model gpt-test"),
        "{command}"
    );
    let automatic = app
        .snapshot
        .preview
        .launch
        .line(&conversation(), OpenMode::Fork, None);
    assert!(automatic.contains("→ another profile"));
}

#[test]
fn conversation_preview_starts_at_bottom_scrolls_up_and_resets_on_selection() {
    let mut app = app();
    app.snapshot.preview.excerpts = vec![search::Excerpt {
        label: "You".into(),
        text: (0..80)
            .map(|i| format!("message line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
        highlights: Vec::new(),
    }];
    let mut terminal = TestTerminal::new(TestBackend::new(100, 30)).unwrap();
    let screen = |terminal: &TestTerminal<TestBackend>| {
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect::<String>()
    };
    terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
    assert!(screen(&terminal).contains("message line 79"));
    assert!(!screen(&terminal).contains("message line 00"));
    app.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
    terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
    assert!(!screen(&terminal).contains("message line 79"));
    let mut row = app.snapshot.rows[0].clone();
    row.key = "another".into();
    app.snapshot.rows.push(row);
    app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.scroll, 0);
}

#[test]
fn selection_and_preview_navigation_do_not_change_the_search_revision() {
    let mut app = app();
    let mut row = app.snapshot.rows[0].clone();
    row.key = "next".into();
    app.snapshot.rows.push(row);
    let revision = app.query.revision;
    let preview = app.snapshot.preview.excerpts[0].text.clone();
    for code in [KeyCode::Down, KeyCode::Up, KeyCode::PageDown, KeyCode::Tab] {
        app.key(KeyEvent::new(code, KeyModifiers::NONE));
        assert_eq!(app.query.revision, revision);
        assert!(app.ready());
        assert_eq!(app.snapshot.preview.excerpts[0].text, preview);
    }
    let mut terminal = TestTerminal::new(TestBackend::new(80, 32)).unwrap();
    terminal.draw(|f| view::draw(f, &mut app)).unwrap();
    let screen = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(!screen.contains("Loading preview"));
}

#[test]
fn all_fields_are_literal_and_case_insensitive() {
    let mut c = conversation();
    c.title = "Unrelated".into();
    c.cwd = PathBuf::from("/project");
    c.id = "opaque".into();
    let index = search::Index::new(
        &c,
        vec![
            ("user".into(), "a---b---c Alpha".into()),
            ("assistant".into(), "BETA suffix".into()),
        ],
    );
    assert!(search::Search::new("abc")
        .matches(&c, Some(&index))
        .is_none());
    assert!(search::Search::new("ALPHA beta")
        .matches(&c, Some(&index))
        .is_some());
    assert!(search::Search::new("alpha missing")
        .matches(&c, Some(&index))
        .is_none());
    assert!(search::Search::new("alpha !BETA")
        .matches(&c, Some(&index))
        .is_none());
    assert!(search::Search::new("^beta suffix$")
        .matches(&c, Some(&index))
        .is_some());
    assert!(search::Search::new("^alpha")
        .matches(&c, Some(&index))
        .is_none());
    c.title = "Login timeout".into();
    assert!(search::Search::new("lgtm")
        .matches(&c, Some(&index))
        .is_none());
    assert!(search::Search::new("'lgtm")
        .matches(&c, Some(&index))
        .is_none());
    assert!(search::Search::new("login !beta")
        .matches(&c, Some(&index))
        .is_none());
}

#[test]
fn help_and_initial_build_progress_snapshots_explain_literal_search_and_cached_rows() {
    for (name, width, height, help) in [
        ("80x32-help", 80, 32, true),
        ("140x36-indexing", 140, 36, false),
    ] {
        let mut app = app();
        app.help = help;
        app.help_scroll = if help { 8 } else { 0 };
        if !help {
            app.snapshot.indexed = 0;
            app.snapshot.total = 40;
            app.snapshot.build_progress = Some((12, 60));
        }
        let mut terminal = TestTerminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| view::draw(f, &mut app)).unwrap();
        let rendered = terminal
            .backend()
            .buffer()
            .content
            .chunks(width as usize)
            .map(|row| {
                row.iter()
                    .map(|c| c.symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/picker-snapshots/{name}.txt"));
        if std::env::var_os("RTR_UPDATE_PICKER_SNAPSHOTS").is_some() {
            std::fs::write(&path, &rendered).unwrap();
        }
        assert_eq!(rendered, std::fs::read_to_string(path).unwrap());
    }
}

#[test]
fn late_results_preserve_local_selection_and_only_the_latest_preview_replaces_content() {
    let mut app = app();
    let mut row = app.snapshot.rows[0].clone();
    row.key = "next".into();
    app.snapshot.rows.push(row);
    let old = app.snapshot.clone();
    app.move_selection(1);
    app.apply(old);
    assert_eq!(app.query.selected.as_deref(), Some("next"));
    assert!(app.ready());
    let original = app.snapshot.preview.excerpts[0].text.clone();
    let mut stale = Snapshot {
        revision: app.query.revision,
        generation: app.generation,
        preview_only: true,
        ..Snapshot::default()
    };
    stale.preview.key = Some("next".into());
    stale.preview.excerpts = vec![search::Excerpt {
        text: "stale".into(),
        ..search::Excerpt::default()
    }];
    app.apply(stale.clone());
    assert_eq!(app.snapshot.preview.excerpts[0].text, original);
    stale.preview_revision = app.query.preview_revision;
    app.apply(stale);
    assert_eq!(app.snapshot.preview.excerpts[0].text, "stale");
}

#[test]
fn refresh_with_same_selection_retries_a_tab_request_from_the_previous_generation() {
    let mut app = app();
    let mut result = app.snapshot.clone();
    app.generation += 1;
    app.changed(false);
    result.generation = app.generation;
    result.revision = app.query.revision;
    result.preview_revision = app.query.preview_revision;
    // The worker captured Conversation before the user requested Details while
    // discovery was still holding the old generation. Identity does not change.
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    let selected = app.query.selected.clone();
    let old = app.snapshot.preview.excerpts[0].text.clone();
    assert!(
        app.apply(result),
        "the event loop must retry the latest preview intent"
    );
    assert_eq!(app.query.selected, selected);
    assert_eq!(app.snapshot.preview.excerpts[0].text, old);
    assert!(app.ready());
    let latest = Snapshot {
        generation: app.generation,
        revision: app.query.revision,
        preview_revision: app.query.preview_revision,
        preview_only: true,
        preview: worker::Preview {
            key: selected,
            excerpts: vec![search::Excerpt {
                text: "Current Details tab".into(),
                ..search::Excerpt::default()
            }],
            ..worker::Preview::default()
        },
        ..Snapshot::default()
    };
    assert!(!app.apply(latest));
    assert_eq!(app.snapshot.preview.excerpts[0].text, "Current Details tab");
}

#[test]
fn each_metadata_field_matches_literal_terms_including_first_prompt_and_hyphens() {
    let mut c = conversation();
    c.title = "unrelated".into();
    c.cwd = PathBuf::from("/project");
    c.id = "opaque".into();
    c.first_prompt = None;
    assert!(search::Search::new("rorate").matches(&c, None).is_none());
    for field in 0..4 {
        let mut sample = c.clone();
        match field {
            0 => sample.title = "Prefix Session-Owner Suffix".into(),
            1 => sample.cwd = PathBuf::from("Prefix Session-Owner Suffix"),
            2 => sample.id = "Prefix Session-Owner Suffix".into(),
            _ => sample.first_prompt = Some("Prefix Session-Owner Suffix".into()),
        }
        let expected = Some(if field == 0 { 2 } else { 1 });
        for text in [
            "SESSION-OWNER",
            "'session-owner",
            "^prefix suffix$",
            "session-owner !missing",
        ] {
            assert_eq!(
                search::Search::new(text).matches(&sample, None),
                expected,
                "{field}: {text}"
            );
        }
        for text in ["ssnwnr", "session-owner missing", "session-owner !OWNER"] {
            assert!(
                search::Search::new(text).matches(&sample, None).is_none(),
                "{field}: {text}"
            );
        }
    }
    c.title = "First term".into();
    c.first_prompt = Some("Second term".into());
    assert_eq!(
        search::Search::new("first second").matches(&c, None),
        Some(1)
    );
    assert!(search::Search::new("first !second")
        .matches(&c, None)
        .is_none());
    let index = search::Index::new(&c, Vec::new());
    assert_eq!(
        index.metadata_matches(&search::Search::new("^second")),
        vec!["first prompt"]
    );
}

#[test]
fn contextual_unicode_case_matching_highlights_the_same_original_characters() {
    let mut c = conversation();
    c.title = "ΟΣ".into();
    let search = search::Search::new("ΟΣ");
    assert_eq!(search.matches(&c, None), Some(2));
    assert_eq!(search.literal_indices(&c.title), vec![0, 1]);
    let index = search::Index::new(&c, Vec::new());
    assert_eq!(index.metadata_matches(&search), vec!["title"]);
}

#[test]
fn an_overlapping_suffix_hit_is_highlighted_and_has_a_metadata_excerpt() {
    let mut c = conversation();
    c.title = "ababa".into();
    let search = search::Search::new("aba$");
    assert_eq!(search.matches(&c, None), Some(2));
    assert_eq!(search.literal_indices(&c.title), vec![2, 3, 4]);
    let index = search::Index::new(&c, Vec::new());
    assert_eq!(index.metadata_matches(&search), vec!["title"]);
}
