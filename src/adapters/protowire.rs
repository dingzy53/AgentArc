//! Minimal protobuf wire-format decoder.
//!
//! Antigravity stores per-generation metadata as protobuf blobs without a
//! schema. We only need varints and length-delimited fields, so a tiny walker is
//! preferable to a generated schema (and lets us tolerate unknown fields).

use std::fmt;

#[derive(Clone, Copy, Debug)]
pub enum WireValue<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
    Fixed64(u64),
}

#[derive(Clone, Copy, Debug)]
pub struct Field<'a> {
    pub number: u32,
    pub value: WireValue<'a>,
}

#[derive(Debug)]
pub struct WireError(pub String);

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for WireError {}

fn varint(data: &[u8], pos: &mut usize) -> Result<u64, WireError> {
    let mut result = 0u64;
    let mut shift = 0u32;
    loop {
        if *pos >= data.len() {
            return Err(WireError("truncated varint".into()));
        }
        if shift >= 64 {
            return Err(WireError("varint too long".into()));
        }
        let b = data[*pos];
        *pos += 1;
        result |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Ok(result);
        }
        shift += 7;
    }
}

/// Decode all fields. Unknown wire types abort decoding (the caller can treat
/// the data as not-a-message).
pub fn parse(data: &[u8]) -> Result<Vec<Field<'_>>, WireError> {
    let mut pos = 0usize;
    let mut fields = Vec::new();
    while pos < data.len() {
        let key = varint(data, &mut pos)?;
        let number = (key >> 3) as u32;
        if number == 0 {
            return Err(WireError("field number zero".into()));
        }
        let wire = (key & 7) as u8;
        let value = match wire {
            0 => WireValue::Varint(varint(data, &mut pos)?),
            1 => {
                if pos + 8 > data.len() {
                    return Err(WireError("truncated fixed64".into()));
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&data[pos..pos + 8]);
                pos += 8;
                WireValue::Fixed64(u64::from_le_bytes(b))
            }
            2 => {
                let len = varint(data, &mut pos)? as usize;
                if pos + len > data.len() {
                    return Err(WireError("truncated length-delimited field".into()));
                }
                let bytes = &data[pos..pos + len];
                pos += len;
                WireValue::Bytes(bytes)
            }
            5 => {
                if pos + 4 > data.len() {
                    return Err(WireError("truncated fixed32".into()));
                }
                let mut b = [0u8; 4];
                b.copy_from_slice(&data[pos..pos + 4]);
                pos += 4;
                WireValue::Fixed32(u32::from_le_bytes(b))
            }
            other => return Err(WireError(format!("unsupported wire type {other}"))),
        };
        fields.push(Field { number, value });
    }
    Ok(fields)
}

pub fn parse_ok(data: &[u8]) -> Option<Vec<Field<'_>>> {
    parse(data).ok().filter(|f| !f.is_empty())
}

pub fn varint_field(fields: &[Field<'_>], number: u32) -> Option<u64> {
    fields.iter().rev().find_map(|f| match (f.number, f.value) {
        (n, WireValue::Varint(v)) if n == number => Some(v),
        _ => None,
    })
}

pub fn bytes_field<'a>(fields: &'a [Field<'a>], number: u32) -> Option<&'a [u8]> {
    fields.iter().rev().find_map(|f| match (f.number, f.value) {
        (n, WireValue::Bytes(v)) if n == number => Some(v),
        _ => None,
    })
}

pub fn all_bytes<'a>(fields: &'a [Field<'a>], number: u32) -> Vec<&'a [u8]> {
    fields
        .iter()
        .filter_map(|f| match (f.number, f.value) {
            (n, WireValue::Bytes(v)) if n == number => Some(v),
            _ => None,
        })
        .collect()
}

pub fn str_field(fields: &[Field<'_>], number: u32) -> Option<String> {
    bytes_field(fields, number).and_then(|b| String::from_utf8(b.to_vec()).ok())
}

/// Nested message accessor: returns the last occurrence of a length-delimited
/// field decoded as a message.
pub fn message<'a>(fields: &'a [Field<'a>], number: u32) -> Option<Vec<Field<'a>>> {
    bytes_field(fields, number).and_then(parse_ok)
}

/// A field is treated as a string when it is valid UTF-8 and mostly printable.
pub fn plausible_string(bytes: &[u8]) -> Option<String> {
    let s = std::str::from_utf8(bytes).ok()?;
    if s.is_empty() {
        return Some(String::new());
    }
    let printable = s
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .count();
    if printable * 10 < s.chars().count() * 9 {
        return None;
    }
    Some(s.to_string())
}

pub fn entropy(data: &[u8]) -> f64 {
    if data.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for b in data {
        counts[*b as usize] += 1;
    }
    let len = data.len() as f64;
    let mut h = 0.0;
    for c in counts {
        if c > 0 {
            let p = c as f64 / len;
            h -= p * p.log2();
        }
    }
    h
}

/// Decide whether a `.pb` file is plain protobuf we can inspect. Encrypted
/// blobs have high entropy and do not fully decode.
pub fn looks_like_plain_protobuf(data: &[u8]) -> bool {
    if data.len() < 4 {
        return false;
    }
    if entropy(data) > 7.2 {
        return false;
    }
    // Low entropy plus a complete wire-format parse is a good signal. Note
    // that a top-level message may consist only of nested message bytes, so we
    // must not require a printable string at this level.
    matches!(parse(data), Ok(fields) if !fields.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_varint_and_bytes() {
        // field 1 varint 1318, field 2 bytes "hi"
        let data = [0x08, 0xa6, 0x0a, 0x12, 0x02, b'h', b'i'];
        let f = parse(&data).unwrap();
        assert_eq!(varint_field(&f, 1), Some(1318));
        assert_eq!(str_field(&f, 2).as_deref(), Some("hi"));
    }
}
