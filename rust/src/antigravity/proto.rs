//! Just enough protobuf wire-format reading for Antigravity's records. The
//! CLI stores its conversation state as serialized messages inside SQLite
//! blobs; the few fields this tool needs are read by number, so no schema
//! (or code generator) is involved.

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
    Fixed64(u64),
    Fixed32(u32),
}

fn varint(buf: &[u8], pos: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *buf.get(*pos)?;
        *pos += 1;
        if shift < 64 {
            value |= ((byte & 0x7f) as u64) << shift;
        }
        if byte < 0x80 {
            return Some(value);
        }
        shift += 7;
        if shift > 70 {
            return None;
        }
    }
}

/// Every top-level (number, value) of one message, in order. A malformed
/// tail ends the list instead of failing the whole message.
pub fn fields(buf: &[u8]) -> Vec<(u32, Field<'_>)> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < buf.len() {
        let key = match varint(buf, &mut pos) {
            Some(k) => k,
            None => break,
        };
        let number = (key >> 3) as u32;
        let value = match key & 7 {
            0 => match varint(buf, &mut pos) {
                Some(v) => Field::Varint(v),
                None => break,
            },
            1 => match buf.get(pos..pos + 8) {
                Some(b) => {
                    pos += 8;
                    Field::Fixed64(u64::from_le_bytes(b.try_into().unwrap()))
                }
                None => break,
            },
            2 => {
                let len = match varint(buf, &mut pos) {
                    Some(l) => l as usize,
                    None => break,
                };
                match pos.checked_add(len).and_then(|end| buf.get(pos..end)) {
                    Some(b) => {
                        pos += len;
                        Field::Bytes(b)
                    }
                    None => break,
                }
            }
            5 => match buf.get(pos..pos + 4) {
                Some(b) => {
                    pos += 4;
                    Field::Fixed32(u32::from_le_bytes(b.try_into().unwrap()))
                }
                None => break,
            },
            _ => break,
        };
        out.push((number, value));
    }
    out
}

/// The first length-delimited field `number` (a sub-message or a string).
pub fn bytes(buf: &[u8], number: u32) -> Option<&[u8]> {
    fields(buf).into_iter().find_map(|(n, v)| match v {
        Field::Bytes(b) if n == number => Some(b),
        _ => None,
    })
}

pub fn varint_field(buf: &[u8], number: u32) -> Option<u64> {
    fields(buf).into_iter().find_map(|(n, v)| match v {
        Field::Varint(x) if n == number => Some(x),
        _ => None,
    })
}

pub fn string(buf: &[u8], number: u32) -> Option<String> {
    bytes(buf, number).and_then(|b| std::str::from_utf8(b).ok()).map(|s| s.to_string())
}

/// A google.protobuf.Timestamp sub-message as Unix seconds.
pub fn timestamp(buf: &[u8], number: u32) -> Option<f64> {
    let ts = bytes(buf, number)?;
    let seconds = varint_field(ts, 1)? as f64;
    let nanos = varint_field(ts, 2).unwrap_or(0) as f64;
    Some(seconds + nanos / 1e9)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_varints_strings_and_nested_messages() {
        // {1: 1533, 2: "ab", 3: {1: 1795386637, 2: 5}}
        let inner = [0x08, 0x8d, 0xda, 0x8d, 0xd8, 0x06, 0x10, 0x05];
        let mut msg = vec![0x08, 0xfd, 0x0b, 0x12, 0x02, b'a', b'b', 0x1a, inner.len() as u8];
        msg.extend_from_slice(&inner);
        assert_eq!(Some(1533), varint_field(&msg, 1));
        assert_eq!(Some("ab".to_string()), string(&msg, 2));
        assert_eq!(Some(1_795_386_637.000_000_005), timestamp(&msg, 3));
        assert_eq!(None, varint_field(&msg, 9));
        // a truncated tail keeps what was read before it
        assert_eq!(Some(1533), varint_field(&msg[..4], 1));
    }
}
