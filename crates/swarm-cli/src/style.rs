//! Palette and geometry, transcribed from the shipped UIs rather than invented.
//!
//! Colours, glyphs, indent widths, truncation markers, spinner frames and the
//! turn verb lists come from the shipped Claude Code binary (2.1.311:
//! `themes.json` for the palette, embedded `mod_frames`/`mod_turn`/`mod_tasks`
//! for glyphs and strings — see `cc-ui-spec.md`), with a few segments from the
//! Hermes CLI where it shows more per-turn detail. The theme is Claude Code's
//! default `dark`. The point is that the terminal reads like the tools people
//! already know: brand-terracotta chevron, `●` action bullets, bold tool names,
//! SGR-2 dim results at a 2-cell gutter, `+N` for what was cut, and a
//! violet-titled permission dialog.
//!
//! Everything is gated: with colours off (pipe, `NO_COLOR`, `TERM=dumb`,
//! `SWARM_UI=never`) the exact plain bytes come out, which is what the tests
//! assert.

use std::env;
use std::io::{self, IsTerminal};

/// Anthropic Terracotta — `color:"claude"`, dark theme rgb(215,119,87). Brand,
/// assistant bullet, prompt chevron, emphasis.
pub const BRAND: &str = "#D77757";
/// Background of the echoed user line (`userMessageBackground`, rgb(0,40,50)).
pub const USER_BG: &str = "#002832";
/// Tool-call arguments and secondary text (`secondary`, rgb(153,153,153)).
pub const TOOL: &str = "#999999";
/// Assistant prose (`text`, dark theme rgb(255,255,255)).
pub const TEXT: &str = "#FFFFFF";
/// Tool results: the `⎿` branch and everything under it.
pub const RESULT: &str = "#999999";
/// Quietest text — counters, hints (`subtle`, rgb(102,102,102)).
pub const SUBTLE: &str = "#666666";
/// Permission dialog titles and dialog commands (`color:"permission"`).
pub const PERMISSION: &str = "#B169F0";
/// Warnings (`color:"warning"`, rgb(255,193,7)).
pub const WARNING: &str = "#FFC107";
pub const ERROR: &str = "#FF6B6B";
pub const SUCCESS: &str = "#69DB7C";
/// "did you mean" suggestions and the select pointer (`color:"suggestion"`).
pub const SUGGESTION: &str = "#93A7F7";
/// The input box's top rule (`promptBorder`, rgb(153,153,153)).
pub const PROMPT_BORDER: &str = "#999999";
/// Inverse text inside a panel (Claude Code inverts, it does not recolour).
pub const SELECTED_BG: &str = "#626262";

/// Claude Code's left gutter is 2 cells (`Box{minWidth:2}`, `paddingLeft:2`).
pub const INDENT: &str = "  ";
/// `  ⎿  ` — two-space gutter, branch, two spaces, body at column 6.
pub const BRANCH_GUTTER: &str = "  \u{23bf} ";
/// Lines of a tool's input/output shown before collapsing the rest.
pub const PREVIEW_LINES: usize = 8;
/// Claude Code's spinner: six frames played forward then reversed (`mod_frames`
/// `["·","✢","*","✶","✻","✽"]` + `toReversed()`), 100 ms per frame.
pub const SPINNER: [&str; 12] = [
    "\u{00b7}", "\u{2722}", "*", "\u{2736}", "\u{273b}", "\u{273d}", "\u{273d}", "\u{273b}",
    "\u{2736}", "*", "\u{2722}", "\u{00b7}",
];
pub const SPINNER_MS: u64 = 100;
/// Hermes' braille spinner at 100 ms, used where a frame must not change width.
pub const BRAILLE: [&str; 10] = [
    "\u{280b}", "\u{2819}", "\u{2839}", "\u{2838}", "\u{283c}", "\u{2834}", "\u{2826}", "\u{2827}",
    "\u{2807}", "\u{280f}",
];
pub const BRAILLE_MS: u64 = 100;
/// The assistant bullet: `⏺` on macOS, `●` everywhere else
/// (`Ro = macos ? "⏺" : "●"`), coloured `BRAND`.
pub const fn bullet() -> &'static str {
    if cfg!(target_os = "macos") {
        "\u{23fa}"
    } else {
        "\u{25cf}"
    }
}

/// Live-turn verbs, verbatim from the shipped 187-entry list (a subset). Claude
/// Code picks one per turn and never re-rolls it mid-turn.
pub const SPARKLE_VERBS: [&str; 14] = [
    "Accomplishing",
    "Calculating",
    "Composing",
    "Cogitating",
    "Contemplating",
    "Crafting",
    "Crunching",
    "Deliberating",
    "Forging",
    "Manifesting",
    "Pondering",
    "Synthesizing",
    "Thinking",
    "Working",
];

/// Post-turn footer verbs (`mod_turn`: 8 entries, hashed per session).
pub const PAST_VERBS: [&str; 8] =
    ["Baked", "Brewed", "Churned", "Cogitated", "Cooked", "Crunched", "Saut\u{e9}ed", "Worked"];

