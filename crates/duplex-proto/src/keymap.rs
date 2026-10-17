//! Universal Keycode Normalization Tables (USB HID Usage Page `0x07`).
//!
//! Normalizes both Linux `evdev` (`KEY_*`) codes and Windows Set 1 Hardware ScanCodes
//! (+ Extended flag) into canonical USB HID Usage Page `0x07` (`u8`) IDs for wire transmission.

/// Canonical mapping triple: `(usb_hid_usage, linux_evdev_code, (win_scancode_set1, win_is_extended))`.
pub const KEYMAP_TABLE: &[(u8, u16, (u16, bool))] = &[
    // Letters A-Z (HID 0x04..=0x1D)
    (0x04, 30, (0x1E, false)), // A
    (0x05, 48, (0x30, false)), // B
    (0x06, 46, (0x2E, false)), // C
    (0x07, 32, (0x20, false)), // D
    (0x08, 18, (0x12, false)), // E
    (0x09, 33, (0x21, false)), // F
    (0x0A, 34, (0x22, false)), // G
    (0x0B, 35, (0x23, false)), // H
    (0x0C, 23, (0x17, false)), // I
    (0x0D, 36, (0x24, false)), // J
    (0x0E, 37, (0x25, false)), // K
    (0x0F, 38, (0x26, false)), // L
    (0x10, 50, (0x32, false)), // M
    (0x11, 49, (0x31, false)), // N
    (0x12, 24, (0x18, false)), // O
    (0x13, 25, (0x19, false)), // P
    (0x14, 16, (0x10, false)), // Q
    (0x15, 19, (0x13, false)), // R
    (0x16, 31, (0x1F, false)), // S
    (0x17, 20, (0x14, false)), // T
    (0x18, 22, (0x16, false)), // U
    (0x19, 47, (0x2F, false)), // V
    (0x1A, 17, (0x11, false)), // W
    (0x1B, 45, (0x2D, false)), // X
    (0x1C, 21, (0x15, false)), // Y
    (0x1D, 44, (0x2C, false)), // Z

    // Number row 1-9, 0 (HID 0x1E..=0x27)
    (0x1E, 2, (0x02, false)),  // 1
    (0x1F, 3, (0x03, false)),  // 2
    (0x20, 4, (0x04, false)),  // 3
    (0x21, 5, (0x05, false)),  // 4
    (0x22, 6, (0x06, false)),  // 5
    (0x23, 7, (0x07, false)),  // 6
    (0x24, 8, (0x08, false)),  // 7
    (0x25, 9, (0x09, false)),  // 8
    (0x26, 10, (0x0A, false)), // 9
    (0x27, 11, (0x0B, false)), // 0

    // Control & Punctuation (HID 0x28..=0x38)
    (0x28, 28, (0x1C, false)), // Enter / Return
    (0x29, 1, (0x01, false)),  // Escape
    (0x2A, 14, (0x0E, false)), // Backspace
    (0x2B, 15, (0x0F, false)), // Tab
    (0x2C, 57, (0x39, false)), // Space
    (0x2D, 12, (0x0C, false)), // Minus -
    (0x2E, 13, (0x0D, false)), // Equal =
    (0x2F, 26, (0x1A, false)), // Left Bracket [
    (0x30, 27, (0x1B, false)), // Right Bracket ]
    (0x31, 43, (0x2B, false)), // Backslash \
    (0x32, 43, (0x2B, false)), // Non-US # and ~
    (0x33, 39, (0x27, false)), // Semicolon ;
    (0x34, 40, (0x28, false)), // Apostrophe '
    (0x35, 41, (0x29, false)), // Grave / Backtick `
    (0x36, 51, (0x33, false)), // Comma ,
    (0x37, 52, (0x34, false)), // Period .
    (0x38, 53, (0x35, false)), // Slash /
    (0x39, 58, (0x3A, false)), // Caps Lock

    // Function Keys F1-F12 (HID 0x3A..=0x45)
    (0x3A, 59, (0x3B, false)), // F1
    (0x3B, 60, (0x3C, false)), // F2
    (0x3C, 61, (0x3D, false)), // F3
    (0x3D, 62, (0x3E, false)), // F4
    (0x3E, 63, (0x3F, false)), // F5
    (0x3F, 64, (0x40, false)), // F6
    (0x40, 65, (0x41, false)), // F7
    (0x41, 66, (0x42, false)), // F8
    (0x42, 67, (0x43, false)), // F9
    (0x43, 68, (0x44, false)), // F10
    (0x44, 87, (0x57, false)), // F11
    (0x45, 88, (0x58, false)), // F12

    // Navigation Cluster & System Keys (HID 0x46..=0x52)
    (0x46, 99, (0x37, true)),  // Print Screen / SysRq
    (0x47, 70, (0x46, false)), // Scroll Lock
    (0x48, 119, (0x45, false)),// Pause / Break
    (0x49, 110, (0x52, true)), // Insert
    (0x4A, 102, (0x47, true)), // Home
    (0x4B, 104, (0x49, true)), // Page Up
    (0x4C, 111, (0x53, true)), // Delete
    (0x4D, 107, (0x4F, true)), // End
    (0x4E, 109, (0x51, true)), // Page Down
    (0x4F, 106, (0x4D, true)), // Right Arrow
    (0x50, 105, (0x4B, true)), // Left Arrow
    (0x51, 108, (0x50, true)), // Down Arrow
    (0x52, 103, (0x48, true)), // Up Arrow

    // Numeric Keypad (HID 0x53..=0x63)
    (0x53, 69, (0x45, true)),  // Num Lock
    (0x54, 98, (0x35, true)),  // Keypad /
    (0x55, 55, (0x37, false)), // Keypad *
    (0x56, 74, (0x4A, false)), // Keypad -
    (0x57, 78, (0x4E, false)), // Keypad +
    (0x58, 96, (0x1C, true)),  // Keypad Enter
    (0x59, 79, (0x4F, false)), // Keypad 1
    (0x5A, 80, (0x50, false)), // Keypad 2
    (0x5B, 81, (0x51, false)), // Keypad 3
    (0x5C, 75, (0x4B, false)), // Keypad 4
    (0x5D, 76, (0x4C, false)), // Keypad 5
    (0x5E, 77, (0x4D, false)), // Keypad 6
    (0x5F, 71, (0x47, false)), // Keypad 7
    (0x60, 72, (0x48, false)), // Keypad 8
    (0x61, 73, (0x49, false)), // Keypad 9
    (0x62, 82, (0x52, false)), // Keypad 0
    (0x63, 83, (0x53, false)), // Keypad .
    (0x64, 86, (0x56, false)), // Non-US \ and | (ISO 102nd key)
    (0x65, 127, (0x5D, true)), // Application / Compose / Menu key

    // Modifiers (HID 0xE0..=0xE7)
    (0xE0, 29, (0x1D, false)),  // Left Control
    (0xE1, 42, (0x2A, false)),  // Left Shift
    (0xE2, 56, (0x38, false)),  // Left Alt
    (0xE3, 125, (0x5B, true)),  // Left GUI (Super / Windows)
    (0xE4, 97, (0x1D, true)),   // Right Control
    (0xE5, 54, (0x36, false)),  // Right Shift
    (0xE6, 100, (0x38, true)),  // Right Alt (AltGr)
    (0xE7, 126, (0x5C, true)),  // Right GUI (Super / Windows)
];

