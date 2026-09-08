//! Shared presentation for RTR's noninteractive inspection commands.
//! Color is resolved once against stdout, then passed explicitly to renderers.
//! Padding uses visible Unicode width before styling so ANSI escapes never
//! influence table alignment. JSON bypasses this module entirely.
use std::io::{self, IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use clap::{Args, ValueEnum};
use ratatui::text::Span;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum ColorMode {
    #[default]
    Auto,
    Always,
    Never,
}

/// Scoped to inspection commands so native launch arguments stay child-owned.
#[derive(Args, Clone, Copy, Debug, Default)]
pub struct ColorArgs {
    /// Colorize human-readable output; auto respects NO_COLOR and terminal detection.
    #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
    pub color: ColorMode,
}

impl ColorMode {
    pub fn stdout(self) -> Style {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        let dumb = std::env::var_os("TERM").is_some_and(|value| value == "dumb");
        self.resolve(io::stdout().is_terminal(), no_color, dumb)
    }

    fn resolve(self, terminal: bool, no_color: bool, dumb: bool) -> Style {
        Style {
            color: match self {
                Self::Always => true,
                Self::Never => false,
                Self::Auto => terminal && !no_color && !dumb,
            },
        }
    }
}

#[derive(Clone, Copy)]
pub enum Tone {
    Normal,
    Accent,
    Strong,
    Muted,
    Good,
    Warning,
    Bad,
}

impl Tone {
    fn escape(self) -> &'static str {
        match self {
            Self::Normal => "",
            Self::Accent => "\x1b[36m",
            Self::Strong => "\x1b[1m",
            Self::Muted => "\x1b[2m",
            Self::Good => "\x1b[32m",
            Self::Warning => "\x1b[33m",
            Self::Bad => "\x1b[31m",
        }
    }
}

/// A resolved color policy; renderers never read the process environment.
#[derive(Clone, Copy, Default)]
pub struct Style {
    color: bool,
}

impl Style {
    pub fn paint(self, text: &str, tone: Tone) -> String {
        let text = clean(text);
        if self.color && !tone.escape().is_empty() {
            format!("{}{text}\x1b[0m", tone.escape())
        } else {
            text
        }
    }

    pub fn padded(self, text: &str, tone: Tone, width: usize, right: bool) -> String {
        let padding = " ".repeat(width.saturating_sub(display_width(text)));
        let text = self.paint(text, tone);
        if right {
            format!("{padding}{text}")
        } else {
            format!("{text}{padding}")
        }
    }

    pub fn path(self, path: &str) -> String {
        let split = path.rfind('/').map_or(0, |index| index + 1);
        format!(
            "{}{}",
            self.paint(&path[..split], Tone::Muted),
            self.paint(&path[split..], Tone::Accent)
        )
    }

    /// `rtr config` is a byte-exact path interface, including non-UTF-8 Unix
    /// filenames. Insert style bytes around its components without re-encoding
    /// the path; the ordinary human-table sanitizer must not touch this output.
    pub fn write_path(self, path: &Path, output: &mut impl Write) -> io::Result<()> {
        let bytes = path.as_os_str().as_bytes();
        if self.color {
            let split = bytes
                .iter()
                .rposition(|byte| *byte == b'/')
                .map_or(0, |i| i + 1);
            output.write_all(Tone::Muted.escape().as_bytes())?;
            output.write_all(&bytes[..split])?;
            output.write_all(b"\x1b[0m")?;
            output.write_all(Tone::Accent.escape().as_bytes())?;
            output.write_all(&bytes[split..])?;
            output.write_all(b"\x1b[0m")?;
        } else {
            output.write_all(bytes)?;
        }
        output.write_all(b"\n")
    }
}

pub fn clean(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

pub fn display_width(text: &str) -> usize {
    Span::raw(clean(text)).width()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_color_overrides_auto_detection_and_no_color() {
        for terminal in [false, true] {
            for no_color in [false, true] {
                for dumb in [false, true] {
                    assert_eq!(
                        ColorMode::Auto.resolve(terminal, no_color, dumb).color,
                        terminal && !no_color && !dumb
                    );
                    assert!(ColorMode::Always.resolve(terminal, no_color, dumb).color);
                    assert!(!ColorMode::Never.resolve(terminal, no_color, dumb).color);
                }
            }
        }
    }

    #[test]
    fn padding_uses_visible_width_and_untrusted_cells_cannot_inject_controls() {
        let plain = Style::default();
        let colored = ColorMode::Always.resolve(false, false, false);
        assert_eq!(plain.padded("東京", Tone::Strong, 6, false), "東京  ");
        assert_eq!(
            colored.padded("東京", Tone::Strong, 6, false),
            "\x1b[1m東京\x1b[0m  "
        );
        assert_eq!(plain.paint("name\n\x1b[31m", Tone::Strong), "name  [31m");
    }
}
