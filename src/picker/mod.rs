//! Interactive session selection, isolated from native launch policy.
//!
//! The terminal owns keyboard input and display state; worker threads own disk
//! reads and fuzzy matching. Only a stable Conversation + OpenMode crosses back
//! to the CLI adapter after raw mode and the alternate screen have been restored.
mod launch;
mod search;
#[cfg(test)]
mod tests;
mod view;
mod worker;

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::widgets::TableState;

use crate::{
    conversations::{Conversation, OpenMode},
    paths::Paths,
};
use search::{Filters, Query, Tab};
use worker::{Snapshot, Worker};

/// Initial filters and invocation intent supplied by the CLI adapter.
pub(crate) struct Options {
    pub query: Option<String>,
    pub tool: Option<String>,
    pub profile: Option<String>,
    pub here: bool,
    pub mode: OpenMode,
    pub to_profile: Option<String>,
    pub extra_args: Vec<String>,
}

/// Everything needed to paint a frame, including the last complete worker
/// snapshot. Revision checks prevent Enter from accepting an obsolete result
/// while the user is still editing a query or the catalog is refreshing.
struct App {
    query: Query,
    cursor: usize,
    snapshot: Snapshot,
    generation: u64,
    table: TableState,
    scroll: u16,
    help: bool,
    help_scroll: u16,
    show_preview: bool,
    notice: Option<String>,
    mode: OpenMode,
    to_profile: Option<String>,
    extra_args: Vec<String>,
    cwd: PathBuf,
}

impl App {
    fn new(options: Options, cwd: PathBuf) -> Self {
        let text = search::clean(&options.query.unwrap_or_default());
        let cursor = text.len();
        Self {
            query: Query {
                text,
                filters: Filters {
                    tool: options.tool,
                    profile: options.profile,
                    here: options.here,
                },
                ..Query::default()
            },
            cursor,
            snapshot: Snapshot::default(),
            generation: 0,
            table: TableState::default(),
            scroll: 0,
            help: false,
            help_scroll: 0,
            show_preview: true,
            notice: None,
            mode: options.mode,
            to_profile: options.to_profile,
            extra_args: options.extra_args,
            cwd,
        }
    }

    fn ready(&self) -> bool {
        self.snapshot.loaded
            && self.snapshot.revision == self.query.revision
            && self.snapshot.generation == self.generation
    }

    fn selected(&self) -> Option<&search::Row> {
        self.query
            .selected
            .as_ref()
            .and_then(|key| self.snapshot.rows.iter().find(|row| row.key == *key))
    }

    fn apply(&mut self, snapshot: Snapshot) {
        if snapshot.revision != self.query.revision || snapshot.generation != self.generation {
            return;
        }
        if self.query.selected != snapshot.selected {
            self.scroll = 0;
        }
        self.query.selected = snapshot.selected.clone();
        self.table.select(
            snapshot
                .selected
                .as_ref()
                .and_then(|key| snapshot.rows.iter().position(|row| row.key == *key)),
        );
        self.snapshot = snapshot;
    }