/// The live verb for a turn: stable for a given seed.
pub fn verb(seed: u64) -> &'static str {
    SPARKLE_VERBS[(seed % SPARKLE_VERBS.len() as u64) as usize]
}

/// The footer verb: `✻ Cogitated for 12s`.
pub fn past_verb(seed: u64) -> &'static str {
    PAST_VERBS[(seed % PAST_VERBS.len() as u64) as usize]
}

/// One random tip per session, Claude Code style.
pub const TIPS: [&str; 6] = [
    "b to expand all tool results, ctrl+o for full detail",
    "ctrl+c interrupts the run; /resume <id> picks the run back up",
    "/allow <tool> pins a tool for the rest of the session",
    "shift+tab cycles approvals: ask dangerous, ask all, ask never",
    "swarm logs <run> --transcript replays any finished run",
    "type /help for every command",
];

/// Turns `#RRGGBB` into its channels. Panics on malformed input: these are
/// constants in this file, so a typo should never reach a user's terminal.
pub fn rgb(hex: &str) -> (u8, u8, u8) {
    let body = hex.strip_prefix('#').unwrap_or(hex);
    assert!(body.len() == 6, "colour must be #RRGGBB, got {hex}");
    let channel = |range: std::ops::Range<usize>| {
        u8::from_str_radix(&body[range], 16).expect("hex colour channel")
    };
    (channel(0..2), channel(2..4), channel(4..6))
}

/// Colour support for the current streams, overridable with `SWARM_UI`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Plain,
    Ansi,
}

/// TTY detection, overridable with `SWARM_UI=always|never` so the live view can
/// be exercised in CI, pipes, and screenshot tooling.
pub fn ui_enabled(streams_tty: bool) -> bool {
    match env::var("SWARM_UI").ok().as_deref() {
        Some("always") => true,
        Some("never") => false,
        _ => streams_tty,
    }
}

impl Color {
    /// Colors only on a real terminal, never for pipes, `NO_COLOR`, or `dumb`.
    pub fn detect(stream_tty: bool) -> Self {
        let tty = ui_enabled(stream_tty) && io::stdout().is_terminal();
        let dumb = env::var("TERM").map(|term| term == "dumb").unwrap_or(false);
        if !tty || env::var_os("NO_COLOR").is_some() || dumb {
            Color::Plain
        } else {
            Color::Ansi
        }
    }
}

/// One SGR colour: truecolor foreground, optional background, bold/italic/dim.
#[derive(Debug, Clone, Copy)]
pub struct Srgb {
    pub hex: &'static str,
    pub bg: Option<&'static str>,
    pub bold: bool,
    pub italic: bool,
    pub dim: bool,
}

impl Srgb {
    pub const fn fg(hex: &'static str) -> Self {
        Self { hex, bg: None, bold: false, italic: false, dim: false }
    }

    pub const fn strong(hex: &'static str) -> Self {
        Self { hex, bg: None, bold: true, italic: false, dim: false }
    }

    /// Claude Code's result styling: grey-600, bold, dim *and* italic.
    pub const fn quiet(hex: &'static str) -> Self {
        Self { hex, bg: None, bold: true, italic: true, dim: true }
    }

    pub const fn on(mut self, bg: &'static str) -> Self {
        self.bg = Some(bg);
        self
    }

    fn escape(&self) -> String {
        let (r, g, b) = rgb(self.hex);
        let mut out = format!("\x1b[38;2;{r};{g};{b}m");
        if let Some(bg) = self.bg {
            let (br, bg_, bb) = rgb(bg);
            out.push_str(&format!("\x1b[48;2;{br};{bg_};{bb}m"));
        }
        if self.bold {
            out.push_str("\x1b[1m");
        }
        if self.dim {
            out.push_str("\x1b[2m");
        }
        if self.italic {
            out.push_str("\x1b[3m");
        }
        out
    }

    pub fn paint(&self, colors: Color, text: &str) -> String {
        if colors == Color::Plain || text.is_empty() {
            return text.to_string();
        }
        format!("{}{text}\x1b[0m", self.escape())
    }
}

/// The named roles the transcript asks for. Keeping this a small closed set is
/// what lets the whole UI be re-skinned from one place.
pub fn role(colors: Color, name: &str, text: &str) -> String {
    let style = match name {
        // Ground truth: tool names are bold on the text colour; only the
        // arguments and counters go grey.
        "action" => Srgb::strong(TEXT),
        "bullet" => Srgb::fg(BRAND),
        "result" => Srgb::quiet(RESULT),
        "text" => Srgb::fg(TEXT),
        "meta" => Srgb::quiet(SUBTLE),
        "ok" => Srgb::strong(SUCCESS),
        "fail" => Srgb::strong(ERROR),
        "warn" => Srgb::strong(WARNING),
        "ask" => Srgb::strong(PERMISSION),
        "brand" => Srgb::fg(BRAND),
        "user" => Srgb::strong(BRAND),
        "echo" => Srgb::fg(TEXT).on(USER_BG),
        "suggest" => Srgb::fg(SUGGESTION),
        _ => return text.to_string(),
    };
    style.paint(colors, text)
}

