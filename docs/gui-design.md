# Duplex interface design

The native GTK and Win32 applications and embedded web interface apply the
Nothing design language supplied in `nothing-design-skill.md`.

- **Primary:** one unboxed Doto-derived mode number (64 px on compact desktops, 80 px on
  larger web layouts).
- **Secondary:** Space Grotesk, 16 px, for mode names, descriptions and readings.
- **Tertiary:** Space Mono, 11 px, uppercase labels, shortcuts and technical
  controls. Regular text and the bold Doto hero are the only weight treatments.

All font files and their OFL licenses are bundled in `crates/duplex-gui/assets/fonts`.
The desktop registers these fonts privately, and the web server serves them from
`/fonts/`; neither interface needs Google Fonts or another external asset host.

Dark mode starts with OLED black and a four-level gray text hierarchy. Light mode
uses warm off-white and matching ink tones. Green, amber and red encode status
values. Emergency release gains red emphasis while input is grabbed. Controls
use flat surfaces and technical corners (4–6 px), with explicit active and focus
states. Update preferences use physical switch shapes. There are no gradients,
shadows or popup toasts; action feedback stays inline.

The main overview fits a typical HD desktop without scrolling. Short browser
viewports use compact mode cards, with descriptions retained in tooltips and the
active-mode summary. Narrow windows and expanded settings can scroll normally.

GTK styles are scoped to `#duplex-content` and `#duplex-scroll`. Window decorations,
title-bar typography, window buttons and their placement remain controlled by the
user's desktop theme. Appearance preferences persist between launches.

The hero font is **Duplex Dots**, a locally named Doto derivative with a conventional
5×7 rounded rectangular zero. All other glyphs, dot contours and font metrics are
unchanged. `scripts/build-hero-font.py` reproduces it from the original bundled
Doto TTF using fontTools. The original font and OFL license remain alongside it.
