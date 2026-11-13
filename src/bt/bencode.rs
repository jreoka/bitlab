use anyhow::{bail, Context, Result};

const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BVal<'a> {
    Int(i64),
    Bytes(&'a [u8]),
    List(Vec<BVal<'a>>),
    Dict(Vec<(&'a [u8], BVal<'a>)>),
}

impl<'a> BVal<'a> {
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            BVal::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&'a str> {
        self.as_bytes().and_then(|b| std::str::from_utf8(b).ok())
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            BVal::Int(n) => Some(*n),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[BVal<'a>]> {
        match self {
            BVal::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn dict_get(&self, key: &[u8]) -> Option<&BVal<'a>> {
        match self {
            BVal::Dict(entries) => entries.iter().find(|(k, _)| *k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn dict_str(&self, key: &[u8]) -> Option<&'a str> {
        self.dict_get(key).and_then(|v| v.as_str())
    }

    pub fn dict_bytes(&self, key: &[u8]) -> Option<&'a [u8]> {
        self.dict_get(key).and_then(|v| v.as_bytes())
    }

    pub fn dict_i64(&self, key: &[u8]) -> Option<i64> {
        self.dict_get(key).and_then(|v| v.as_i64())
    }
}

pub fn decode(input: &[u8]) -> Result<BVal<'_>> {
    let (val, rest) = decode_with_rest(input)?;
    if !rest.is_empty() {
        bail!("trailing bytes after bencode value");
    }
    Ok(val)
}

pub fn decode_with_rest(input: &[u8]) -> Result<(BVal<'_>, &[u8])> {
    let mut pos = 0usize;
    let val = parse_val(input, &mut pos, 0)?;
    Ok((val, &input[pos..]))
}

pub fn extract_info_raw(input: &[u8]) -> Result<&[u8]> {
    let mut pos = 0usize;
    if input.first() != Some(&b'd') {
        bail!("torrent root is not a bencode dict");
    }
    pos += 1;
    while pos < input.len() {
        if input[pos] == b'e' {
            bail!("missing 'info' dictionary");
        }
        let key = parse_bytes_raw(input, &mut pos)?;
        let val_start = pos;
        skip_val(input, &mut pos, 1)?;
        let val_end = pos;
        if key == b"info" {
            if input.get(val_start) != Some(&b'd') {
                bail!("'info' value is not a dictionary");
            }
            return Ok(&input[val_start..val_end]);
        }
    }
    bail!("unterminated torrent dictionary");
}

fn parse_bytes_raw<'a>(input: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let start = *pos;
    let colon = input[*pos..]
        .iter()
        .position(|&b| b == b':')
        .context("unterminated byte string length")?;
    let len_slice = &input[start..start + colon];
    if len_slice.is_empty() || (len_slice.len() > 1 && len_slice[0] == b'0') {
        bail!("invalid byte string length");
    }
    if !len_slice.iter().all(|b| b.is_ascii_digit()) {
        bail!("non-digit byte string length");
    }
    let len_str = std::str::from_utf8(len_slice)?;
    let len: usize = len_str.parse().context("byte string length overflow")?;
    *pos = start + colon + 1;
    let end = pos.checked_add(len).context("byte string overflow")?;
    if end > input.len() {
        bail!("unexpected EOF in byte string");
    }
    let out = &input[*pos..end];
    *pos = end;
    Ok(out)
}

fn parse_int_raw(input: &[u8], pos: &mut usize) -> Result<i64> {
    if input.get(*pos) != Some(&b'i') {
        bail!("expected 'i'");
    }
    *pos += 1;
    let end_rel = input[*pos..]
        .iter()
        .position(|&b| b == b'e')
        .context("unterminated integer")?;
    let raw = &input[*pos..*pos + end_rel];
    if raw.is_empty() {
        bail!("empty integer");
    }
    if raw == b"-0" || (raw.len() > 1 && raw[0] == b'0') || (raw.len() > 2 && raw[0] == b'-' && raw[1] == b'0') {
        bail!("non-canonical bencode integer");
    }
    let s = std::str::from_utf8(raw).context("invalid utf-8 in integer")?;
    let n: i64 = s.parse().context("invalid integer")?;
    *pos += end_rel + 1;
    Ok(n)
}

fn parse_val<'a>(input: &'a [u8], pos: &mut usize, depth: usize) -> Result<BVal<'a>> {
    if depth >= MAX_DEPTH {
        bail!("bencode nesting limit exceeded");
    }
    let &b = input.get(*pos).context("unexpected EOF")?;
    match b {
        b'i' => Ok(BVal::Int(parse_int_raw(input, pos)?)),
        b'0'..=b'9' => Ok(BVal::Bytes(parse_bytes_raw(input, pos)?)),
        b'l' => {
            *pos += 1;
            let mut items = Vec::new();
            while *pos < input.len() {
                if input[*pos] == b'e' {
                    *pos += 1;
                    return Ok(BVal::List(items));
                }
                items.push(parse_val(input, pos, depth + 1)?);
            }
            bail!("unterminated list");
        }
        b'd' => {
            *pos += 1;
            let mut entries = Vec::new();
            while *pos < input.len() {
                if input[*pos] == b'e' {
                    *pos += 1;
                    return Ok(BVal::Dict(entries));
                }
                let key = parse_bytes_raw(input, pos)?;
                let val = parse_val(input, pos, depth + 1)?;
                entries.push((key, val));
            }
            bail!("unterminated dict");
        }
        other => bail!("invalid bencode tag 0x{other:02x} at {pos}"),
    }
}

