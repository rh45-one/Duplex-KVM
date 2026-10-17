use serde::{Deserialize, Serialize};

/// Canonical USB HID Usage Page 0x07 (Keyboard/Keypad) key codes for panic detection and modifier sanitization.
pub const HID_USAGE_ESCAPE: u8 = 0x29;
pub const HID_USAGE_BACKSPACE: u8 = 0x2A;

pub const HID_USAGE_LEFT_CTRL: u8 = 0xE0;
pub const HID_USAGE_LEFT_SHIFT: u8 = 0xE1;
pub const HID_USAGE_LEFT_ALT: u8 = 0xE2;
pub const HID_USAGE_LEFT_GUI: u8 = 0xE3; // Super / Windows key
pub const HID_USAGE_RIGHT_CTRL: u8 = 0xE4;
pub const HID_USAGE_RIGHT_SHIFT: u8 = 0xE5;
pub const HID_USAGE_RIGHT_ALT: u8 = 0xE6;
pub const HID_USAGE_RIGHT_GUI: u8 = 0xE7;

/// All 8 canonical USB HID modifier Usage IDs (0xE0..=0xE7).
pub const ALL_HID_MODIFIER_USAGES: [u8; 8] = [
    HID_USAGE_LEFT_CTRL,
    HID_USAGE_LEFT_SHIFT,
    HID_USAGE_LEFT_ALT,
    HID_USAGE_LEFT_GUI,
    HID_USAGE_RIGHT_CTRL,
    HID_USAGE_RIGHT_SHIFT,
    HID_USAGE_RIGHT_ALT,
    HID_USAGE_RIGHT_GUI,
];

/// Rolling time window (1.5 seconds in nanoseconds) for the 5x Escape panic sequence.
pub const PANIC_ESCAPE_WINDOW_NS: u64 = 1_500_000_000;
/// Number of Escape presses within `PANIC_ESCAPE_WINDOW_NS` required to trip the panic escape hatch.
pub const PANIC_ESCAPE_REQUIRED_PRESSES: usize = 5;

/// Normalized mouse button representation across Windows and Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum HidMouseButton {
    Left = 1,
    Right = 2,
    Middle = 3,
    Back = 4,
    Forward = 5,
}

/// Normalized, OS-agnostic USB HID input event transmitted over the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HidEvent {
    /// Keyboard key press or release using canonical USB HID Usage Page 0x07 ID.
    Key { usage: u8, pressed: bool },
    /// High-frequency relative mouse motion delta (unaccelerated raw counts).
    MouseMoveRel { dx: i16, dy: i16 },
    /// Mouse button press or release.
    MouseButton {
        button: HidMouseButton,
        pressed: bool,
    },
    /// Vertical and horizontal scroll wheel ticks (high-resolution 120-units or discrete notches).
    MouseWheel { vertical: i16, horizontal: i16 },
    /// Explicit command instructing receiver to release all depressed keys and modifiers immediately.
    ReleaseAllModifiers,
}

/// Reason a hardware panic escape hatch was triggered locally.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PanicTriggerKind {
    /// User held `Ctrl + Alt + Shift + Backspace`.
    ChordCtrlAltShiftBackspace,
    /// User pressed `Escape` 5 times within 1.5 seconds.
    RapidEscapeFiveTimes,
}

/// 256-bit bitmap tracking every currently depressed USB HID Usage ID (`0x00..=0xFF`).
///
/// Used on both capture and receiver sides to guarantee that mode switches, panics, or
/// disconnects never leave `Ctrl`, `Alt`, `Shift`, `Super/Win`, or alphanumeric keys stuck down.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct DepressedKeyBitmap {
    words: [u64; 4],
}

impl DepressedKeyBitmap {
    #[inline]
    pub const fn new() -> Self {
        Self { words: [0u64; 4] }
    }

    #[inline]
    pub const fn from_words(words: [u64; 4]) -> Self {
        Self { words }
    }

