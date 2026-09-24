//! Minimal HID report-descriptor parser and report decoder.
//!
//! Enough of the HID spec to lay out the Input items of a joystick: usage pages, usages,
//! logical ranges, report size/count, report IDs and constant padding. Used to decode
//! whatever layout the radio actually announces instead of assuming one.

use std::fmt;

/// One Input item: `count` values of `size` bits each, starting at `bit_offset`
/// (counted after the report-ID byte, if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub report_id: u8,
    pub usage_page: u32,
    pub usages: Vec<u32>,
    pub bit_offset: u32,
    pub size: u32,
    pub count: u32,
    pub logical: (i32, i32),
    /// Constant (padding) or array items carry no per-control values.
    pub padding: bool,
}

impl Field {
    /// Buttons are 1-bit values or anything on the Button usage page.
    pub fn is_button(&self) -> bool {
        self.usage_page == 0x09 || self.size == 1
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub fields: Vec<Field>,
    pub uses_report_ids: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Truncated,
    NoInputs,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated => f.write_str("report descriptor ends mid-item"),
            Self::NoInputs => f.write_str("report descriptor has no input values"),
        }
    }
}

impl std::error::Error for ParseError {}

/// A decoded report: analog values normalised to EdgeTX units (-1024..=1024) and buttons,
/// both in descriptor order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub axes: Vec<i16>,
    pub buttons: Vec<bool>,
}

impl Report {
    /// 1-based input channel: axes first, then buttons (for EdgeTX classic this is exactly
    /// CH1-8 then CH9-32). Buttons read as -1024 / +1024.
    pub fn channel(&self, ch: usize) -> Option<i16> {
        let i = ch.checked_sub(1)?;
        if let Some(v) = self.axes.get(i) {
            return Some(*v);
        }
        self.buttons
            .get(i - self.axes.len())
            .map(|on| if *on { 1024 } else { -1024 })
    }

    pub fn channel_count(&self) -> usize {
        self.axes.len() + self.buttons.len()
    }
}

impl Layout {
    pub fn parse(desc: &[u8]) -> Result<Self, ParseError> {
        let (mut page, mut lmin, mut lmax, mut size, mut count, mut report_id) =
            (0, 0, 0, 0, 0, 0u8);
        let mut usages = Vec::new();
        let (mut umin, mut umax) = (None, None);
        let mut uses_report_ids = false;
        // bit cursor per report ID
        let mut cursors = std::collections::HashMap::<u8, u32>::new();
        let mut fields = Vec::new();

        let mut i = 0;
        while i < desc.len() {
            let prefix = desc[i];
            if prefix == 0xFE {
                // long item: skip
                let len = *desc.get(i + 1).ok_or(ParseError::Truncated)? as usize;
                i += 3 + len;
                continue;
            }
            let len = [0, 1, 2, 4][(prefix & 3) as usize];
            let data = desc.get(i + 1..i + 1 + len).ok_or(ParseError::Truncated)?;
            let unsigned = data
                .iter()
                .rev()
                .fold(0u32, |acc, b| acc << 8 | u32::from(*b));
            let signed = match len {
                0 => 0,
                1 => i32::from(data[0] as i8),
                2 => i32::from(i16::from_le_bytes([data[0], data[1]])),
                _ => unsigned as i32,
            };
            match prefix & 0xFC {
                0x04 => page = unsigned,
                0x14 => lmin = signed,
                0x24 => lmax = signed,
                0x74 => size = unsigned,
                0x94 => count = unsigned,
                0x84 => {
                    uses_report_ids = true;
                    report_id = unsigned as u8;
                }
                0x08 => usages.push(unsigned),
                0x18 => umin = Some(unsigned),
                0x28 => umax = Some(unsigned),
                0x80 => {
                    // Input: bit 0 = constant, bit 1 = variable (vs array)
                    if let (Some(a), Some(b)) = (umin, umax) {
                        usages.extend(a..=b);
                    }
                    let cursor = cursors.entry(report_id).or_insert(0);
                    fields.push(Field {
                        report_id,
                        usage_page: page,
                        usages: std::mem::take(&mut usages),
                        bit_offset: *cursor,
                        size,
                        count,
                        logical: (lmin, lmax),
                        padding: unsigned & 1 == 1 || unsigned & 2 == 0,
                    });
                    *cursor += size * count;
                    (umin, umax) = (None, None);
                }
                // Output / Feature / Collection / End Collection clear local usages
                0x90 | 0xB0 | 0xA0 | 0xC0 => {
                    usages.clear();
                    (umin, umax) = (None, None);
                }
                _ => {}
            }
            i += 1 + len;
        }

        if !fields.iter().any(|f| !f.padding && f.count > 0) {
            return Err(ParseError::NoInputs);
        }
        Ok(Self {
            fields,
            uses_report_ids,
        })
    }

