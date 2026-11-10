//! Searchable dialogue is independent of terminal rows and native session IDs.
//! Only the worker owns full transcript text. The terminal receives matching
//! rows and bounded excerpts, so rendering never walks a transcript.
use std::sync::Arc;

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
    pub preview_revision: u64,
    pub preview_messages: usize,
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

/// One searchable dialogue with byte ranges back to its original messages.
/// The folded projection supports substring membership without allocating on
/// every query; excerpts map Unicode folds back to original scalar positions.
pub(super) struct Index {
    pub text: String,
    folded: String,
    messages: Vec<MessageRange>,
    metadata: Vec<String>,
}

// Message bodies borrow byte ranges in the original projection, preserving
// paragraph boundaries without another owned copy of every message.
struct MessageRange {
    role: String,
    bytes: std::ops::Range<usize>,
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

/// These field boundaries are shared by membership and excerpts. Anchors apply
/// to each field independently; neither tool/profile labels nor dates are text
/// fields, because their picker filters already express those dimensions.
fn metadata_fields(conversation: &Conversation) -> [&str; 4] {
    [
        conversation.title.as_str(),
        conversation.cwd.to_str().unwrap_or(""),
        conversation.id.as_str(),
        conversation.first_prompt.as_deref().unwrap_or(""),
    ]
}

impl Index {
    pub fn new(conversation: &Conversation, messages: Vec<(String, String)>) -> Self {
        let mut text = String::new();
        let metadata = metadata_fields(conversation)
            .into_iter()
            .map(clean)
            .collect();
        let mut ranges = Vec::new();
        for (role, message) in messages {
            text.push('\n');
            let start = text.len();
            text.push_str(&message);
            ranges.push(MessageRange {
                role,
                bytes: start..text.len(),
            });
        }
        Self {
            folded: text.to_lowercase(),
            text,
            messages: ranges,
            metadata,
        }
    }

    #[cfg(test)]
    pub fn excerpts(&self, text: &str) -> Vec<Excerpt> {
        self.excerpts_cancellable(text, &|| false)
    }
    pub fn excerpts_cancellable(&self, text: &str, cancelled: &impl Fn() -> bool) -> Vec<Excerpt> {
        if text.trim().is_empty() {
            return Vec::new();
        }
        let search = Search::new(text);
        let mut excerpts = Vec::new();
        for field in &self.metadata {
            let positions = search.literal_indices(field);
            if !positions.is_empty() {
                excerpts.push(excerpt("Session metadata", field, &positions));
            }
        }
        for message in &self.messages {
            if cancelled() {
                return Vec::new();
            }
            let text = &self.text[message.bytes.clone()];
            let hits = search.literal_indices(text);
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

/// Terms are literal in every field and AND across fields/messages. One parser
/// drives membership and highlights, so a subsequence cannot outrank a real hit.
pub(super) struct Search {
    terms: Vec<Term>,
}
struct Term {
    text: String,
    negative: bool,
    prefix: bool,
    suffix: bool,
}
impl Term {
    fn literal(&self, text: &str) -> bool {
        // Anchors refer to message/field boundaries, not an arbitrary offset in
        // the concatenated corpus. Plain terms may span whitespace in a message.
        if self.prefix && self.suffix {
            text == self.text
        } else if self.prefix {
            text.starts_with(&self.text)
        } else if self.suffix {
            text.ends_with(&self.text)
        } else {
            text.contains(&self.text)
        }
    }
}
impl Search {
    pub fn new(text: &str) -> Self {
        let terms = text
            .split_whitespace()
            .filter_map(|raw| {
                let (negative, raw) = raw.strip_prefix('!').map_or((false, raw), |s| (true, s));
                let raw = raw.strip_prefix("'").unwrap_or(raw);
                let prefix = raw.starts_with('^');
                let suffix = raw.ends_with('$');
                let literal = raw
                    .trim_start_matches('^')
                    .trim_end_matches('$')
                    .to_lowercase();
                if literal.is_empty() {
                    return None;
                }
                Some(Term {
                    text: literal,
                    negative,
                    prefix,
                    suffix,
                })
            })
            .collect();
        Self { terms }
    }
    pub fn literal_indices(&self, text: &str) -> Vec<u32> {
        // Lowercasing may expand a Unicode scalar (e.g. İ). Retain a map back
        // to original scalar positions rather than treating folded bytes as UI offsets.
        let mut folded = String::new();
        let mut map = Vec::new();
        for (i, ch) in text.chars().enumerate() {
            for lower in ch.to_lowercase() {
                for _ in 0..lower.len_utf8() {
                    map.push(i as u32);
                }
                folded.push(lower);
            }
        }
        let mut positions = Vec::new();
        for term in self.terms.iter().filter(|t| !t.negative) {
            for (offset, _) in folded.match_indices(&term.text) {
                if term.prefix && offset != 0 {
                    continue;
                }
                if term.suffix && offset + term.text.len() != folded.len() {
                    continue;
                }
                positions.extend_from_slice(&map[offset..offset + term.text.len()]);
            }
        }
        positions.sort_unstable();
        positions.dedup();
        positions
    }
    /// Return a rank class, leaving recency as the tie break. Positive terms
    /// can be satisfied across different short fields and different messages.
    pub fn matches(&self, c: &Conversation, index: Option<&Index>) -> Option<u8> {
        let folded = metadata_fields(c).map(str::to_lowercase);
        let mut all_title = true;
        let mut metadata_hit = false;
        for term in &self.terms {
            let dialogue = index.is_some_and(|i| {
                if term.prefix || term.suffix {
                    i.messages
                        .iter()
                        .any(|m| term.literal(&i.text[m.bytes.clone()].to_lowercase()))
                } else {
                    i.folded.contains(&term.text)
                }
            });
            if term.negative {
                if dialogue || folded.iter().any(|f| term.literal(f)) {
                    return None;
                }
                continue;
            }
            let title = term.literal(&folded[0]);
            let metadata = folded.iter().any(|field| term.literal(field));
            if !metadata && !dialogue {
                return None;
            }
            all_title &= title;
            metadata_hit |= metadata;
        }
        Some(if all_title {
            2
        } else if metadata_hit {
            1
        } else {
            0
        })
    }
}
impl Index {
    pub fn conversation(&self, count: usize) -> Vec<Excerpt> {
        self.messages
            .iter()
            .skip(self.messages.len().saturating_sub(count))
            .map(|m| Excerpt {
                label: role_label(&m.role).into(),
                text: self.text[m.bytes.clone()].to_string(),
                highlights: Vec::new(),
            })
            .collect()
    }
}
