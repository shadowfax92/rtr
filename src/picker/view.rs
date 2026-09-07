//! Responsive terminal rendering. All content is already bounded and sanitized;
//! painting never reads profile files, spawns a command, or indexes transcripts.
use super::{
    search::{self, Excerpt, Tab},
    App,
};
use chrono::Utc;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, BorderType, Borders, Cell, Clear, Paragraph, Row, Table, Tabs, Wrap},
    Frame,
};

const ACCENT: Color = Color::Cyan;

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    let area = frame.area();
    if area.width < 32 || area.height < 12 {
        frame.render_widget(
            Paragraph::new("Enlarge the terminal to browse sessions.\nEsc cancels."),
            area,
        );
        return;
    }
    let sections = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(area);
    let status = if app.snapshot.generation != app.generation || !app.snapshot.loaded {
        "Loading sessions…".into()
    } else if app.snapshot.indexed < app.snapshot.total {
        format!(
            "{} results · indexing {}/{}",
            app.snapshot.rows.len(),
            app.snapshot.indexed,
            app.snapshot.total
        )
    } else {
        format!(
            "{} / {} sessions{}",
            app.snapshot.rows.len(),
            app.snapshot.total,
            if app.snapshot.diagnostics.is_empty() {
                ""
            } else {
                " · some files unreadable"
            }
        )
    };
    let title = Layout::horizontal([Constraint::Length(16), Constraint::Min(0)]).split(sections[0]);
    frame.render_widget(
        Paragraph::new(" RTR sessions").style(Style::default().bold()),
        title[0],
    );
    frame.render_widget(
        Paragraph::new(status).right_aligned().style(dim()),
        title[1],
    );
    search_input(frame, app, sections[1]);
    let filters = format!(
        " Alt-H {}  ·  Alt-T {}  ·  Alt-A {}",
        if app.query.filters.here {
            "this directory"
        } else {
            "all projects"
        },
        app.query.filters.tool.as_deref().unwrap_or("all agents"),
        app.query
            .filters
            .profile
            .as_deref()
            .unwrap_or("all profiles")
    );
    frame.render_widget(
        Paragraph::new(search::clean(&filters))
            .style(dim())
            .wrap(Wrap { trim: false }),
        sections[2],
    );

    let body = sections[3];
    if app.show_preview && body.height >= 8 {
        let wide = body.width >= 110 && body.width as f32 / body.height as f32 >= 1.8;
        let split = Layout::default()
            .direction(if wide {
                Direction::Horizontal
            } else {
                Direction::Vertical
            })
            .constraints(if wide {
                [Constraint::Percentage(50), Constraint::Percentage(50)]
            } else {
                [Constraint::Percentage(42), Constraint::Percentage(58)]
            })
            .split(body);
        list(frame, app, split[0]);
        preview(frame, app, split[1]);
    } else {
        list(frame, app, body);
    }

    let action = if let Some(notice) = &app.notice {
        search::clean(notice)
    } else if app.ready() {
        app.selected()
            .map(|row| {
                app.snapshot.preview.launch.line(
                    &row.conversation,
                    app.mode,
                    app.to_profile.as_deref(),
                )
            })
            .unwrap_or_else(|| "No conversation selected".into())
    } else if let Some(error) = &app.snapshot.error {
        format!("Error: {}", search::clean(error))
    } else {
        "Updating results…".into()
    };
    frame.render_widget(
        Paragraph::new(format!(" {action}")).style(Style::default().fg(ACCENT)),
        sections[4],
    );
    let controls = if app.to_profile.is_some() {
        " Enter fork  Ctrl-F fork  Tab preview  F1 help".to_string()
    } else if area.width >= 84 {
        format!(
            " Enter {}  Ctrl-F fork  Ctrl-R resume  Tab preview  Alt-Y copy  F1 help",
            app.mode.label()
        )
    } else if area.width >= 60 {
        format!(
            " Enter {}  Ctrl-R resume  Tab preview  F1 help",
            app.mode.label()
        )
    } else {
        format!(
            " Enter {}  {}  Tab view  F1 help",
            app.mode.label(),
            if app.mode == crate::conversations::OpenMode::Fork {
                "^R resume"
            } else {
                "^F fork"
            }
        )
    };
    frame.render_widget(Paragraph::new(controls).style(dim()), sections[5]);
    if app.help {
        help(frame, app, area);
    }
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn search_input(frame: &mut Frame, app: &App, area: Rect) {
    let room = area.width.saturating_sub(4) as usize;
    let before = &app.query.text[..app.cursor];
    let mut start = app.cursor;
    let mut width = 0;
    for (index, ch) in before.char_indices().rev() {
        let char_width = Span::raw(ch.to_string()).width();
        if width + char_width >= room {
            break;
        }
        width += char_width;
        start = index;
    }
    let display = format!(" > {}", &app.query.text[start..]);
    frame.render_widget(Paragraph::new(display), area);
    if !app.help {
        frame.set_cursor_position((area.x + 3 + width as u16, area.y));
    }
}