    /// Report ID of the first input report (0 when IDs aren't used).
    fn primary_report_id(&self) -> u8 {
        self.fields.first().map_or(0, |f| f.report_id)
    }

    /// Payload length in bytes (excluding the report-ID byte) of the primary input report.
    pub fn report_len(&self) -> usize {
        let id = self.primary_report_id();
        let bits = self
            .fields
            .iter()
            .filter(|f| f.report_id == id)
            .map(|f| f.size * f.count)
            .sum::<u32>();
        bits.div_ceil(8) as usize
    }

    pub fn axis_count(&self) -> usize {
        self.values()
            .filter(|f| !f.is_button())
            .map(|f| f.count as usize)
            .sum()
    }

    pub fn button_count(&self) -> usize {
        self.values()
            .filter(|f| f.is_button())
            .map(|f| f.count as usize)
            .sum()
    }

    fn values(&self) -> impl Iterator<Item = &Field> {
        let id = self.primary_report_id();
        self.fields
            .iter()
            .filter(move |f| f.report_id == id && !f.padding)
    }

    /// Decodes one input report. Accepts an extra leading zero byte (Windows prepends the
    /// report ID even when a device doesn't use them).
    pub fn decode(&self, data: &[u8]) -> Option<Report> {
        let len = self.report_len();
        let payload = if self.uses_report_ids {
            let (id, rest) = data.split_first()?;
            if *id != self.primary_report_id() {
                return None;
            }
            rest
        } else if data.len() == len + 1 && data[0] == 0 {
            &data[1..]
        } else {
            data
        };
        if payload.len() != len {
            return None;
        }

        let mut report = Report::default();
        for f in self.values() {
            for n in 0..f.count {
                let raw = extract(payload, f.bit_offset + n * f.size, f.size, f.logical.0 < 0);
                if f.is_button() {
                    report.buttons.push(raw != 0);
                } else {
                    report.axes.push(normalise(raw, f.logical));
                }
            }
        }
        Some(report)
    }
}

/// Little-endian bit extraction, optionally sign-extended.
fn extract(data: &[u8], offset: u32, size: u32, signed: bool) -> i64 {
    let mut v: u64 = 0;
    for b in 0..size {
        let bit = offset + b;
        let byte = data[(bit / 8) as usize];
        if byte >> (bit % 8) & 1 == 1 {
            v |= 1 << b;
        }
    }
    if signed && size > 0 && size < 64 && v >> (size - 1) & 1 == 1 {
        (v | (!0u64 << size)) as i64
    } else {
        v as i64
    }
}

/// Maps `raw` in `min..=max` onto -1024..=1024 (exact for EdgeTX's 0..=2048).
fn normalise(raw: i64, (min, max): (i32, i32)) -> i16 {
    let (min, max) = (i64::from(min), i64::from(max));
    if max <= min {
        return 0;
    }
    let range = max - min;
    let raw = raw.clamp(min, max);
    let scaled = ((raw - min) * 2048 + range / 2) / range;
    (scaled - 1024) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_axes_and_report_ids() {
        // Report ID 3, one 8-bit signed axis (-127..127), 4 buttons + 4 bits padding.
        let desc = [
            0x05, 0x01, 0x09, 0x04, 0xA1, 0x01, 0x85, 0x03, //
            0x09, 0x30, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x02, //
            0x05, 0x09, 0x19, 0x01, 0x29, 0x04, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x04,
            0x81, 0x02, //
            0x75, 0x04, 0x95, 0x01, 0x81, 0x03, 0xC0,
        ];
        let layout = Layout::parse(&desc).unwrap();
        assert!(layout.uses_report_ids);
        assert_eq!(
            (
                layout.axis_count(),
                layout.button_count(),
                layout.report_len()
            ),
            (1, 4, 2)
        );

        let r = layout.decode(&[3, 0x81, 0b0101]).unwrap(); // -127, buttons 1 and 3
        assert_eq!(r.axes, [-1024]);
        assert_eq!(r.buttons, [true, false, true, false]);
        assert_eq!(layout.decode(&[3, 0x7F, 0]).unwrap().axes, [1024]);
        assert_eq!(layout.decode(&[3, 0, 0]).unwrap().axes, [0]);
        assert!(layout.decode(&[4, 0, 0]).is_none(), "wrong report id");
        assert_eq!(r.channel(1), Some(-1024));
        assert_eq!(r.channel(2), Some(1024));
        assert_eq!(r.channel(3), Some(-1024));
        assert_eq!(r.channel(6), None);
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(Layout::parse(&[0x05]), Err(ParseError::Truncated));
        assert_eq!(
            Layout::parse(&[0x05, 0x01, 0xC0]),
            Err(ParseError::NoInputs)
        );
    }
}
