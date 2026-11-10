//! Searchable dialogue is independent of terminal rows and native session IDs.
//! Only the worker owns full transcript text. The terminal receives matching
//! rows and bounded excerpts, so rendering never walks a transcript.
use std::sync::Arc;

use crate::{conversations::Conversation, session_index::dialogue::Dialogue};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Tab {
    #[default]
    Conversation,
    Details,
}

impl Tab {
    pub fn cycle(self, _backwards: bool) -> Self {
        match self {
            Self::Conversation => Self::Details,
            Self::Details => Self::Conversation,
        }
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
    pub preview_newer_messages: usize,
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

/// Search view over immutable original/folded message bytes. New projections
/// own their buffer; warm projections borrow the mapped snapshot without copies.
pub(super) struct Index {
    pub dialogue: Dialogue,
    metadata: Vec<String>,
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
    #[cfg(test)]
    pub fn new(conversation: &Conversation, messages: Vec<(String, String)>) -> Self {
        Self::from_dialogue(conversation, Dialogue::new(messages))
    }

    pub fn from_dialogue(conversation: &Conversation, dialogue: Dialogue) -> Self {
        Self {
            dialogue,
            metadata: metadata_fields(conversation)
                .into_iter()
                .map(clean)
                .collect(),
        }
    }

    /// Locate matching messages only for the selected row. Store indices, not
    /// copied excerpts; the worker reuses them while paging this query.
    pub fn matching_messages(&self, search: &Search, cancelled: &impl Fn() -> bool) -> Vec<usize> {
        let mut matches = Vec::new();
        if !search.terms.iter().any(|term| !term.negative) {
            return matches;
        }
        for i in (0..self.dialogue.len()).rev() {
            if cancelled() {
                return Vec::new();
            }
            if self
                .dialogue
                .message(i)
                .is_some_and(|m| search.positive_match(m.folded))
            {
                matches.push(i);
            }
        }
        matches
    }

    pub fn metadata_matches(&self, search: &Search) -> Vec<&'static str> {
        self.metadata
            .iter()
            .zip(["title", "project", "ID", "first prompt"])
            .filter_map(|(text, label)| search.has_positive_match(text).then_some(label))
            .collect()
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
    fn has_positive_match(&self, text: &str) -> bool {
        let folded = text.to_lowercase();
        self.positive_match(&folded)
    }

    fn positive_match(&self, folded: &str) -> bool {
        self.terms
            .iter()
            .any(|term| !term.negative && term.literal(folded))
    }

    pub fn literal_indices(&self, text: &str) -> Vec<u32> {
        if !self.terms.iter().any(|term| !term.negative) {
            return Vec::new();
        }
        // Lowercasing may expand a Unicode scalar (e.g. İ). Retain a map back
        // to original scalar positions rather than treating folded bytes as UI
        // offsets. Use the whole-string fold for matching, just like membership:
        // Greek final sigma depends on context even though its offset width stays
        // the same as the character-wise fold used to build this coordinate map.
        let folded = text.to_lowercase();
        let mut map = Vec::new();
        for (i, ch) in text.chars().enumerate() {
            for lower in ch.to_lowercase() {
                for _ in 0..lower.len_utf8() {
                    map.push(i as u32);
                }
            }
        }
        let mut positions = Vec::new();
        for term in self.terms.iter().filter(|t| !t.negative) {
            if term.prefix || term.suffix {
                // An anchored suffix can overlap an earlier occurrence: aba$
                // matches the last three characters of ababa. Non-overlapping
                // match_indices would report only the earlier, unanchored hit.
                if term.literal(&folded) {
                    let offset = if term.suffix {
                        folded.len() - term.text.len()
                    } else {
                        0
                    };
                    positions.extend_from_slice(&map[offset..offset + term.text.len()]);
                }
            } else {
                for (offset, _) in folded.match_indices(&term.text) {
                    positions.extend_from_slice(&map[offset..offset + term.text.len()]);
                }
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
                    (0..i.dialogue.len()).any(|n| {
                        i.dialogue
                            .message(n)
                            .is_some_and(|m| term.literal(m.folded))
                    })
                } else {
                    i.dialogue.contains(&term.text)
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
/// Position within the bounded preview that should sit at the bottom on landing.
/// Character offsets refer to the original text, just like highlight offsets.
#[derive(Clone)]
pub(super) struct Anchor {
    pub excerpt: usize,
    pub characters: usize,
}

impl Index {
    /// Keep both sides of a match in one continuous window. The terminal scrolls
    /// relative to the anchor, so adding older/newer messages preserves position.
    pub fn conversation(
        &self,
        count: usize,
        newer_count: usize,
        target: Option<usize>,
        search: &Search,
    ) -> (Vec<Excerpt>, Option<Anchor>) {
        if self.dialogue.len() == 0 {
            return (Vec::new(), None);
        }
        let end = target.unwrap_or(self.dialogue.len() - 1);
        let start = (end + 1).saturating_sub(count);
        let stop = (end + 1)
            .saturating_add(if target.is_some() { newer_count } else { 0 })
            .min(self.dialogue.len());
        let excerpts: Vec<_> = (start..stop)
            .filter_map(|i| self.dialogue.message(i))
            .map(|m| {
                let text = m.text.to_string();
                let highlights = search.literal_indices(&text);
                Excerpt {
                    label: role_label(m.role).into(),
                    text,
                    highlights,
                }
            })
            .collect();
        let excerpt = end - start;
        let Some(selected) = excerpts.get(excerpt) else {
            return (Vec::new(), None);
        };
        let characters = if target.is_some() {
            selected
                .highlights
                .last()
                .map(|last| *last as usize + 1)
                .unwrap_or_else(|| selected.text.chars().count())
        } else {
            selected.text.chars().count()
        };
        (
            excerpts,
            Some(Anchor {
                excerpt,
                characters,
            }),
        )
    }
}
