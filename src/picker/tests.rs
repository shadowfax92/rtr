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
    let excerpts = index.excerpts("'unusual_old_needle");
    assert_eq!(excerpts.len(), 1);
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
        .score(&search::metadata(&conversation()))
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
    let repeated = index.excerpts("'search_needle");
    assert_eq!(repeated.len(), 2);
    assert_eq!(repeated[0].label, "You");
    assert_eq!(repeated[1].label, "Assistant");
    let spanning = index.excerpts("'first_unique 'second_unique");
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
fn tiny_frame_and_filter_shortcuts_do_not_panic_or_change_launch_mode() {
    let mut app = app();
    let mut terminal = TestTerminal::new(TestBackend::new(20, 5)).unwrap();
    terminal.draw(|frame| view::draw(frame, &mut app)).unwrap();
    app.key(KeyEvent::new(KeyCode::Char('h'), KeyModifiers::ALT));
    assert!(app.query.filters.here);
    app.key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::ALT));
    assert_eq!(app.query.filters.tool.as_deref(), Some("claude"));
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(app.query.tab, Tab::Matches);
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
    assert!(automatic.contains("→ next profile"));
}