/// `547` / `1.2k` / `1.2M` — Claude Code's token formatter.
pub fn tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

/// `12.4s` under a minute, `1m05s` beyond it (Hermes' rule).
pub fn elapsed(seconds: f64) -> String {
    if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        let whole = seconds as u64;
        format!("{}m{:02}s", whole / 60, whole % 60)
    }
}

/// `[██████░░░░░] 42%` context gauge (Hermes).
pub fn gauge(fraction: f64, cells: usize) -> String {
    let clamped = fraction.clamp(0.0, 1.0);
    let filled = (clamped * cells as f64).round() as usize;
    let mut out = String::with_capacity(cells);
    for index in 0..cells {
        out.push(if index < filled { '\u{2588}' } else { '\u{2591}' });
    }
    format!("[{out}] {}%", (clamped * 100.0).round() as u32)
}

/// Tail-truncate with Claude Code's `+N` marker (not Hermes' `...`).
pub fn tail_truncate(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let mut out: String = chars[..limit.max(1)].iter().collect();
    out.push('+');
    out.push_str(&(chars.len() - limit).to_string());
    out
}

/// Head-truncate a long path so the tail (the interesting part) survives.
pub fn head_truncate(text: &str, limit: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= limit {
        return text.to_string();
    }
    let mut out = String::from('+');
    out.extend(chars[chars.len() - limit..].iter());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colours_become_channels() {
        assert_eq!(rgb("#D77757"), (215, 119, 87));
        assert_eq!(rgb("#002832"), (0, 40, 50));
    }

    #[test]
    fn token_counts_use_compact_units() {
        assert_eq!(tokens(547), "547");
        assert_eq!(tokens(1200), "1.2k");
        assert_eq!(tokens(2_400_000), "2.4M");
    }

    #[test]
    fn elapsed_switches_to_minutes_past_a_minute() {
        assert_eq!(elapsed(12.36), "12.4s");
        assert_eq!(elapsed(65.0), "1m05s");
        assert_eq!(elapsed(600.0), "10m00s");
    }

    #[test]
    fn gauge_fills_and_labels() {
        assert_eq!(gauge(0.42, 10), "[\u{2588}\u{2588}\u{2588}\u{2588}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}\u{2591}] 42%");
        assert_eq!(gauge(2.0, 4), "[\u{2588}\u{2588}\u{2588}\u{2588}] 100%");
    }

    #[test]
    fn truncation_markers_are_additive_not_ellipsis() {
        assert_eq!(tail_truncate("abcdefghij", 4), "abcd+6");
        assert_eq!(tail_truncate("abc", 4), "abc");
        assert_eq!(head_truncate("/very/long/path/file.py", 8), "+/file.py");
    }

    #[test]
    fn colours_are_exact_noops_when_disabled() {
        assert_eq!(role(Color::Plain, "brand", "swarm"), "swarm");
        let painted = role(Color::Ansi, "brand", "swarm");
        assert!(painted.starts_with("\x1b[38;2;215;119;87m"), "{painted}");
        assert!(painted.ends_with("\x1b[0m"));
    }

    #[test]
    fn the_bullet_is_brand_and_the_user_line_is_backed() {
        // Ground truth: the assistant bullet carries colour and no background;
        // only the echoed user line gets userMessageBackground.
        let bullet = role(Color::Ansi, "bullet", bullet());
        assert!(bullet.contains("38;2;215;119;87"), "{bullet}");
        assert!(!bullet.contains("48;2;"), "{bullet}");
        let echo = role(Color::Ansi, "echo", "fix the bug");
        assert!(echo.contains("48;2;0;40;50"), "{echo}");
    }

    #[test]
    fn spinner_frames_are_one_cell_and_ping_pong() {
        // Six source frames played forward then reversed, so frame 0 and the
        // last frame match and the cycle never jumps.
        for frame in SPINNER {
            assert!(frame.chars().count() == 1, "frame {frame:?} is not one cell");
        }
        assert_eq!(SPINNER[0], SPINNER[SPINNER.len() - 1]);
        assert_eq!(SPINNER[1], SPINNER[SPINNER.len() - 2]);
        for frame in BRAILLE {
            assert!(frame.chars().count() == 1, "frame {frame:?} is not one cell");
        }
    }

    #[test]
    fn verbs_come_from_the_shipped_lists_and_are_stable() {
        assert_eq!(verb(0), SPARKLE_VERBS[0]);
        assert_eq!(verb(14), SPARKLE_VERBS[0]);
        assert_eq!(past_verb(3), "Cogitated");
        assert_eq!(past_verb(11), PAST_VERBS[3]);
        assert!(SPARKLE_VERBS.contains(&"Working"));
    }

    #[test]
    fn the_bullet_glyph_follows_the_platform_rule() {
        #[cfg(target_os = "macos")]
        assert_eq!(bullet(), "\u{23fa}");
        #[cfg(not(target_os = "macos"))]
        assert_eq!(bullet(), "\u{25cf}");
    }
}
