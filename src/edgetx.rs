//! What EdgeTX sends over USB in joystick mode.
//!
//! Source of truth: `radio/src/targets/common/arm/stm32/usbd_hid_joystick.c`
//! (`HID_JOYSTICK_ReportDesc`) and `usbJoystickUpdate()` in
//! `radio/src/targets/common/arm/stm32/usb_driver.cpp`. The Pocket is an STM32F407xE
//! target, which EdgeTX builds with `USBJ_EX OFF`, so this fixed "classic" layout is the
//! only one it can send. `tests/edgetx_conformance.rs` checks everything here against
//! that source.
//!
//! ```text
//! byte 0..3   buttons: bit n of byte b = channel 9 + 8b + n is > 0
//! byte 3..19  8 x u16 LE axes (X Y Z Rx Ry Rz Slider Dial) = clamp(ch + 1024, 0, 2048)
//! ```

use crate::hid::Layout;

/// pid.codes VID / "OpenTX" PID shared by every EdgeTX radio's HID joystick (`usbd_desc.c`).
pub const USB_VID: u16 = 0x1209;
pub const USB_PID: u16 = 0x4F54;
/// `USB_NAME " Joystick"` for `RADIO_POCKET` (`targets/taranis/usb_descriptor.h`).
pub const POCKET_PRODUCT: &str = "Radiomaster Pocket Joystick";

/// `HID_IN_PACKET` in `usbd_conf.h`.
pub const REPORT_LEN: usize = 19;
pub const AXES: usize = 8;
pub const BUTTONS: usize = 24;
pub const CHANNELS: usize = AXES + BUTTONS;

/// `HID_JOYSTICK_ReportDesc`, byte for byte.
pub const CLASSIC_DESCRIPTOR: [u8; 56] = [
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0xa1, 0x00, // Generic Desktop, Game Pad, collections
    0x05, 0x09, 0x19, 0x01, 0x29, 0x18, 0x15, 0x00, 0x25, 0x01, // Buttons 1..24, 0..1
    0x95, 0x18, 0x75, 0x01, 0x81, 0x02, // 24 x 1 bit
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x32, 0x09, 0x33, // X Y Z Rx
    0x09, 0x34, 0x09, 0x35, 0x09, 0x36, 0x09, 0x37, // Ry Rz Slider Dial
    0x16, 0x00, 0x00, 0x26, 0x00, 0x08, // 0..2048
    0x75, 0x10, 0x95, 0x08, 0x81, 0x02, // 8 x 16 bit
    0xc0, 0xc0,
];

pub fn classic_layout() -> Layout {
    static LAYOUT: std::sync::OnceLock<Layout> = std::sync::OnceLock::new();
    LAYOUT
        .get_or_init(|| Layout::parse(&CLASSIC_DESCRIPTOR).expect("classic descriptor parses"))
        .clone()
}

/// Rust mirror of EdgeTX's classic `usbJoystickUpdate()`: 32 channel outputs to report
/// bytes. Used by demo mode and as a fallback encoder in tests; the conformance test
/// checks it matches the firmware's output byte for byte.
pub fn encode_classic(ch: &[i16; CHANNELS]) -> [u8; REPORT_LEN] {
    let mut buf = [0u8; REPORT_LEN];
    for i in 0..8 {
        for (byte, base) in [(0, 8), (1, 16), (2, 24)] {
            if ch[i + base] > 0 {
                buf[byte] |= 1 << i;
            }
        }
    }
    for i in 0..AXES {
        let v = (i32::from(ch[i]) + 1024).clamp(0, 2048) as u16;
        buf[3 + i * 2..5 + i * 2].copy_from_slice(&v.to_le_bytes());
    }
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_layout_shape() {
        let l = classic_layout();
        assert_eq!(
            (l.axis_count(), l.button_count(), l.report_len()),
            (AXES, BUTTONS, REPORT_LEN)
        );
        assert!(!l.uses_report_ids);
    }

    #[test]
    fn round_trips_through_classic_layout() {
        let mut ch = [0i16; CHANNELS];
        ch[..8].copy_from_slice(&[0, -1024, 1024, 512, -1, 1, 2000, -2000]);
        ch[8] = 1; // CH9
        ch[10] = 1024; // CH11
        ch[31] = 5; // CH32
        ch[9] = 0; // CH10: 0 is not > 0
        let r = classic_layout().decode(&encode_classic(&ch)).unwrap();
        assert_eq!(r.axes, [0, -1024, 1024, 512, -1, 1, 1024, -1024]);
        let on: Vec<usize> = (9..=32).filter(|&c| r.channel(c) == Some(1024)).collect();
        assert_eq!(on, [9, 11, 32]);
    }

    #[test]
    fn tolerates_windows_report_id_prefix() {
        let mut bytes = vec![0u8];
        bytes.extend(encode_classic(&[0; CHANNELS]));
        assert_eq!(classic_layout().decode(&bytes).unwrap().axes, [0; 8]);
        assert!(classic_layout().decode(&bytes[..10]).is_none());
    }
}
