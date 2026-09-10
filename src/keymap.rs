//! Linux input codes to the scan codes QEMU's extended VNC key events carry ("qnum": set 1,
//! with the 0xE0 prefix folded into the high bit). Values from keymap-gen's keymaps.csv.

pub const KEY_BACKSPACE: u32 = 14;
pub const KEY_LEFTCTRL: u32 = 29;
pub const KEY_LEFTALT: u32 = 56;
pub const KEY_F1: u32 = 59;
pub const KEY_F2: u32 = 60;
pub const KEY_F7: u32 = 65;
pub const KEY_DELETE: u32 = 111;

/// The scan code for evdev code `code`, or 0 where there is none; with 0, gvnc sends the
/// keysym alone.
pub fn qnum(code: u32) -> u16 {
    match code {
        // Escape through keypad `.` share their numbers with set 1.
        1..=83 => code as u16,
        85 => 0x76,
        86 => 0x56,
        87 => 0x57,
        88 => 0x58,
        89 => 0x73,
        90 => 0x78,
        91 => 0x77,
        92 => 0x79,
        93 => 0x70,
        94 => 0x7b,
        95 => 0x5c,
        96 => 0x9c,
        97 => 0x9d,
        98 => 0xb5,
        99 => 0x54,
        100 => 0xb8,
        102 => 0xc7,
        103 => 0xc8,
        104 => 0xc9,
        105 => 0xcb,
        106 => 0xcd,
        107 => 0xcf,
        108 => 0xd0,
        109 => 0xd1,
        110 => 0xd2,
        111 => 0xd3,
        113 => 0xa0,
        114 => 0xae,
        115 => 0xb0,
        116 => 0xde,
        117 => 0x59,
        119 => 0xc6,
        121 => 0x7e,
        122 => 0xf2,
        123 => 0xf1,
        124 => 0x7d,
        125 => 0xdb,
        126 => 0xdc,
        127 => 0xdd,
        142 => 0xdf,
        143 => 0xe3,
        163 => 0x99,
        164 => 0xa2,
        165 => 0x90,
        166 => 0xa4,
        183 => 0x5d,
        184 => 0x5e,
        185 => 0x5f,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_keys_carry_the_prefix_bit() {
        assert_eq!(qnum(KEY_DELETE), 0xd3);
        assert_eq!(qnum(97), 0x9d, "right Ctrl is E0 1D");
        assert_eq!(qnum(KEY_LEFTCTRL), 0x1d);
        assert_eq!(qnum(0), 0);
        assert_eq!(qnum(250), 0);
    }
}
