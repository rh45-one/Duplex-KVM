//! Shared Nothing-inspired design system for every Duplex-KVM front-end
//! (GTK 3 on Linux, Win32 on Windows, and the embedded Web UI).
//!
//! One source of truth for:
//! - **Typography**: Space Grotesk (UI / body), Space Mono (data / labels), Doto (hero only).
//!   The static TTFs are embedded in the executable so the UI renders identically offline.
//! - **Color tokens**: OLED-black dark mode and warm off-white light mode. Both are first-class.
//! - **Spacing scale**: 8px base.
//! - **Mode catalog**: glyphs, names, descriptions and shortcuts for the four operational modes.
//! - **Data semantics**: heartbeat freshness (250ms dead-man's switch), number formatting.

use duplex_proto::OperationalMode;

// ───────────────────────────────────────────────────────────────────────────────────────────────
// 1. TYPOGRAPHY — embedded static font instances (SIL Open Font License 1.1, see assets/fonts)
// ───────────────────────────────────────────────────────────────────────────────────────────────

pub mod fonts {
    pub const SPACE_GROTESK_REGULAR: &[u8] =
        include_bytes!("../assets/fonts/SpaceGrotesk-Regular.ttf");
    pub const SPACE_GROTESK_MEDIUM: &[u8] =
        include_bytes!("../assets/fonts/SpaceGrotesk-Medium.ttf");
    pub const SPACE_MONO_REGULAR: &[u8] = include_bytes!("../assets/fonts/SpaceMono-Regular.ttf");
    pub const SPACE_MONO_BOLD: &[u8] = include_bytes!("../assets/fonts/SpaceMono-Bold.ttf");
    pub const DOTO_BOLD: &[u8] = include_bytes!("../assets/fonts/Doto-Bold.ttf");

    /// `(file name, bytes)` for every embedded face. File names are also the Web UI routes
    /// (`/fonts/<file name>`).
    pub const ALL: &[(&str, &[u8])] = &[
        ("SpaceGrotesk-Regular.ttf", SPACE_GROTESK_REGULAR),
        ("SpaceGrotesk-Medium.ttf", SPACE_GROTESK_MEDIUM),
        ("SpaceMono-Regular.ttf", SPACE_MONO_REGULAR),
        ("SpaceMono-Bold.ttf", SPACE_MONO_BOLD),
        ("Doto-Bold.ttf", DOTO_BOLD),
    ];

    /// Family names as registered by fontconfig / Pango / CSS.
    pub const FAMILY_GROTESK: &str = "Space Grotesk";
    pub const FAMILY_MONO: &str = "Space Mono";
    pub const FAMILY_DOTO: &str = "Doto";

    /// Legacy (nameID 1) GDI face names. GDI does not understand typographic families, so the
    /// Medium static instance must be requested by its own face name with `FW_NORMAL`.
    pub const GDI_FACE_GROTESK: &str = "Space Grotesk";
    pub const GDI_FACE_GROTESK_MEDIUM: &str = "Space Grotesk Medium";
    pub const GDI_FACE_MONO: &str = "Space Mono";
    /// Doto Bold is a RIBBI style: request face `"Doto"` with `FW_BOLD`.
    pub const GDI_FACE_DOTO: &str = "Doto";
}

/// Type scale in logical pixels (DIPs). Per screen we use at most four sizes:
/// hero (Doto, fluid), medium, body, label.
pub mod type_scale {
    /// Hero glyph (Doto) per layout tier.
    pub const HERO_COMPACT: f32 = 120.0;
    pub const HERO_MEDIUM: f32 = 168.0;
    pub const HERO_WIDE: f32 = 224.0;
    /// Mode title, RTT readout.
    pub const MEDIUM: f32 = 24.0;
    /// Body copy, stat values, descriptions.
    pub const BODY: f32 = 14.0;
    /// Buttons (Space Mono caps).
    pub const BUTTON: f32 = 13.0;
    /// ALL CAPS Space Mono instrument labels.
    pub const LABEL: f32 = 11.0;
    /// Letter-spacing for labels (em).
    pub const LABEL_TRACKING_EM: f32 = 0.08;
    /// Letter-spacing for buttons (em).
    pub const BUTTON_TRACKING_EM: f32 = 0.06;
}