fn list(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP)
        .title(" Sessions ")
        .border_style(dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if app.snapshot.rows.is_empty() {
        let message = if let Some(error) = &app.snapshot.error {
            format!("Could not load sessions\n\n{}", search::clean(error))
        } else if !app.snapshot.loaded {
            "Loading session metadata…\n\nYou can type while sessions load.".into()
        } else if app.snapshot.total == 0 {
            "No saved conversations found.\n\nStart a conversation with rtr codex or rtr claude."
                .into()
        } else {
            format!(
                "No matching conversations.\n\nChange the query or widen the filters.{}",
                if app.snapshot.indexed < app.snapshot.total {
                    "\nTranscript matches are still indexing."
                } else {
                    ""
                }
            )
        };
        frame.render_widget(
            Paragraph::new(message)
                .style(dim())
                .wrap(Wrap { trim: false }),
            inner,
        );
        return;
    }
    let project = inner.width >= 76;
    let profile = inner.width >= 44;
    let reserved = 11 + if profile { 17 } else { 0 } + if project { 17 } else { 0 };
    let title_width = inner.width.saturating_sub(reserved) as usize;
    let mut headers = vec!["Conversation", "Updated"];
    let mut widths = vec![Constraint::Min(8), Constraint::Length(7)];
    if profile {
        headers.push("Agent/Profile");
        widths.push(Constraint::Length(15));
    }
    if project {
        headers.push("Project");
        widths.push(Constraint::Length(15));
    }
    let now = Utc::now();
    let rows = app.snapshot.rows.iter().map(|row| {
        let c = &row.conversation;
        let title = truncate(&search::clean(&c.title), title_width);
        let mut cells = vec![
            Cell::from(highlighted(&title, &row.highlights)),
            Cell::from(relative_age((now - c.updated_at).num_seconds())).style(dim()),
        ];
        if profile {
            cells.push(
                Cell::from(format!(
                    "{}/{}",
                    search::clean(&c.tool),
                    search::clean(&c.profile)
                ))
                .style(dim()),
            );
        }
        if project {
            let name = c
                .cwd
                .file_name()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_else(|| c.cwd.display().to_string());
            cells.push(Cell::from(truncate(&search::clean(&name), 15)).style(dim()));
        }
        Row::new(cells)
    });
    let table = Table::new(rows, widths)
        .header(Row::new(headers).style(dim()).bottom_margin(1))
        .column_spacing(2)
        .row_highlight_style(Style::default().add_modifier(Modifier::REVERSED))
        .highlight_symbol("› ");
    frame.render_stateful_widget(table, inner, &mut app.table);
}

fn preview(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::default()
        .borders(Borders::TOP | Borders::LEFT)
        .border_style(dim());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 {
        return;
    }
    let parts = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
    let active = match app.query.tab {
        Tab::Conversation => 0,
        Tab::Matches => 1,
        Tab::Details => 2,
    };
    let labels = if inner.width >= 40 {
        vec!["1 Conversation", "2 Matches", "3 Details"]
    } else {
        match app.query.tab {
            Tab::Conversation => vec!["1 Conversation", "2", "3"],
            Tab::Matches => vec!["1", "2 Matches", "3"],
            Tab::Details => vec!["1", "2", "3 Details"],
        }
    };
    frame.render_widget(
        Tabs::new(labels)
            .select(active)
            .style(dim())
            .highlight_style(Style::default().fg(ACCENT).bold())
            .divider(" ")
            .padding("", " "),
        parts[0],
    );
    if !app.ready() || app.snapshot.preview.key != app.query.selected {
        frame.render_widget(Paragraph::new("Loading preview…").style(dim()), parts[1]);
        return;
    }
    let mut lines = Vec::new();
    if app.snapshot.preview.matches > 0 {
        lines.push(Line::styled(
            format!(
                "Passage {}/{}  Alt-B/N previous/next",
                app.snapshot.preview.match_index + 1,
                app.snapshot.preview.matches
            ),
            dim(),
        ));
    }
    for excerpt in &app.snapshot.preview.excerpts {
        lines.push(Line::default());
        lines.push(Line::styled(
            excerpt.label.clone(),
            Style::default().fg(ACCENT).bold(),
        ));
        lines.extend(excerpt_lines(excerpt));
    }
    if lines.is_empty() {
        lines.push(Line::styled("Select a conversation to preview it.", dim()));
    }
    let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
    // Use the renderer's word wrapping to clamp scroll; estimating by raw text
    // width can make the final lines unreachable on a narrow pane.
    let max_scroll = paragraph
        .line_count(parts[1].width)
        .saturating_sub(parts[1].height as usize)
        .min(u16::MAX as usize) as u16;
    app.scroll = app.scroll.min(max_scroll);
    frame.render_widget(paragraph.scroll((app.scroll, 0)), parts[1]);
}