    #[inline]
    pub const fn as_words(&self) -> &[u64; 4] {
        &self.words
    }

    /// Updates the bitmap for `usage` and returns the previous pressed state.
    #[inline]
    pub fn set_pressed(&mut self, usage: u8, pressed: bool) -> bool {
        let idx = (usage >> 6) as usize;
        let bit = 1u64 << (usage & 63);
        let was_pressed = (self.words[idx] & bit) != 0;
        if pressed {
            self.words[idx] |= bit;
        } else {
            self.words[idx] &= !bit;
        }
        was_pressed
    }

    #[inline]
    pub const fn is_pressed(&self, usage: u8) -> bool {
        let idx = (usage >> 6) as usize;
        let bit = 1u64 << (usage & 63);
        (self.words[idx] & bit) != 0
    }

    #[inline]
    pub const fn clear(&mut self) {
        self.words = [0u64; 4];
    }

    /// Iterates all set bits (depressed keys), clearing the bitmap to all-zeros and yielding
    /// each depressed `usage: u8` so the caller can emit synthetic `KEY_UP` events.
    #[inline]
    pub fn drain_pressed(&mut self) -> DrainPressedIter {
        let words = self.words;
        self.words = [0u64; 4];
        DrainPressedIter {
            words,
            word_idx: 0,
        }
    }
}

pub struct DrainPressedIter {
    words: [u64; 4],
    word_idx: usize,
}

impl Iterator for DrainPressedIter {
    type Item = u8;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        while self.word_idx < 4 {
            let w = self.words[self.word_idx];
            if w != 0 {
                let tz = w.trailing_zeros() as u8;
                self.words[self.word_idx] &= w - 1; // clear lowest set bit
                return Some((self.word_idx as u8) * 64 + tz);
            }
            self.word_idx += 1;
        }
        None
    }
}

/// Zero-allocation, deterministic Hardware Panic Escape Hatch evaluator.
///
/// Runs inline inside both the Windows low-level hook (`LowLevelKeyboardProc`) and
/// the Debian Linux `evdev` non-blocking capture thread.
///
/// Triggers immediately when EITHER:
/// 1. `Ctrl + Alt + Shift + Backspace` is depressed simultaneously.
/// 2. `Escape` is pressed (distinct `KEY_DOWN` edges, ignoring auto-repeat) 5 times within 1.5 seconds.
#[derive(Debug, Clone, Copy)]
pub struct PanicDetector {
    depressed: DepressedKeyBitmap,
    escape_timestamps_ns: [u64; PANIC_ESCAPE_REQUIRED_PRESSES],
    escape_write_idx: usize,
    escape_count: usize,
}

impl Default for PanicDetector {
    fn default() -> Self {
        Self::new()
    }
}

impl PanicDetector {
    pub const fn new() -> Self {
        Self {
            depressed: DepressedKeyBitmap::new(),
            escape_timestamps_ns: [0u64; PANIC_ESCAPE_REQUIRED_PRESSES],
            escape_write_idx: 0,
            escape_count: 0,
        }
    }

    #[inline]
    pub const fn reset(&mut self) {
        self.depressed.clear();
        self.escape_timestamps_ns = [0u64; PANIC_ESCAPE_REQUIRED_PRESSES];
        self.escape_write_idx = 0;
        self.escape_count = 0;
    }

    #[inline]
    pub const fn depressed_bitmap(&self) -> &DepressedKeyBitmap {
        &self.depressed
    }