/// Translates a Linux `evdev` `KEY_*` code (`u16`) into a canonical USB HID Usage ID (`0x07` page).
#[inline]
pub fn linux_evdev_to_hid(evdev_code: u16) -> Option<u8> {
    for &(hid, evdev, _) in KEYMAP_TABLE {
        if evdev == evdev_code {
            return Some(hid);
        }
    }
    None
}

/// Translates a canonical USB HID Usage ID (`0x07` page) into a Linux `evdev` `KEY_*` code (`u16`).
#[inline]
pub fn hid_to_linux_evdev(hid_usage: u8) -> Option<u16> {
    for &(hid, evdev, _) in KEYMAP_TABLE {
        if hid == hid_usage {
            return Some(evdev);
        }
    }
    None
}

/// Translates a Windows Set 1 hardware scan code and extended-key flag into a canonical USB HID Usage ID.
#[inline]
pub fn windows_scancode_to_hid(scancode: u16, is_extended: bool) -> Option<u8> {
    // Handle Right Shift which some Windows keyboards report with LLKHF_EXTENDED=true on 0x36
    if scancode == 0x36 {
        return Some(0xE5);
    }
    for &(hid, _, (win_sc, win_ext)) in KEYMAP_TABLE {
        if win_sc == scancode && win_ext == is_extended {
            return Some(hid);
        }
    }
    None
}

/// Translates a canonical USB HID Usage ID into a Windows Set 1 hardware scan code and extended flag.
#[inline]
pub fn hid_to_windows_scancode(hid_usage: u8) -> Option<(u16, bool)> {
    for &(hid, _, win_pair) in KEYMAP_TABLE {
        if hid == hid_usage {
            return Some(win_pair);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hid::*;

    #[test]
    fn test_modifier_roundtrip_linux_and_windows() {
        for usage in ALL_HID_MODIFIER_USAGES {
            let evdev = hid_to_linux_evdev(usage).expect("evdev mapping must exist");
            assert_eq!(linux_evdev_to_hid(evdev), Some(usage));

            let (sc, ext) = hid_to_windows_scancode(usage).expect("win scancode must exist");
            assert_eq!(windows_scancode_to_hid(sc, ext), Some(usage));
        }
    }

    #[test]
    fn test_escape_and_backspace_mappings() {
        assert_eq!(linux_evdev_to_hid(1), Some(HID_USAGE_ESCAPE));
        assert_eq!(windows_scancode_to_hid(0x01, false), Some(HID_USAGE_ESCAPE));
        assert_eq!(linux_evdev_to_hid(14), Some(HID_USAGE_BACKSPACE));
        assert_eq!(windows_scancode_to_hid(0x0E, false), Some(HID_USAGE_BACKSPACE));
    }
}
