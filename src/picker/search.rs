//! Searchable dialogue is independent of terminal rows and native session IDs.
//! Only the worker owns full transcript text. The terminal receives matching
//! rows and bounded excerpts, so rendering never walks a transcript.
use std::cmp::Reverse;
use std::sync::Arc;

use nucleo_matcher::{
    pattern::{CaseMatching, Normalization, Pattern},
    Config, Matcher, Utf32Str,
};

use crate::conversations::Conversation;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Tab {
    #[default]
    Conversation,
    Matches,
    Details,
}

impl Tab {
    pub fn cycle(self, backwards: bool) -> Self {
        let tabs = [Self::Conversation, Self::Matches, Self::Details];
        let index = tabs.iter().position(|tab| *tab == self).unwrap_or(0);
        tabs[(index + if backwards { 2 } else { 1 }) % 3]
    }
}

#[derive(Clone, Default, PartialEq, Eq)]
pub(super) struct Filters {
    pub tool: Option<String>,
    pub profile: Option<String>,
    pub here: bool,
}

#[derive(Clone, Default)]
/// A complete query/selection request; revision correlates its worker response.
pub(super) struct Query {
    pub revision: u64,
    pub text: String,
    pub filters: Filters,
    pub selected: Option<String>,
    pub tab: Tab,
    pub match_index: usize,
}

#[derive(Clone)]
/// Display metadata plus immutable identity; no transcript bodies reach rows.
pub(super) struct Row {
    pub key: String,
    pub conversation: Arc<Conversation>,
    pub highlights: Vec<u32>,
}

/// One indexed dialogue, with character offsets back into its messages.
/// Nucleo returns character indices rather than byte offsets; storing the same
/// coordinates here keeps Unicode excerpts and their highlights aligned.
pub(super) struct Index {
    pub text: String,
    messages: Vec<MessageRange>,
    metadata_len: u32,
}

// Byte ranges borrow the one indexed string; character ranges translate matcher
// highlights. This avoids retaining a second copy of every dialogue message.
struct MessageRange {
    role: String,
    bytes: std::ops::Range<usize>,
    characters: std::ops::Range<u32>,
}

#[derive(Clone, Default)]
pub(super) struct Excerpt {
    pub label: String,
    pub text: String,
    pub highlights: Vec<u32>,
}

pub(super) fn clean(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

pub(super) fn metadata(conversation: &Conversation) -> String {
    format!(
        "{} {} {} {} {} {} {}",
        clean(&conversation.title),
        conversation.tool,
        conversation.profile,
        clean(&conversation.cwd.display().to_string()),
        conversation.id,
        clean(conversation.first_prompt.as_deref().unwrap_or("")),
        conversation.updated_at.format("%Y-%m-%d %H:%M")
    )
}

impl Index {
    pub fn new(conversation: &Conversation, messages: Vec<(String, String)>) -> Self {
        let mut text = metadata(conversation);
        let metadata_len = text.chars().count() as u32;
        let mut position = metadata_len;
        let mut ranges = Vec::new();
        for (role, message) in messages {
            text.push(' ');
            let start = text.len();
            text.push_str(&message);
            let length = message.chars().count() as u32;
            ranges.push(MessageRange {
                role,
                bytes: start..text.len(),
                characters: position + 1..position + 1 + length,
            });
            position += 1 + length;
        }
        Self {
            text,
            messages: ranges,
            metadata_len,
        }
    }

    pub fn excerpts(&self, text: &str) -> Vec<Excerpt> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        let mut search = Search::new(text);
        let positions = search.indices(&self.text);
        let mut excerpts = Vec::new();
        if positions.iter().any(|pos| *pos < self.metadata_len) {
            excerpts.push(excerpt(
                "Session metadata",
                &self
                    .text
                    .chars()
                    .take(self.metadata_len as usize)
                    .collect::<String>(),
                &positions
                    .iter()
                    .copied()
                    .filter(|pos| *pos < self.metadata_len)
                    .collect::<Vec<_>>(),
            ));
        }
        for message in &self.messages {
            let text = &self.text[message.bytes.clone()];
            // The whole-session match can span metadata and several messages.
            // Also match each message so repeated mentions remain browsable;
            // Nucleo otherwise returns only one best set of character positions.
            let mut hits = search.indices(text);
            hits.extend(
                positions
                    .iter()
                    .copied()
                    .filter(|pos| message.characters.contains(pos))
                    .map(|pos| pos - message.characters.start),
            );
            hits.sort_unstable();
            hits.dedup();
            if !hits.is_empty() {
                excerpts.push(excerpt(role_label(&message.role), text, &hits));
            }
        }
        excerpts
    }
}

fn excerpt(label: &str, text: &str, positions: &[u32]) -> Excerpt {
    let chars: Vec<_> = text.chars().collect();
    let first = positions.first().copied().unwrap_or(0) as usize;
    let start = first.saturating_sub(180);
    let end = (start + 1000).min(chars.len());
    let prefix = usize::from(start > 0);
    let mut excerpt = if start > 0 {
        "…".into()
    } else {
        String::new()
    };
    excerpt.extend(chars[start..end].iter());
    if end < chars.len() {
        excerpt.push('…');
    }
    Excerpt {
        label: label.into(),
        text: excerpt,
        highlights: positions
            .iter()
            .copied()
            .filter(|pos| (*pos as usize) >= start && (*pos as usize) < end)
            .map(|pos| pos - start as u32 + prefix as u32)
            .collect(),
    }
}

pub(super) fn role_label(role: &str) -> &str {
    if role == "user" {
        "You"
    } else {
        "Assistant"
    }
}

pub(super) struct Search {
    pattern: Pattern,
    matcher: Matcher,
    buffer: Vec<char>,
}

impl Search {
    pub fn new(text: &str) -> Self {
        Self {
            pattern: Pattern::parse(text, CaseMatching::Smart, Normalization::Smart),
            matcher: Matcher::new(Config::DEFAULT),
            buffer: Vec::new(),
        }
    }

    pub fn score(&mut self, text: &str) -> Option<u32> {
        self.pattern
            .score(Utf32Str::new(text, &mut self.buffer), &mut self.matcher)
    }

    pub fn indices(&mut self, text: &str) -> Vec<u32> {
        let mut indices = Vec::new();
        if self
            .pattern
            .indices(
                Utf32Str::new(text, &mut self.buffer),
                &mut self.matcher,
                &mut indices,
            )
            .is_none()
        {
            return Vec::new();
        }
        indices.sort_unstable();
        indices.dedup();
        indices
    }
}

/// Empty queries prefer this directory, then retain the catalog's recency order.
/// Typed queries rank metadata matches before dialogue-only matches. Position
/// remains the last tie-break, so progressively indexed rows do not shuffle ties.
pub(super) fn rank(
    score: u32,
    metadata_match: bool,
    here: bool,
    position: usize,
    empty: bool,
) -> (Reverse<bool>, Reverse<u32>, Reverse<bool>, usize) {
    (
        Reverse(!empty && metadata_match),
        Reverse(score),
        Reverse(here),
        position,
    )
}
