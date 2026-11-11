//! X11 keysym (the shared wire namespace) to macOS virtual key codes (US layout).

/// Modifier bit masks for CGEventFlags.
pub const FLAG_CAPS: u64 = 0x0001_0000;
pub const FLAG_SHIFT: u64 = 0x0002_0000;
pub const FLAG_CONTROL: u64 = 0x0004_0000;
pub const FLAG_OPTION: u64 = 0x0008_0000;
pub const FLAG_COMMAND: u64 = 0x0010_0000;

/// Returns the modifier flag a keysym toggles, if it is a modifier.
pub fn modifier_flag(keysym: u32) -> Option<u64> {
    Some(match keysym {
        0xffe1 | 0xffe2 => FLAG_SHIFT,
        0xffe3 | 0xffe4 => FLAG_CONTROL,
        0xffe5 => FLAG_CAPS,
        0xffe9 | 0xffea | 0xfe03 => FLAG_OPTION,
        0xffeb | 0xffec | 0xffe7 | 0xffe8 => FLAG_COMMAND,
        _ => return None,
    })
}

const ASCII_ROWS: &[(&str, u16)] = &[
    ("aA", 0x00),
    ("sS", 0x01),
    ("dD", 0x02),
    ("fF", 0x03),
    ("hH", 0x04),
    ("gG", 0x05),
    ("zZ", 0x06),
    ("xX", 0x07),
    ("cC", 0x08),
    ("vV", 0x09),
    ("bB", 0x0B),
    ("qQ", 0x0C),
    ("wW", 0x0D),
    ("eE", 0x0E),
    ("rR", 0x0F),
    ("yY", 0x10),
    ("tT", 0x11),
    ("1!", 0x12),
    ("2@", 0x13),
    ("3#", 0x14),
    ("4$", 0x15),
    ("6^", 0x16),
    ("5%", 0x17),
    ("=+", 0x18),
    ("9(", 0x19),
    ("7&", 0x1A),
    ("-_", 0x1B),
    ("8*", 0x1C),
    ("0)", 0x1D),
    ("]}", 0x1E),
    ("oO", 0x1F),
    ("uU", 0x20),
    ("[{", 0x21),
    ("iI", 0x22),
    ("pP", 0x23),
    ("lL", 0x25),
    ("jJ", 0x26),
    ("'\"", 0x27),
    ("kK", 0x28),
    (";:", 0x29),
    ("\\|", 0x2A),
    (",<", 0x2B),
    ("/?", 0x2C),
    ("nN", 0x2D),
    ("mM", 0x2E),
    (".>", 0x2F),
    ("  ", 0x31),
    ("`~", 0x32),
];

/// Virtual key code for a keysym, or `None` when it needs Unicode injection.
pub fn keycode(keysym: u32) -> Option<u16> {
    if (0x20..=0x7e).contains(&keysym) {
        let ch = char::from_u32(keysym)?;
        return ASCII_ROWS
            .iter()
            .find(|(chars, _)| chars.contains(ch))
            .map(|(_, code)| *code);
    }
    Some(match keysym {
        0xff0d | 0xff8d => 0x24, // Return / KP_Enter
        0xff09 => 0x30,          // Tab
        0xff08 => 0x33,          // BackSpace
        0xff1b => 0x35,          // Escape
        0xffff => 0x75,          // Delete (forward)
        0xff50 => 0x73,          // Home
        0xff57 => 0x77,          // End
        0xff55 => 0x74,          // Page_Up
        0xff56 => 0x79,          // Page_Down
        0xff51 => 0x7B,          // Left
        0xff52 => 0x7E,          // Up
        0xff53 => 0x7C,          // Right
        0xff54 => 0x7D,          // Down
        0xffbe => 0x7A,          // F1
        0xffbf => 0x78,
        0xffc0 => 0x63,
        0xffc1 => 0x76,
        0xffc2 => 0x60,
        0xffc3 => 0x61,
        0xffc4 => 0x62,
        0xffc5 => 0x64,
        0xffc6 => 0x65,
        0xffc7 => 0x6D,
        0xffc8 => 0x67,
        0xffc9 => 0x6F, // F12
        0xffe1 => 0x38, // Shift_L
        0xffe2 => 0x3C,
        0xffe3 => 0x3B, // Control_L
        0xffe4 => 0x3E,
        0xffe5 => 0x39,          // Caps_Lock
        0xffe9 | 0xffe7 => 0x3A, // Alt_L / Meta_L -> Option
        0xffea | 0xffe8 => 0x3D,
        0xffeb => 0x37, // Super_L -> Command
        0xffec => 0x36,
        _ => return None,
    })
}

/// Unicode scalar for keysyms outside the US-layout table (Latin-1 and the
/// `0x01000000 + codepoint` range).
pub fn unicode(keysym: u32) -> Option<char> {
    match keysym {
        0xa0..=0xff => char::from_u32(keysym),
        0x0100_0000..=0x0110_ffff => char::from_u32(keysym - 0x0100_0000),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn letters_and_shifted_symbols_share_a_physical_key() {
        assert_eq!(keycode('a' as u32), Some(0x00));
        assert_eq!(keycode('A' as u32), Some(0x00));
        assert_eq!(keycode('1' as u32), keycode('!' as u32));
        assert_eq!(keycode(' ' as u32), Some(0x31));
    }
    #[test]
    fn navigation_and_modifiers_map() {
        assert_eq!(keycode(0xff0d), Some(0x24));
        assert_eq!(keycode(0xff51), Some(0x7B));
        assert_eq!(keycode(0xffeb), Some(0x37));
        assert_eq!(modifier_flag(0xffe1), Some(FLAG_SHIFT));
        assert_eq!(modifier_flag(0x61), None);
    }
    #[test]
    fn non_ascii_falls_back_to_unicode() {
        assert_eq!(keycode(0xe9), None);
        assert_eq!(unicode(0xe9), Some('é'));
        assert_eq!(unicode(0x0100_1ea1), Some('ạ'));
        assert_eq!(unicode(0xff0d), None);
    }
    #[test]
    fn every_ascii_printable_has_a_key() {
        for c in 0x20u32..=0x7e {
            assert!(keycode(c).is_some(), "missing {c:#x}");
        }
    }
}