// ───────────────────────────────────────────────────────────────────────────────────────────────
// 2. COLOR SYSTEM
// ───────────────────────────────────────────────────────────────────────────────────────────────

/// A complete set of color tokens. All values are `0xRRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    pub black: u32,
    pub surface: u32,
    pub surface_raised: u32,
    pub border: u32,
    pub border_visible: u32,
    pub text_disabled: u32,
    pub text_secondary: u32,
    pub text_primary: u32,
    pub text_display: u32,
    pub accent: u32,
    pub success: u32,
    pub warning: u32,
    pub interactive: u32,
    /// Empty segment in segmented progress bars.
    pub segment_empty: u32,
}

/// Dark mode: instrument panel in a dark room. OLED black, white data glowing.
pub const DARK: Palette = Palette {
    black: 0x000000,
    surface: 0x111111,
    surface_raised: 0x1A1A1A,
    border: 0x222222,
    border_visible: 0x333333,
    text_disabled: 0x666666,
    text_secondary: 0x999999,
    text_primary: 0xE8E8E8,
    text_display: 0xFFFFFF,
    accent: 0xD71921,
    success: 0x4A9E5C,
    warning: 0xD4A843,
    interactive: 0x5B9BF6,
    segment_empty: 0x222222,
};

/// Light mode: printed technical manual. Off-white paper, black ink.
pub const LIGHT: Palette = Palette {
    black: 0xF5F5F5,
    surface: 0xFFFFFF,
    surface_raised: 0xF0F0F0,
    border: 0xE8E8E8,
    border_visible: 0xCCCCCC,
    text_disabled: 0x999999,
    text_secondary: 0x666666,
    text_primary: 0x1A1A1A,
    text_display: 0x000000,
    accent: 0xD71921,
    success: 0x4A9E5C,
    warning: 0xD4A843,
    interactive: 0x007AFF,
    segment_empty: 0xE0E0E0,
};

/// Formats `0xRRGGBB` as `#RRGGBB`.
pub fn hex(color: u32) -> String {
    format!("#{:06X}", color & 0x00FF_FFFF)
}

/// Splits `0xRRGGBB` into floating point `(r, g, b)` in `0.0..=1.0` (Cairo / GDI+ helpers).
pub fn rgb_f64(color: u32) -> (f64, f64, f64) {
    (
        ((color >> 16) & 0xFF) as f64 / 255.0,
        ((color >> 8) & 0xFF) as f64 / 255.0,
        (color & 0xFF) as f64 / 255.0,
    )
}

/// Win32 `COLORREF` (`0x00BBGGRR`) from `0xRRGGBB`.
pub fn colorref(color: u32) -> u32 {
    ((color & 0xFF) << 16) | (color & 0xFF00) | ((color >> 16) & 0xFF)
}

/// User appearance preference. `Auto` follows the operating system.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemePref {
    #[default]
    Auto,
    Dark,
    Light,
}

impl ThemePref {
    pub const ALL: [ThemePref; 3] = [ThemePref::Auto, ThemePref::Dark, ThemePref::Light];

    pub fn label(self) -> &'static str {
        match self {
            ThemePref::Auto => "AUTO",
            ThemePref::Dark => "DARK",
            ThemePref::Light => "LIGHT",
        }
    }

    /// Resolves the preference against the OS setting.
    pub fn resolve(self, system_prefers_dark: bool) -> &'static Palette {
        match self {
            ThemePref::Dark => &DARK,
            ThemePref::Light => &LIGHT,
            ThemePref::Auto => {
                if system_prefers_dark {
                    &DARK
                } else {
                    &LIGHT
                }
            }
        }
    }
}

/// Data status encoding. Color is applied to the **value**, never to labels or backgrounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// `--text-primary`
    Neutral,
    /// `--success`
    Good,
    /// `--warning`
    Moderate,
    /// `--accent`
    Bad,
    /// `--text-disabled`
    Muted,
}

