//! Checks the decoder against the EdgeTX firmware source itself.
//!
//! Needs an EdgeTX checkout at `ref/edgetx` (or `EDGETX_SRC`) and clang++/g++ (or `CXX`):
//!   git clone --depth 1 https://github.com/EdgeTX/edgetx.git ref/edgetx
//! Tests print SKIPPED and pass when either is missing.

mod common;

use common::{
    Encoder, Rng, between, edgetx_src, expected_channels, harness_exe, read, source_descriptor,
};
use pocket_overlay::edgetx;
use pocket_overlay::hid::Layout;

#[test]
fn embedded_descriptor_is_edgetx_classic() {
    let Some(src) = edgetx_src() else { return };
    assert_eq!(source_descriptor(&src), edgetx::CLASSIC_DESCRIPTOR);

    let layout = Layout::parse(&source_descriptor(&src)).unwrap();
    assert!(!layout.uses_report_ids);
    let values: Vec<_> = layout.fields.iter().filter(|f| !f.padding).collect();
    assert_eq!(values.len(), 2, "{values:#?}");

    let buttons = values[0];
    assert_eq!(buttons.usage_page, 0x09);
    assert_eq!(buttons.usages, (1..=24).collect::<Vec<_>>());
    assert_eq!(
        (buttons.bit_offset, buttons.size, buttons.count),
        (0, 1, 24)
    );

    let axes = values[1];
    assert_eq!(axes.usage_page, 0x01);
    assert_eq!(
        axes.usages,
        (0x30..=0x37).collect::<Vec<_>>(),
        "X Y Z Rx Ry Rz Slider Dial"
    );
    assert_eq!((axes.bit_offset, axes.size, axes.count), (24, 16, 8));
    assert_eq!(axes.logical, (0, 2048));

    assert_eq!(layout.report_len(), edgetx::REPORT_LEN);
    let conf = read(&src, "targets/common/arm/stm32/usbd_conf.h");
    let packet: usize = between(&conf, "#define HID_IN_PACKET", "\n")
        .trim()
        .parse()
        .unwrap();
    assert_eq!(packet, edgetx::REPORT_LEN, "HID_IN_PACKET");
}

#[test]
fn usb_ids_and_name() {
    let Some(src) = edgetx_src() else { return };
    let desc = read(&src, "targets/common/arm/stm32/usbd_desc.c");
    let hex = |s: &str| u16::from_str_radix(s.trim().trim_start_matches("0x"), 16).unwrap();
    assert_eq!(
        hex(between(&desc, "#define USBD_VID_PID_CODES", "//")),
        edgetx::USB_VID
    );
    assert_eq!(
        hex(between(&desc, "#define USBD_HID_PID", "//")),
        edgetx::USB_PID
    );
    let product = between(&desc, "#define USBD_HID_PRODUCT_FS_STRING", "\n").trim();
    assert_eq!(product, r#"USB_NAME " Joystick""#);

    let names = read(&src, "targets/taranis/usb_descriptor.h");
    let pocket = between(&names, "defined(RADIO_POCKET)", "#elif");
    let name = between(pocket, "\"", "\"");
    assert_eq!(format!("{name} Joystick"), edgetx::POCKET_PRODUCT);
}

#[test]
fn pocket_uses_classic_report() {
    let Some(src) = edgetx_src() else { return };
    // USBJ_EX (configurable reports) is forced off for STM32F407xE, which the Pocket uses.
    let taranis = read(&src, "targets/taranis/CMakeLists.txt");
    let pocket = between(&taranis, "elseif(PCBREV STREQUAL POCKET)", "elseif(");
    assert!(
        pocket.contains("set(CPU_TYPE_FULL STM32F407xE)"),
        "{pocket}"
    );
    let f407 = between(
        &taranis,
        "STM32F407xE) OR (CPU_TYPE_FULL STREQUAL STM32F407xG))",
        "elseif",
    );
    let xe_branch = between(f407, "else()", "endif()");
    assert!(xe_branch.contains("set(USBJ_EX OFF)"), "{f407}");
}

#[test]
fn pocket_controls_match_hardware_definition() {
    let Some(src) = edgetx_src() else { return };
    let hw: serde_json::Value =
        serde_json::from_str(&read(&src, "boards/hw_defs/pocket.json")).unwrap();
    let switches: Vec<(String, String, String)> = hw["switches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["name"].as_str().unwrap().into(),
                s["type"].as_str().unwrap().into(),
                s["default"].as_str().unwrap().into(),
            )
        })
        .collect();
    let expect = |n: &str, t: &str, d: &str| (n.to_owned(), t.to_owned(), d.to_owned());
    assert_eq!(
        switches,
        [
            expect("SA", "2POS", "2POS"),
            expect("SB", "3POS", "3POS"),
            expect("SC", "3POS", "3POS"),
            expect("SD", "2POS", "2POS"),
            expect("SE", "2POS", "TOGGLE"), // momentary
        ]
    );
    let inputs = hw["adc_inputs"]["inputs"].as_array().unwrap();
    let sticks: Vec<&str> = inputs
        .iter()
        .filter(|i| i["type"] == "STICK")
        .map(|i| i["name"].as_str().unwrap())
        .collect();
    assert_eq!(sticks, ["LH", "LV", "RV", "RH"]);
    let pots: Vec<&str> = inputs
        .iter()
        .filter(|i| i["type"] == "FLEX")
        .map(|i| i["label"].as_str().unwrap())
        .collect();
    assert_eq!(pots, ["S1"]);
}

/// Deterministic vectors: edge cases plus pseudo-random values beyond +-1024 to cover clamping.
fn vectors() -> Vec<[i16; 32]> {
    let mut v = vec![
        [0; 32],
        [1024; 32],
        [-1024; 32],
        [1; 32],
        [-1; 32],
        [2000; 32],
        [-2000; 32],
    ];
    let mut ramp = [0i16; 32];
    for (i, c) in ramp.iter_mut().enumerate() {
        *c = (i as i16 - 16) * 70;
    }
    v.push(ramp);
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..500 {
        let mut ch = [0i16; 32];
        for c in &mut ch {
            *c = rng.below(3001) as i16 - 1500;
        }
        v.push(ch);
    }
    v
}

#[test]
fn rust_encoder_mirror_matches_firmware_bytes() {
    if harness_exe().is_none() {
        return;
    }
    let mut firmware = Encoder::best();
    for ch in vectors() {
        assert_eq!(
            firmware.encode(&ch),
            edgetx::encode_classic(&ch).to_vec(),
            "channels {ch:?}"
        );
    }
}

#[test]
fn decodes_edgetx_encoder_output() {
    let Some(src) = edgetx_src() else { return };
    if harness_exe().is_none() {
        return;
    }
    let layout = Layout::parse(&source_descriptor(&src)).unwrap();
    let mut firmware = Encoder::best();
    for ch in vectors() {
        let bytes = firmware.encode(&ch);
        let report = layout
            .decode(&bytes)
            .unwrap_or_else(|| panic!("undecodable {bytes:02x?}"));
        let got: Vec<i16> = (1..=32).map(|n| report.channel(n).unwrap()).collect();
        assert_eq!(got, expected_channels(&ch), "sent {ch:?}");
    }
}