    /// Evaluates a key transition at `now_ns` (monotonic nanoseconds).
    /// Returns `Some(PanicTriggerKind)` if the hardware panic escape hatch tripped.
    #[inline]
    pub fn observe_key(&mut self, usage: u8, pressed: bool, now_ns: u64) -> Option<PanicTriggerKind> {
        let was_pressed = self.depressed.set_pressed(usage, pressed);

        if !pressed {
            return None;
        }

        // 1. Check Ctrl + Alt + Shift + Backspace chord
        let ctrl_held = self.depressed.is_pressed(HID_USAGE_LEFT_CTRL)
            || self.depressed.is_pressed(HID_USAGE_RIGHT_CTRL);
        let alt_held = self.depressed.is_pressed(HID_USAGE_LEFT_ALT)
            || self.depressed.is_pressed(HID_USAGE_RIGHT_ALT);
        let shift_held = self.depressed.is_pressed(HID_USAGE_LEFT_SHIFT)
            || self.depressed.is_pressed(HID_USAGE_RIGHT_SHIFT);
        let backspace_held = self.depressed.is_pressed(HID_USAGE_BACKSPACE);

        if ctrl_held && alt_held && shift_held && backspace_held {
            self.reset();
            return Some(PanicTriggerKind::ChordCtrlAltShiftBackspace);
        }

        // 2. Check 5x distinct Escape KEY_DOWN transitions within 1.5s (ignore key auto-repeat)
        if usage == HID_USAGE_ESCAPE && !was_pressed {
            self.escape_timestamps_ns[self.escape_write_idx] = now_ns;
            self.escape_write_idx = (self.escape_write_idx + 1) % PANIC_ESCAPE_REQUIRED_PRESSES;
            if self.escape_count < PANIC_ESCAPE_REQUIRED_PRESSES {
                self.escape_count += 1;
            }

            if self.escape_count == PANIC_ESCAPE_REQUIRED_PRESSES {
                // Oldest timestamp in the circular buffer is now at `self.escape_write_idx`
                let oldest_ns = self.escape_timestamps_ns[self.escape_write_idx];
                if now_ns.saturating_sub(oldest_ns) <= PANIC_ESCAPE_WINDOW_NS {
                    self.reset();
                    return Some(PanicTriggerKind::RapidEscapeFiveTimes);
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chord_panic_detector() {
        let mut detector = PanicDetector::new();
        assert_eq!(detector.observe_key(HID_USAGE_LEFT_CTRL, true, 100), None);
        assert_eq!(detector.observe_key(HID_USAGE_RIGHT_ALT, true, 200), None);
        assert_eq!(detector.observe_key(HID_USAGE_LEFT_SHIFT, true, 300), None);
        assert_eq!(
            detector.observe_key(HID_USAGE_BACKSPACE, true, 400),
            Some(PanicTriggerKind::ChordCtrlAltShiftBackspace)
        );
    }

    #[test]
    fn test_rapid_escape_panic_detector() {
        let mut detector = PanicDetector::new();
        let t0 = 1_000_000_000u64;
        for i in 0..4 {
            let t = t0 + i * 250_000_000;
            assert_eq!(detector.observe_key(HID_USAGE_ESCAPE, true, t), None);
            // Auto-repeat while held must not count as a distinct press
            assert_eq!(detector.observe_key(HID_USAGE_ESCAPE, true, t + 10_000_000), None);
            assert_eq!(detector.observe_key(HID_USAGE_ESCAPE, false, t + 50_000_000), None);
        }
        // 5th press at t0 + 1.0s (within 1.5s window)
        assert_eq!(
            detector.observe_key(HID_USAGE_ESCAPE, true, t0 + 1_000_000_000),
            Some(PanicTriggerKind::RapidEscapeFiveTimes)
        );
    }

    #[test]
    fn test_depressed_key_bitmap_drain() {
        let mut bmp = DepressedKeyBitmap::new();
        bmp.set_pressed(0x04, true); // 'A'
        bmp.set_pressed(HID_USAGE_LEFT_CTRL, true);
        bmp.set_pressed(HID_USAGE_RIGHT_GUI, true);

        let drained: Vec<u8> = bmp.drain_pressed().collect();
        assert_eq!(drained, vec![0x04, HID_USAGE_LEFT_CTRL, HID_USAGE_RIGHT_GUI]);
        assert_eq!(bmp.drain_pressed().count(), 0);
    }
}