impl Level {
    pub fn color(self, p: &Palette) -> u32 {
        match self {
            Level::Neutral => p.text_primary,
            Level::Good => p.success,
            Level::Moderate => p.warning,
            Level::Bad => p.accent,
            Level::Muted => p.text_disabled,
        }
    }

    pub fn css_class(self) -> &'static str {
        match self {
            Level::Neutral => "lv-neutral",
            Level::Good => "lv-good",
            Level::Moderate => "lv-moderate",
            Level::Bad => "lv-bad",
            Level::Muted => "lv-muted",
        }
    }
}

// ───────────────────────────────────────────────────────────────────────────────────────────────
// 3. SPACING (8px base) & LAYOUT TIERS
// ───────────────────────────────────────────────────────────────────────────────────────────────

pub mod space {
    pub const XS2: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 8.0;
    pub const MD: f32 = 16.0;
    pub const LG: f32 = 24.0;
    pub const XL: f32 = 32.0;
    pub const XL2: f32 = 48.0;
    pub const XL3: f32 = 64.0;
    pub const XL4: f32 = 96.0;

    /// Card radius (mode tiles, widgets).
    pub const RADIUS_CARD: f32 = 12.0;
    /// Technical radius (chips, segmented control inner).
    pub const RADIUS_TECH: f32 = 4.0;
    /// Minimum interactive target.
    pub const TOUCH_MIN: f32 = 44.0;
    /// Max content width before the layout centers itself.
    pub const CONTENT_MAX: f32 = 1440.0;
}

/// Responsive layout tier for native windows, derived from the client width in DIPs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// `< 600` — single column, stacked hero, 1 tile per row.
    Compact,
    /// `600..960` — stacked hero, 2 tiles per row, 2-column telemetry.
    Medium,
    /// `>= 960` — side-by-side hero + link widget, 4 tiles per row.
    Wide,
}

impl Tier {
    pub fn from_width(width_dip: f32) -> Self {
        if width_dip < 600.0 {
            Tier::Compact
        } else if width_dip < 960.0 {
            Tier::Medium
        } else {
            Tier::Wide
        }
    }

    pub fn page_padding(self) -> f32 {
        match self {
            Tier::Compact => space::MD,
            Tier::Medium => space::LG,
            Tier::Wide => space::XL2,
        }
    }

    pub fn hero_size(self) -> f32 {
        match self {
            Tier::Compact => type_scale::HERO_COMPACT,
            Tier::Medium => type_scale::HERO_MEDIUM,
            Tier::Wide => type_scale::HERO_WIDE,
        }
    }

    pub fn tile_columns(self) -> usize {
        match self {
            Tier::Compact => 1,
            Tier::Medium => 2,
            Tier::Wide => 4,
        }
    }

    pub fn telemetry_columns(self) -> usize {
        match self {
            Tier::Compact => 1,
            Tier::Medium | Tier::Wide => 2,
        }
    }

    pub fn css_class(self) -> &'static str {
        match self {
            Tier::Compact => "tier-compact",
            Tier::Medium => "tier-medium",
            Tier::Wide => "tier-wide",
        }
    }
}

/// Native window defaults (DIPs).
pub const WINDOW_DEFAULT_W: i32 = 1120;
pub const WINDOW_DEFAULT_H: i32 = 780;
pub const WINDOW_MIN_W: i32 = 360;
pub const WINDOW_MIN_H: i32 = 480;

/// Motion: subtle ease-out, never spring.
pub const MOTION_MICRO_MS: u32 = 150;
pub const MOTION_EASE_CSS: &str = "cubic-bezier(0.25, 0.1, 0.25, 1)";

// ───────────────────────────────────────────────────────────────────────────────────────────────
// 4. MODE CATALOG
// ───────────────────────────────────────────────────────────────────────────────────────────────

/// Presentation data for one operational mode.
#[derive(Debug, Clone, Copy)]
pub struct ModeSpec {
    /// Wire code (`OperationalMode::as_u8`).
    pub code: u8,
    /// Hero glyph rendered in Doto: `0`, `1`, `1+`, `2`.
    pub glyph: &'static str,
    /// Short ALL CAPS name.
    pub name: &'static str,
    /// Sentence-case title for the hero.
    pub title: &'static str,
    /// One-sentence description.
    pub description: &'static str,
    /// Keyboard shortcut label.
    pub shortcut: &'static str,
    /// Target string accepted by `POST /api/mode?target=`.
    pub api_target: &'static str,
    pub mode: OperationalMode,
}

