//! Owned bencode values for protocol messages (DHT, extension protocol).

use std::collections::BTreeMap;

/// Nesting limit, so hostile input cannot exhaust the stack.
const MAX_DEPTH: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

impl Value {
    pub(crate) fn bytes(bytes: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(bytes.into())
    }

    /// Builds a dictionary from `(key, value)` pairs.
    pub(crate) fn dict<const N: usize>(entries: [(&[u8], Value); N]) -> Self {
        Self::Dict(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_vec(), value))
                .collect(),
        )
    }

    pub(crate) fn get(&self, key: &[u8]) -> Option<&Value> {
        match self {
            Self::Dict(entries) => entries.get(key),
            _ => None,
        }
    }

    pub(crate) fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(bytes) => Some(bytes),
            _ => None,
        }
    }

    pub(crate) fn as_int(&self) -> Option<i64> {
        match self {
            Self::Int(value) => Some(*value),
            _ => None,
        }
    }

    pub(crate) fn as_list(&self) -> Option<&[Value]> {
        match self {
            Self::List(items) => Some(items),
            _ => None,
        }
    }

    pub(crate) fn get_bytes(&self, key: &[u8]) -> Option<&[u8]> {
        self.get(key).and_then(Value::as_bytes)
    }

    pub(crate) fn get_int(&self, key: &[u8]) -> Option<i64> {
        self.get(key).and_then(Value::as_int)
    }

    pub(crate) fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Int(value) => out.extend(format!("i{value}e").as_bytes()),
            Self::Bytes(bytes) => {
                out.extend(format!("{}:", bytes.len()).as_bytes());
                out.extend(bytes);
            }
            Self::List(items) => {
                out.push(b'l');
                items.iter().for_each(|item| item.encode_into(out));
                out.push(b'e');
            }
            Self::Dict(entries) => {
                // BTreeMap iterates in sorted key order, as bencode requires.
                out.push(b'd');
                for (key, value) in entries {
                    Self::Bytes(key.clone()).encode_into(out);
                    value.encode_into(out);
                }
                out.push(b'e');
            }
        }
    }
}

/// Decodes one complete value, rejecting trailing bytes.
pub(crate) fn decode(bytes: &[u8]) -> Result<Value, crate::BackendError> {
    let (value, used) = decode_prefix(bytes)?;
    if used != bytes.len() {
        return Err(invalid("trailing data after bencoded value"));
    }
    Ok(value)
}

/// Decodes one value from the start of `bytes`, returning it and the number
/// of bytes it used. Extension messages carry raw data after the dictionary.
pub(crate) fn decode_prefix(bytes: &[u8]) -> Result<(Value, usize), crate::BackendError> {
    let mut position = 0;
    let value = parse(bytes, &mut position, 0)?;
    Ok((value, position))
}

fn parse(bytes: &[u8], position: &mut usize, depth: usize) -> Result<Value, crate::BackendError> {
    if depth > MAX_DEPTH {
        return Err(invalid("bencoded value is nested too deeply"));
    }
    match bytes.get(*position) {
        Some(b'i') => {
            let end = find(bytes, *position + 1, b'e')?;
            let text = std::str::from_utf8(&bytes[*position + 1..end])
                .map_err(|_| invalid("bencoded integer is not text"))?;
            let canonical = text == "0"
                || (!text.starts_with('0') && !text.starts_with("-0") && !text.is_empty());
            let value = text
                .parse::<i64>()
                .ok()
                .filter(|_| canonical)
                .ok_or_else(|| invalid("bencoded integer is invalid"))?;
            *position = end + 1;
            Ok(Value::Int(value))
        }
        Some(b'l') => {
            *position += 1;
            let mut items = Vec::new();
            while bytes.get(*position) != Some(&b'e') {
                items.push(parse(bytes, position, depth + 1)?);
            }
            *position += 1;
            Ok(Value::List(items))
        }
        Some(b'd') => {
            *position += 1;
            let mut entries = BTreeMap::new();
            while bytes.get(*position) != Some(&b'e') {
                let Value::Bytes(key) = parse_bytes(bytes, position)? else {
                    unreachable!("parse_bytes returns bytes");
                };
                let value = parse(bytes, position, depth + 1)?;
                entries.insert(key, value);
            }
            *position += 1;
            Ok(Value::Dict(entries))
        }
        Some(b'0'..=b'9') => parse_bytes(bytes, position),
        Some(_) => Err(invalid("unexpected byte in bencoded value")),
        None => Err(invalid("bencoded value is truncated")),
    }
}

fn parse_bytes(bytes: &[u8], position: &mut usize) -> Result<Value, crate::BackendError> {
    let colon = find(bytes, *position, b':')?;
    let length = std::str::from_utf8(&bytes[*position..colon])
        .ok()
        .and_then(|text| text.parse::<usize>().ok())
        .ok_or_else(|| invalid("bencoded string length is invalid"))?;
    let start = colon + 1;
    let end = start
        .checked_add(length)
        .filter(|end| *end <= bytes.len())
        .ok_or_else(|| invalid("bencoded string is truncated"))?;
    *position = end;
    Ok(Value::Bytes(bytes[start..end].to_vec()))
}

fn find(bytes: &[u8], from: usize, byte: u8) -> Result<usize, crate::BackendError> {
    bytes
        .get(from..)
        .and_then(|rest| rest.iter().position(|candidate| *candidate == byte))
        .map(|offset| from + offset)
        .ok_or_else(|| invalid("bencoded value is truncated"))
}

fn invalid(message: &str) -> crate::BackendError {
    crate::BackendError::InvalidPeerMessage(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_nested_values() {
        let value = Value::dict([
            (b"t", Value::bytes(*b"aa")),
            (b"y", Value::bytes(*b"q")),
            (
                b"a",
                Value::dict([(b"id", Value::bytes([1; 20])), (b"n", Value::Int(-7))]),
            ),
            (b"l", Value::List(vec![Value::Int(0), Value::bytes(*b"")])),
        ]);
        let encoded = value.encode();
        assert!(encoded.starts_with(b"d1:ad2:id20:"));
        assert_eq!(decode(&encoded).unwrap(), value);
    }

    #[test]
    fn decodes_a_prefix_followed_by_raw_data() {
        let (value, used) = decode_prefix(b"d8:msg_typei1ee\x01\x02").unwrap();
        assert_eq!(value.get_int(b"msg_type"), Some(1));
        assert_eq!(used, 15);
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            &b"i01e"[..],
            b"i-0e",
            b"ie",
            b"5:abc",
            b"l",
            b"d1:a",
            b"x",
            b"i1ex",
        ] {
            assert!(decode(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let deep = "l".repeat(100) + &"e".repeat(100);
        assert!(decode(deep.as_bytes()).is_err());
    }
}