    fn changed(&mut self, reset_selection: bool) {
        self.query.revision += 1;
        self.notice = None;
        self.scroll = 0;
        if reset_selection {
            self.query.selected = None;
            self.query.match_index = 0;
            self.table.select(None);
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.snapshot.rows.is_empty() {
            return;
        }
        let current = self
            .query
            .selected
            .as_ref()
            .and_then(|key| self.snapshot.rows.iter().position(|row| row.key == *key))
            .unwrap_or(0);
        let next =
            (current as isize + delta).rem_euclid(self.snapshot.rows.len() as isize) as usize;
        self.query.selected = Some(self.snapshot.rows[next].key.clone());
        self.table.select(Some(next));
        self.query.match_index = 0;
        self.changed(false);
    }

    fn edit(&mut self, text: &str) {
        let text = search::clean(text);
        self.query.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
        self.changed(true);
    }

    fn key(&mut self, key: KeyEvent) -> Effect {
        if key.kind == KeyEventKind::Release {
            return Effect::None;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if control && key.code == KeyCode::Char('c') {
            return Effect::Cancel;
        }
        if key.code == KeyCode::F(1) {
            self.help = !self.help;
            self.help_scroll = 0;
            return Effect::None;
        }
        if key.code == KeyCode::Esc {
            if self.help {
                self.help = false;
                return Effect::None;
            }
            return Effect::Cancel;
        }
        if self.help {
            match key.code {
                KeyCode::Down => self.help_scroll = self.help_scroll.saturating_add(1),
                KeyCode::Up => self.help_scroll = self.help_scroll.saturating_sub(1),
                KeyCode::Char('d') if control => {
                    self.help_scroll = self.help_scroll.saturating_add(10)
                }
                KeyCode::Char('u') if control => {
                    self.help_scroll = self.help_scroll.saturating_sub(10)
                }
                _ => {}
            }
            return Effect::None;
        }
        match key.code {
            KeyCode::Enter => return Effect::Accept(self.mode),
            KeyCode::Char('r') if control && self.to_profile.is_some() => {
                // An explicit destination belongs to a fork; keep that intent
                // visible instead of silently dropping it on an in-place resume.
                self.notice =
                    Some("Destination selected: Enter forks; Ctrl-R is unavailable".into());
            }
            KeyCode::Char('r') if control => return Effect::Accept(OpenMode::Resume),
            KeyCode::Char('f') if control => return Effect::Accept(OpenMode::Fork),
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Char('u') if control => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Char('d') if control => self.scroll = self.scroll.saturating_add(10),
            KeyCode::Char('r') if alt => return Effect::Refresh,
            KeyCode::Char('y') if alt => return Effect::Copy,
            KeyCode::Char('p') if alt => self.show_preview = !self.show_preview,
            KeyCode::Char('h') if alt => {
                self.query.filters.here = !self.query.filters.here;
                self.changed(true);
            }
            KeyCode::Char('t') if alt => {
                self.query.filters.tool = match self.query.filters.tool.as_deref() {
                    None => Some("claude".into()),
                    Some("claude") => Some("codex".into()),
                    _ => None,
                };
                self.query.filters.profile = None;
                self.changed(true);
            }
            KeyCode::Char('a') if alt => {
                let profiles = &self.snapshot.profiles;
                self.query.filters.profile = match &self.query.filters.profile {
                    None => profiles.first().cloned(),
                    Some(current) => profiles
                        .iter()
                        .position(|profile| profile == current)
                        .and_then(|index| profiles.get(index + 1))
                        .cloned(),
                };
                self.changed(true);
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.query.tab = self.query.tab.cycle(
                    key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
                );
                self.changed(false);
            }
            KeyCode::Char(ch @ '1'..='3') if alt => {
                self.query.tab = match ch {
                    '1' => Tab::Conversation,
                    '2' => Tab::Matches,
                    _ => Tab::Details,
                };
                self.changed(false);
            }
            KeyCode::Char('n') if alt => {
                if self.snapshot.preview.matches > 0 {
                    self.query.match_index =
                        (self.query.match_index + 1) % self.snapshot.preview.matches;
                    self.changed(false);
                }
            }
            KeyCode::Char('b') if alt => {
                self.query.match_index = self.query.match_index.saturating_sub(1);
                self.changed(false);
            }
            KeyCode::Left => {
                self.cursor = self.query.text[..self.cursor]
                    .char_indices()
                    .last()
                    .map(|(i, _)| i)
                    .unwrap_or(0);
            }
            KeyCode::Right => {
                if let Some(ch) = self.query.text[self.cursor..].chars().next() {
                    self.cursor += ch.len_utf8();
                }
            }
            KeyCode::Home | KeyCode::Char('a') if key.code == KeyCode::Home || control => {
                self.cursor = 0
            }
            KeyCode::End | KeyCode::Char('e') if key.code == KeyCode::End || control => {
                self.cursor = self.query.text.len()
            }
            KeyCode::Backspace => {
                if let Some((previous, _)) = self.query.text[..self.cursor].char_indices().last() {
                    self.query.text.drain(previous..self.cursor);
                    self.cursor = previous;
                    self.changed(true);
                }
            }
            KeyCode::Delete => {
                if let Some(ch) = self.query.text[self.cursor..].chars().next() {
                    self.query
                        .text
                        .drain(self.cursor..self.cursor + ch.len_utf8());
                    self.changed(true);
                }
            }
            KeyCode::Char('w') if control => {
                let before = &self.query.text[..self.cursor];
                let start = before
                    .trim_end()
                    .rfind(char::is_whitespace)
                    .map(|i| i + 1)
                    .unwrap_or(0);
                self.query.text.drain(start..self.cursor);
                self.cursor = start;
                self.changed(true);
            }
            KeyCode::Char(ch) if !control && !alt => self.edit(&ch.to_string()),
            _ => {}
        }
        Effect::None
    }
}

enum Effect {
    None,
    Cancel,
    Accept(OpenMode),
    Refresh,
    Copy,
}

struct Terminal {
    terminal: ratatui::DefaultTerminal,
}

impl Terminal {
    fn enter() -> Result<Self> {
        match ratatui::try_init() {
            Ok(terminal) => {
                if let Err(error) = crossterm::execute!(io::stdout(), event::EnableBracketedPaste) {
                    ratatui::restore();
                    return Err(error.into());
                }
                Ok(Self { terminal })
            }
            Err(error) => {
                ratatui::restore();
                Err(error).context("initializing the session picker terminal")
            }
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = crossterm::execute!(io::stdout(), event::DisableBracketedPaste);
        ratatui::restore();
    }
}

pub(crate) fn run(paths: &Paths, options: Options) -> Result<Option<(Conversation, OpenMode)>> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "interactive session selection requires a terminal; use rtr sessions --list or --json"
        );
    }
    let cwd = std::env::current_dir().context("resolving the picker directory")?;
    let mut app = App::new(options, cwd.clone());
    let worker = Worker::new(paths, cwd, app.extra_args.clone());
    app.generation = worker.refresh();
    worker.search(&app.query);
    let mut terminal = Terminal::enter()?;
    loop {
        loop {
            match worker.updates.try_recv() {
                Ok(snapshot) => app.apply(snapshot),
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    bail!("background session search stopped unexpectedly");
                }
            }
        }
        terminal
            .terminal
            .draw(|frame| view::draw(frame, &mut app))?;
        if !event::poll(Duration::from_millis(50))? {
            continue;
        }
        let previous_revision = app.query.revision;
        let effect = match event::read()? {
            Event::Key(key) => app.key(key),
            Event::Paste(text) if !app.help => {
                app.edit(&text);
                Effect::None
            }
            _ => Effect::None,
        };
        match effect {
            Effect::Cancel => return Ok(None),
            Effect::Accept(mode) if app.ready() => {
                if let Some(row) = app.selected() {
                    return Ok(Some(((*row.conversation).clone(), mode)));
                }
            }
            Effect::Accept(_) => app.notice = Some("Updating results…".into()),
            Effect::Refresh => {
                app.generation = worker.refresh();
                app.changed(false);
            }
            Effect::Copy if app.ready() => {
                if let Some(row) = app.selected() {
                    let command = launch::copy_command(
                        &row.conversation,
                        app.mode,
                        &app.extra_args,
                        app.to_profile.as_deref(),
                    );
                    app.notice = Some(match copy_to_clipboard(&command) {
                        Ok(()) => format!("Copied {} command", app.mode.label()),
                        Err(error) => format!("Could not copy: {error}"),
                    });
                }
            }
            _ => {}
        }
        if app.query.revision != previous_revision {
            worker.search(&app.query);
        }
    }
}

fn copy_to_clipboard(text: &str) -> Result<()> {
    let choices: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("pbcopy", &[])]
    } else {
        &[("wl-copy", &[]), ("xclip", &["-selection", "clipboard"])]
    };
    for (program, arguments) in choices {
        let child = Command::new(program)
            .args(*arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match child {
            Ok(child) => child,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(error) = stdin.write_all(text.as_bytes()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        }
        if child.wait()?.success() {
            return Ok(());
        }
        bail!("{program} failed");
    }
    bail!("install pbcopy, wl-copy, or xclip")
}