fn skip_val(input: &[u8], pos: &mut usize, depth: usize) -> Result<()> {
    if depth >= MAX_DEPTH {
        bail!("bencode nesting limit exceeded");
    }
    let &b = input.get(*pos).context("unexpected EOF")?;
    match b {
        b'i' => {
            let _ = parse_int_raw(input, pos)?;
            Ok(())
        }
        b'0'..=b'9' => {
            let _ = parse_bytes_raw(input, pos)?;
            Ok(())
        }
        b'l' => {
            *pos += 1;
            while *pos < input.len() {
                if input[*pos] == b'e' {
                    *pos += 1;
                    return Ok(());
                }
                skip_val(input, pos, depth + 1)?;
            }
            bail!("unterminated list");
        }
        b'd' => {
            *pos += 1;
            while *pos < input.len() {
                if input[*pos] == b'e' {
                    *pos += 1;
                    return Ok(());
                }
                let _ = parse_bytes_raw(input, pos)?;
                skip_val(input, pos, depth + 1)?;
            }
            bail!("unterminated dict");
        }
        other => bail!("invalid bencode tag 0x{other:02x} at {pos}"),
    }
}

pub struct BWriter {
    pub buf: Vec<u8>,
}

impl BWriter {
    pub fn with_capacity(cap: usize) -> Self {
        Self { buf: Vec::with_capacity(cap) }
    }

    pub fn push_int(&mut self, n: i64) {
        self.buf.push(b'i');
        self.buf.extend_from_slice(n.to_string().as_bytes());
        self.buf.push(b'e');
    }

    pub fn push_bytes(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b.len().to_string().as_bytes());
        self.buf.push(b':');
        self.buf.extend_from_slice(b);
    }

    pub fn push_str(&mut self, s: &str) {
        self.push_bytes(s.as_bytes());
    }

    pub fn start_dict(&mut self) {
        self.buf.push(b'd');
    }

    pub fn start_list(&mut self) {
        self.buf.push(b'l');
    }

    pub fn end(&mut self) {
        self.buf.push(b'e');
    }

    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_and_extract_info_raw() {
        let torrent = b"d8:announce18:http://ex.com:80/a4:infod6:lengthi10e4:name8:test.bin12:piece lengthi16384e6:pieces20:12345678901234567890ee";
        let val = decode(torrent).unwrap();
        assert_eq!(val.dict_str(b"announce"), Some("http://ex.com:80/a"));
        let raw_info = extract_info_raw(torrent).unwrap();
        assert_eq!(
            raw_info,
            b"d6:lengthi10e4:name8:test.bin12:piece lengthi16384e6:pieces20:12345678901234567890e"
        );
    }

    #[test]
    fn test_decode_with_rest_ut_metadata_style() {
        let mut msg = b"d8:msg_typei1e5:piecei0e10:total_sizei4ee".to_vec();
        msg.extend_from_slice(b"ABCD");
        let (dict, payload) = decode_with_rest(&msg).unwrap();
        assert_eq!(dict.dict_i64(b"msg_type"), Some(1));
        assert_eq!(dict.dict_i64(b"piece"), Some(0));
        assert_eq!(payload, b"ABCD");
    }

    #[test]
    fn test_depth_limit_rejects_overflow() {
        let mut deep = vec![b'l'; 100];
        deep.extend(vec![b'e'; 100]);
        assert!(decode(&deep).is_err());
    }
}