fn excerpt_lines(excerpt: &Excerpt) -> Vec<Line<'static>> {
    let mut offset = 0;
    excerpt
        .text
        .split('\n')
        .map(|text| {
            let length = text.chars().count() as u32;
            let indices: Vec<_> = excerpt
                .highlights
                .iter()
                .copied()
                .filter(|i| *i >= offset && *i < offset + length)
                .map(|i| i - offset)
                .collect();
            offset += length + 1;
            highlighted(text, &indices)
        })
        .collect()
}

fn highlighted(text: &str, indices: &[u32]) -> Line<'static> {
    let mut spans = Vec::new();
    let mut segment = String::new();
    let mut previous = false;
    for (index, ch) in text.chars().enumerate() {
        let matched = indices.binary_search(&(index as u32)).is_ok();
        if matched != previous && !segment.is_empty() {
            spans.push(styled_segment(std::mem::take(&mut segment), previous));
        }
        segment.push(ch);
        previous = matched;
    }
    if !segment.is_empty() {
        spans.push(styled_segment(segment, previous));
    }
    Line::from(spans)
}

fn styled_segment(text: String, matched: bool) -> Span<'static> {
    Span::styled(
        text,
        if matched {
            Style::default().fg(Color::Yellow).bold()
        } else {
            Style::default()
        },
    )
}

fn truncate(text: &str, width: usize) -> String {
    if Span::raw(text).width() <= width {
        return text.into();
    }
    let mut result = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let size = Span::raw(ch.to_string()).width();
        if used + size >= width {
            break;
        }
        result.push(ch);
        used += size;
    }
    result.push('…');
    result
}

fn relative_age(seconds: i64) -> String {
    let seconds = seconds.max(0);
    match seconds {
        0..60 => "now".into(),
        60..3600 => format!("{}m ago", seconds / 60),
        3600..86400 => format!("{}h ago", seconds / 3600),
        _ => format!("{}d ago", seconds / 86400),
    }
}

fn help(frame: &mut Frame, app: &mut App, area: Rect) {
    let width = area.width.saturating_sub(4).min(76);
    let height = area.height.saturating_sub(2).min(24);
    let rect = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    frame.render_widget(Clear, rect);
    let content = format!(
        "Type to search titles, projects, IDs, and the complete human dialogue.\n\n\
         Enter          {} selected conversation\n\
         {}\n\
         Up / Down      Select conversation\n\
         Tab / Shift-Tab  Next / previous preview\n\
         Alt-1 / 2 / 3  Conversation / Matches / Details\n\
         Ctrl-U / Ctrl-D  Scroll preview\n\
         Alt-B / Alt-N  Previous / next matching passage\n\
         Alt-H          Toggle this directory / all projects\n\
         Alt-T / Alt-A  Cycle agent / profile\n\
         Alt-P          Show / hide preview\n\
         Alt-R          Refresh catalog and transcripts\n\
         Alt-Y          Copy the current action's command\n\
         Esc / Ctrl-C   Cancel (Esc closes this help first)\n\n\
         Scope directory: {}\n\n\
         Search: space-separated terms, 'exact, ^prefix, suffix$, !exclude.\n\
         Unreadable transcripts remain selectable by metadata.",
        if app.mode == crate::conversations::OpenMode::Fork {
            "Fork"
        } else {
            "Resume"
        },
        if app.to_profile.is_some() {
            "Ctrl-F          Fork to selected destination"
        } else {
            "Ctrl-F / Ctrl-R  Fork / resume explicitly"
        },
        search::clean(&app.cwd.display().to_string())
    );
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .title(" Controls · F1/Esc close · ↑↓ scroll ");
    let inner = block.inner(rect);
    let paragraph = Paragraph::new(content).wrap(Wrap { trim: false });
    app.help_scroll = app.help_scroll.min(
        paragraph
            .line_count(inner.width)
            .saturating_sub(inner.height as usize)
            .min(u16::MAX as usize) as u16,
    );
    frame.render_widget(block, rect);
    frame.render_widget(paragraph.scroll((app.help_scroll, 0)), inner);
}
