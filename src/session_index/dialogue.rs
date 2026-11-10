//! Byte-addressable search projection shared by mapped snapshots and new parses.
//!
//! Offsets describe original and folded message boundaries. Readers check every
//! range before use; they never cast disk bytes to Rust structs or assume UTF-8.
//! Opening a view touches only its fixed header, leaving dialogue pages on disk.
use anyhow::{ensure, Result};
use memmap2::Mmap;
use std::{ops::Range, sync::Arc};

const HEADER: usize = 24;
const MESSAGE: usize = 40;

#[derive(Clone)]
enum Bytes {
    Owned(Arc<[u8]>),
    Mapped(Arc<Mmap>),
}

/// Immutable original/folded text and fixed-width message coordinates. Clones
/// retain the mapped inode, so replacing or deleting its pathname is harmless.
#[derive(Clone)]
pub(crate) struct Dialogue {
    bytes: Bytes,
    range: Range<usize>,
    text: Range<usize>,
    folded: Range<usize>,
    count: usize,
}

pub(crate) struct Message<'a> {
    pub role: &'static str,
    pub text: &'a str,
    pub folded: &'a str,
}

fn integer(bytes: &[u8], at: usize) -> Option<usize> {
    usize::try_from(u64::from_le_bytes(
        bytes.get(at..at.checked_add(8)?)?.try_into().ok()?,
    ))
    .ok()
}
fn push(bytes: &mut Vec<u8>, n: usize) {
    bytes.extend_from_slice(&(n as u64).to_le_bytes());
}

impl Dialogue {
    pub fn new(messages: Vec<(String, String)>) -> Self {
        let mut text = String::new();
        let mut folded = String::new();
        let mut table = Vec::with_capacity(messages.len() * MESSAGE);
        let count = messages.len();
        for (role, message) in messages {
            text.push('\n');
            folded.push('\n');
            push(&mut table, usize::from(role != "user"));
            push(&mut table, text.len());
            text.push_str(&message);
            push(&mut table, text.len());
            push(&mut table, folded.len());
            folded.push_str(&message.to_lowercase());
            push(&mut table, folded.len());
        }
        let mut bytes = Vec::with_capacity(HEADER + table.len() + text.len() + folded.len());
        push(&mut bytes, text.len());
        push(&mut bytes, folded.len());
        push(&mut bytes, count);
        bytes.extend(table);
        bytes.extend(text.as_bytes());
        bytes.extend(folded.as_bytes());
        let len = bytes.len();
        Self::view(Bytes::Owned(bytes.into()), 0..len).expect("encoded dialogue")
    }

    pub fn mapped(bytes: Arc<Mmap>, range: Range<usize>) -> Result<Self> {
        Self::view(Bytes::Mapped(bytes), range)
    }

    fn view(bytes: Bytes, range: Range<usize>) -> Result<Self> {
        let all = match &bytes {
            Bytes::Owned(b) => b.as_ref(),
            Bytes::Mapped(b) => b.as_ref(),
        };
        let slice = all
            .get(range.clone())
            .ok_or_else(|| anyhow::anyhow!("dialogue outside snapshot"))?;
        let text_len =
            integer(slice, 0).ok_or_else(|| anyhow::anyhow!("missing dialogue header"))?;
        let folded_len =
            integer(slice, 8).ok_or_else(|| anyhow::anyhow!("missing dialogue header"))?;
        let count = integer(slice, 16).ok_or_else(|| anyhow::anyhow!("missing dialogue header"))?;
        let start = count
            .checked_mul(MESSAGE)
            .and_then(|n| n.checked_add(HEADER));
        let text = start
            .zip(start.and_then(|n| n.checked_add(text_len)))
            .map(|(start, end)| start..end)
            .ok_or_else(|| anyhow::anyhow!("dialogue length overflow"))?;
        let end = text
            .end
            .checked_add(folded_len)
            .ok_or_else(|| anyhow::anyhow!("dialogue length overflow"))?;
        ensure!(end == slice.len(), "invalid dialogue length");
        let folded = text.end..end;
        Ok(Self {
            bytes,
            range,
            text,
            folded,
            count,
        })
    }

    pub fn encoded(&self) -> &[u8] {
        let all = match &self.bytes {
            Bytes::Owned(b) => b.as_ref(),
            Bytes::Mapped(b) => b.as_ref(),
        };
        &all[self.range.clone()]
    }
    pub fn len(&self) -> usize {
        self.count
    }
    pub fn message(&self, index: usize) -> Option<Message<'_>> {
        if index >= self.count {
            return None;
        }
        let bytes = self.encoded();
        let at = HEADER + index * MESSAGE;
        let role = match integer(bytes, at)? {
            0 => "user",
            1 => "assistant",
            _ => return None,
        };
        let text = bytes
            .get(self.text.clone())?
            .get(integer(bytes, at + 8)?..integer(bytes, at + 16)?)?;
        let folded = bytes
            .get(self.folded.clone())?
            .get(integer(bytes, at + 24)?..integer(bytes, at + 32)?)?;
        Some(Message {
            role,
            text: std::str::from_utf8(text).ok()?,
            folded: std::str::from_utf8(folded).ok()?,
        })
    }
    /// Structural/text validation is deferred until background reconciliation.
    /// A bad block is a cache miss and is rebuilt from its physical checkpoint;
    /// validation never delays painting the cached catalog.
    pub fn valid(&self) -> bool {
        let bytes = self.encoded();
        std::str::from_utf8(&bytes[self.text.clone()]).is_ok()
            && std::str::from_utf8(&bytes[self.folded.clone()]).is_ok()
            && (0..self.count).all(|i| self.message(i).is_some())
    }

    /// Ordinary substring search needs no text decoding or message allocation.
    /// Anchored terms use individual checked message slices instead.
    pub fn contains(&self, term: &str) -> bool {
        memchr::memmem::find(&self.encoded()[self.folded.clone()], term.as_bytes()).is_some()
    }
}