/// Modes in cycle order (`Ctrl+Alt+P`): 0 → 1 → 1+ → 2.
pub const MODES: [ModeSpec; 4] = [
    ModeSpec {
        code: 0,
        glyph: "0",
        name: "IDLE",
        title: "Idle",
        description: "Local only. Peripherals and displays stay independent; 1GbE passthrough stays up.",
        shortcut: "CTRL+ALT+0",
        api_target: "idle",
        mode: OperationalMode::Mode0Idle,
    },
    ModeSpec {
        code: 3,
        glyph: "1",
        name: "PC PRIMARY",
        title: "PC Primary",
        description: "Laptop keyboard and trackpad drive the Windows desktop.",
        shortcut: "CTRL+ALT+1",
        api_target: "pc_kvm",
        mode: OperationalMode::Mode1PcPrimary {
            extend_display: false,
        },
    },
    ModeSpec {
        code: 1,
        glyph: "1+",
        name: "PC + DISPLAY",
        title: "PC Primary + Display",
        description: "Input control plus one Windows virtual display streamed to the laptop.",
        shortcut: "CTRL+ALT+2",
        api_target: "pc_extend",
        mode: OperationalMode::Mode1PcPrimary {
            extend_display: true,
        },
    },
    ModeSpec {
        code: 2,
        glyph: "2",
        name: "LAPTOP PRIMARY",
        title: "Laptop Primary",
        description: "Windows peripherals and both monitors dock to the laptop.",
        shortcut: "CTRL+ALT+3",
        api_target: "laptop",
        mode: OperationalMode::Mode2LaptopPrimary,
    },
];

/// Looks up a mode by wire code (unknown codes fall back to Idle, like `from_u8_lossy`).
pub fn mode_spec(code: u8) -> &'static ModeSpec {
    MODES.iter().find(|m| m.code == code).unwrap_or(&MODES[0])
}

pub const SHORTCUT_CYCLE: &str = "CTRL+ALT+P";
pub const SHORTCUT_PANIC: &str = "CTRL+ALT+SHIFT+BKSP";
pub const SHORTCUT_PANIC_ALT: &str = "ESC ×5";

// ───────────────────────────────────────────────────────────────────────────────────────────────
// 5. DATA SEMANTICS & FORMATTING
// ───────────────────────────────────────────────────────────────────────────────────────────────

/// Dead-man's switch deadline: the link drops if the peer heartbeat is older than this.
pub const HEARTBEAT_DEADLINE_MS: f64 = 250.0;
/// Number of discrete segments in the heartbeat bar.
pub const HEARTBEAT_SEGMENTS: usize = 25;
/// RTT sparkline history length (samples at the UI poll rate).
pub const RTT_HISTORY_LEN: usize = 120;

/// Status color for a heartbeat age relative to the 250ms deadline.
pub fn heartbeat_level(age_ms: f64, connected: bool) -> Level {
    if !connected {
        return Level::Muted;
    }
    let ratio = age_ms / HEARTBEAT_DEADLINE_MS;
    if ratio < 0.4 {
        Level::Good
    } else if ratio < 0.8 {
        Level::Moderate
    } else {
        Level::Bad
    }
}

/// Filled segment count for the heartbeat bar (at least 1 while connected).
pub fn heartbeat_filled_segments(age_ms: f64, connected: bool) -> usize {
    if !connected {
        return 0;
    }
    let ratio = (age_ms / HEARTBEAT_DEADLINE_MS).clamp(0.0, 1.0);
    ((ratio * HEARTBEAT_SEGMENTS as f64).ceil() as usize).clamp(1, HEARTBEAT_SEGMENTS)
}

/// Groups digits with thin spaces: `1234567` → `1 234 567`.
pub fn fmt_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('\u{2009}');
        }
        out.push(ch);
    }
    out
}

/// RTT readout `(value, unit)`. Sub-millisecond values stay in µs; `—` when unknown.
pub fn fmt_rtt(us: f64) -> (String, &'static str) {
    if us <= 0.0 || !us.is_finite() {
        ("—".to_string(), "µs")
    } else if us < 1000.0 {
        (format!("{us:.0}"), "µs")
    } else {
        (format!("{:.2}", us / 1000.0), "ms")
    }
}

/// Milliseconds with one decimal, `—` when zero/unknown.
pub fn fmt_ms(ms: f64) -> String {
    if ms <= 0.0 || !ms.is_finite() {
        "—".to_string()
    } else {
        format!("{ms:.1}")
    }
}

/// Fixed-size ring buffer of RTT samples for sparklines in native front-ends.
#[derive(Debug, Clone)]
pub struct RttHistory {
    samples: [f64; RTT_HISTORY_LEN],
    len: usize,
    head: usize,
}

impl Default for RttHistory {
    fn default() -> Self {
        Self {
            samples: [0.0; RTT_HISTORY_LEN],
            len: 0,
            head: 0,
        }
    }
}

impl RttHistory {
    pub fn push(&mut self, us: f64) {
        self.samples[self.head] = if us.is_finite() { us.max(0.0) } else { 0.0 };
        self.head = (self.head + 1) % RTT_HISTORY_LEN;
        self.len = (self.len + 1).min(RTT_HISTORY_LEN);
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.head = 0;
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Samples oldest → newest.
    pub fn iter(&self) -> impl Iterator<Item = f64> + '_ {
        let start = (self.head + RTT_HISTORY_LEN - self.len) % RTT_HISTORY_LEN;
        (0..self.len).map(move |i| self.samples[(start + i) % RTT_HISTORY_LEN])
    }

    /// `(min, max)` of the stored samples, ignoring zeros. `None` if no valid sample.
    pub fn range(&self) -> Option<(f64, f64)> {
        let mut it = self.iter().filter(|v| *v > 0.0);
        let first = it.next()?;
        Some(it.fold((first, first), |(lo, hi), v| (lo.min(v), hi.max(v))))
    }
}

/// Link status shown in headers: `(value text, level)`.
pub fn daemon_status(online: bool) -> (&'static str, Level) {
    if online {
        ("ONLINE", Level::Good)
    } else {
        ("OFFLINE", Level::Bad)
    }
}

pub fn peer_status(online: bool, connected: bool) -> (&'static str, Level) {
    if !online {
        ("—", Level::Muted)
    } else if connected {
        ("LINKED", Level::Good)
    } else {
        ("STANDBY", Level::Moderate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_grouping() {
        assert_eq!(fmt_count(0), "0");
        assert_eq!(fmt_count(999), "999");
        assert_eq!(fmt_count(1000), "1\u{2009}000");
        assert_eq!(fmt_count(1234567), "1\u{2009}234\u{2009}567");
    }

    #[test]
    fn mode_codes_match_proto() {
        for m in MODES {
            assert_eq!(m.mode.as_u8(), m.code);
            assert_eq!(mode_spec(m.code).glyph, m.glyph);
        }
    }

    #[test]
    fn heartbeat_segments() {
        assert_eq!(heartbeat_filled_segments(0.0, false), 0);
        assert_eq!(heartbeat_filled_segments(0.0, true), 1);
        assert_eq!(heartbeat_filled_segments(500.0, true), HEARTBEAT_SEGMENTS);
        assert_eq!(heartbeat_level(20.0, true), Level::Good);
        assert_eq!(heartbeat_level(240.0, true), Level::Bad);
    }

    #[test]
    fn history_ring() {
        let mut h = RttHistory::default();
        for i in 0..(RTT_HISTORY_LEN + 5) {
            h.push(i as f64 + 1.0);
        }
        assert_eq!(h.len(), RTT_HISTORY_LEN);
        assert_eq!(h.iter().next(), Some(6.0));
        assert_eq!(h.range(), Some((6.0, (RTT_HISTORY_LEN + 5) as f64)));
    }
}
