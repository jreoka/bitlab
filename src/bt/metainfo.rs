use anyhow::{bail, Context, Result};
use bytes::Bytes;
use sha1::{Digest, Sha1};
use std::fmt;
use std::net::SocketAddr;
use std::ops::RangeInclusive;
use std::path::PathBuf;

use super::bencode::{self, BVal};

pub const CHUNK_SIZE: u32 = 16_384;

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Id20(pub [u8; 20]);

impl Id20 {
    pub fn from_hex(s: &str) -> Result<Self> {
        let s = s.trim();
        if s.len() != 40 {
            bail!("invalid infohash hex length");
        }
        let v = hex::decode(s).context("invalid hex infohash")?;
        let mut out = [0u8; 20];
        out.copy_from_slice(&v);
        Ok(Self(out))
    }

    pub fn from_slice(s: &[u8]) -> Result<Self> {
        if s.len() != 20 {
            bail!("expected 20 bytes, got {}", s.len());
        }
        let mut out = [0u8; 20];
        out.copy_from_slice(s);
        Ok(Self(out))
    }

    pub fn as_string(self) -> String {
        hex::encode(self.0)
    }

    pub fn distance(&self, other: &Id20) -> [u8; 20] {
        let mut out = [0u8; 20];
        for i in 0..20 {
            out[i] = self.0[i] ^ other.0[i];
        }
        out
    }

    /// Bucket index in 0..160 (0 = farthest MSB differs, 159 = closest bit differs).
    /// Returns None if identical.
    pub fn bucket_index(&self, other: &Id20) -> Option<usize> {
        let dist = self.distance(other);
        for (byte_idx, &b) in dist.iter().enumerate() {
            if b != 0 {
                return Some(byte_idx * 8 + b.leading_zeros() as usize);
            }
        }
        None
    }
}

impl fmt::Debug for Id20 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Display for Id20 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

#[derive(Debug, Clone)]
pub struct MagnetLink {
    pub info_hash: Id20,
    pub name: Option<String>,
    pub trackers: Vec<String>,
    pub peer_hints: Vec<SocketAddr>,
}

pub fn parse_magnet(raw: &str) -> Result<MagnetLink> {
    let u = url::Url::parse(raw.trim()).context("invalid magnet uri")?;
    if u.scheme() != "magnet" {
        bail!("not a magnet url");
    }
    let mut info_hash = None;
    let mut name = None;
    let mut trackers = Vec::new();
    let mut peer_hints = Vec::new();

    for (k, v) in u.query_pairs() {
        match k.as_ref() {
            "xt" => {
                if let Some(rest) = v.strip_prefix("urn:btih:") {
                    let s = rest.trim();
                    if s.len() == 40 {
                        if let Ok(ih) = Id20::from_hex(s) {
                            info_hash = Some(ih);
                        }
                    } else if s.len() == 32 {
                        if let Some(decoded) =
                            base32::decode(base32::Alphabet::RFC4648 { padding: false }, &s.to_ascii_uppercase())
                        {
                            if let Ok(ih) = Id20::from_slice(&decoded) {
                                info_hash = Some(ih);
                            }
                        }
                    }
                }
            }
            "dn" => {
                if !v.trim().is_empty() && name.is_none() {
                    name = Some(v.into_owned());
                }
            }
            "tr" => {
                let tr = v.trim();
                if !tr.is_empty() {
                    trackers.push(tr.to_string());
                }
            }
            "x.pe" => {
                if let Ok(addr) = v.trim().parse::<SocketAddr>() {
                    peer_hints.push(addr);
                }
            }
            _ => {}
        }
    }

    let info_hash = info_hash.context("magnet missing valid urn:btih")?;
    Ok(MagnetLink { info_hash, name, trackers, peer_hints })
}

#[derive(Debug, Clone)]
pub struct FileEntry {
    pub idx: usize,
    pub relative_filename: PathBuf,
    pub offset: u64,
    pub len: u64,
    pub is_padding: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct FileSlice {
    pub file_idx: usize,
    pub file_offset: u64,
    pub piece_offset: usize,
    pub len: usize,
}

#[derive(Debug, Clone)]
pub struct TorrentMeta {
    pub info_hash: Id20,
    pub info_bytes: Bytes,
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<[u8; 20]>,
    pub file_infos: Vec<FileEntry>,
    pub total_length: u64,
    pub trackers: Vec<String>,
}

fn sanitize_component(raw: &str, fallback: &str) -> String {
    let cleaned: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        fallback.to_string()
    } else {
        trimmed.to_string()
    }
}

fn parse_info_dict(info: &BVal<'_>, info_bytes: Bytes, info_hash: Id20, trackers: Vec<String>) -> Result<TorrentMeta> {
    let piece_length_i = info.dict_i64(b"piece length").context("info missing piece length")?;
    if piece_length_i <= 0 || piece_length_i > 256 * 1024 * 1024 {
        bail!("invalid piece length {piece_length_i}");
    }
    let piece_length = piece_length_i as u64;

    let pieces_raw = info.dict_bytes(b"pieces").context("info missing pieces")?;
    if pieces_raw.len() % 20 != 0 {
        bail!("pieces byte length {} not divisible by 20", pieces_raw.len());
    }
    let mut pieces = Vec::with_capacity(pieces_raw.len() / 20);
    pieces.extend_from_slice(pieces_raw.as_chunks::<20>().0);

    let raw_name = info
        .dict_str(b"name.utf-8")
        .or_else(|| info.dict_str(b"name"))
        .unwrap_or("torrent");
    let safe_root_name = sanitize_component(raw_name, "torrent");

    let mut file_infos = Vec::new();
    let mut offset: u64 = 0;

    if let Some(files_list) = info.dict_get(b"files").and_then(|v| v.as_list()) {
        for (idx, fval) in files_list.iter().enumerate() {
            let flen_i = fval.dict_i64(b"length").context("file entry missing length")?;
            if flen_i < 0 {
                bail!("negative file length");
            }
            let flen = flen_i as u64;
            let is_padding = fval
                .dict_str(b"attr")
                .map(|a| a.contains('p'))
                .unwrap_or(false);

            let path_items = fval
                .dict_get(b"path.utf-8")
                .or_else(|| fval.dict_get(b"path"))
                .and_then(|v| v.as_list())
                .context("file entry missing path")?;

            let mut rel = PathBuf::from(&safe_root_name);
            for (cidx, comp) in path_items.iter().enumerate() {
                let s = String::from_utf8_lossy(comp.as_bytes().unwrap_or(b"file"));
                let fallback = if cidx + 1 == path_items.len() {
                    format!("file_{idx}")
                } else {
                    format!("dir_{cidx}")
                };
                rel.push(sanitize_component(&s, &fallback));
            }
            if rel.as_os_str().is_empty() {
                rel.push(format!("file_{idx}"));
            }
            file_infos.push(FileEntry {
                idx,
                relative_filename: rel,
                offset,
                len: flen,
                is_padding,
            });
            offset = offset.checked_add(flen).context("torrent total size overflow")?;
        }
    } else if let Some(flen_i) = info.dict_i64(b"length") {
        if flen_i < 0 {
            bail!("negative file length");
        }
        let flen = flen_i as u64;
        file_infos.push(FileEntry {
            idx: 0,
            relative_filename: PathBuf::from(&safe_root_name),
            offset: 0,
            len: flen,
            is_padding: false,
        });
        offset = flen;
    } else {
        bail!("info dict missing both 'files' and 'length'");
    }

    let expected_pieces = if offset == 0 {
        0
    } else {
        offset.div_ceil(piece_length)
    };
    if pieces.len() as u64 != expected_pieces {
        bail!(
            "piece count mismatch: got {} hashes, expected {} for total_len={}",
            pieces.len(),
            expected_pieces,
            offset
        );
    }

    Ok(TorrentMeta {
        info_hash,
        info_bytes,
        name: raw_name.to_string(),
        piece_length,
        pieces,
        file_infos,
        total_length: offset,
        trackers,
    })
}

impl TorrentMeta {
    pub fn from_torrent_bytes(raw: &[u8]) -> Result<Self> {
        let root = bencode::decode(raw).context("decode .torrent")?;
        let raw_info = bencode::extract_info_raw(raw).context("extract raw info dict")?;
        let digest: [u8; 20] = Sha1::digest(raw_info).into();
        let info_hash = Id20(digest);

        let mut trackers = Vec::new();
        if let Some(a) = root.dict_str(b"announce") {
            let a = a.trim();
            if !a.is_empty() {
                trackers.push(a.to_string());
            }
        }
        if let Some(tiers) = root.dict_get(b"announce-list").and_then(|v| v.as_list()) {
            for tier in tiers {
                if let Some(urls) = tier.as_list() {
                    for u in urls {
                        if let Some(s) = u.as_str() {
                            let s = s.trim();
                            if !s.is_empty() && !trackers.iter().any(|x| x == s) {
                                trackers.push(s.to_string());
                            }
                        }
                    }
                }
            }
        }

        let info = root.dict_get(b"info").context("missing info dict")?;
        parse_info_dict(info, Bytes::copy_from_slice(raw_info), info_hash, trackers)
    }

    pub fn from_info_bytes(info_bytes: Bytes, expected_hash: Option<Id20>) -> Result<Self> {
        let digest: [u8; 20] = Sha1::digest(&info_bytes).into();
        let ih = Id20(digest);
        if let Some(want) = expected_hash {
            if want != ih {
                bail!("infohash mismatch: expected {want}, got {ih}");
            }
        }
        let info_ref = info_bytes.clone();
        let info = bencode::decode(&info_ref).context("decode info dict")?;
        parse_info_dict(&info, info_bytes, ih, Vec::new())
    }

    pub fn num_pieces(&self) -> usize {
        self.pieces.len()
    }

    pub fn piece_len(&self, piece_idx: usize) -> u32 {
        let start = (piece_idx as u64) * self.piece_length;
        if start >= self.total_length {
            return 0;
        }
        (self.total_length - start).min(self.piece_length) as u32
    }

    pub fn num_chunks(&self, piece_idx: usize) -> usize {
        let plen = self.piece_len(piece_idx);
        if plen == 0 {
            0
        } else {
            plen.div_ceil(CHUNK_SIZE) as usize
        }
    }

    pub fn chunk_len(&self, piece_idx: usize, chunk_idx: usize) -> u32 {
        let plen = self.piece_len(piece_idx);
        let begin = (chunk_idx as u32) * CHUNK_SIZE;
        if begin >= plen {
            0
        } else {
            (plen - begin).min(CHUNK_SIZE)
        }
    }

    pub fn file_offset_to_piece(&self, file_idx: usize, file_offset: u64) -> Option<(usize, usize)> {
        let fi = self.file_infos.get(file_idx)?;
        if file_offset >= fi.len || self.piece_length == 0 {
            return None;
        }
        let abs = fi.offset + file_offset;
        let p = (abs / self.piece_length) as usize;
        let off = (abs % self.piece_length) as usize;
        Some((p, off))
    }

    pub fn file_range_to_pieces(&self, file_idx: usize, start: u64, len: u64) -> Option<RangeInclusive<usize>> {
        let fi = self.file_infos.get(file_idx)?;
        if fi.len == 0 || len == 0 || start >= fi.len || self.piece_length == 0 {
            return None;
        }
        let clamped_end = start.saturating_add(len).min(fi.len);
        if clamped_end <= start {
            return None;
        }
        let abs_start = fi.offset + start;
        let abs_last = fi.offset + clamped_end - 1;
        let p0 = (abs_start / self.piece_length) as usize;
        let p1 = (abs_last / self.piece_length) as usize;
        Some(p0..=p1)
    }

    pub fn piece_to_file_slices(&self, piece_idx: usize) -> Vec<FileSlice> {
        let p_start = (piece_idx as u64) * self.piece_length;
        let p_len = self.piece_len(piece_idx) as u64;
        let p_end = p_start + p_len;
        if p_len == 0 {
            return Vec::new();
        }
        // Binary search first file whose end > p_start
        let idx = self
            .file_infos
            .partition_point(|f| f.offset + f.len <= p_start);
        let mut out = Vec::new();
        for fi in &self.file_infos[idx..] {
            if fi.offset >= p_end {
                break;
            }
            if fi.len == 0 {
                continue;
            }
            let f_end = fi.offset + fi.len;
            let ov_start = p_start.max(fi.offset);
            let ov_end = p_end.min(f_end);
            if ov_end > ov_start {
                out.push(FileSlice {
                    file_idx: fi.idx,
                    file_offset: ov_start - fi.offset,
                    piece_offset: (ov_start - p_start) as usize,
                    len: (ov_end - ov_start) as usize,
                });
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_magnet_hex_and_base32() {
        let m1 = parse_magnet("magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Movie&tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce&x.pe=1.2.3.4:6881").unwrap();
        assert_eq!(m1.info_hash.as_string(), "0123456789abcdef0123456789abcdef01234567");
        assert_eq!(m1.name.as_deref(), Some("Movie"));
        assert_eq!(m1.trackers, vec!["udp://tracker.opentrackr.org:1337/announce"]);
        assert_eq!(m1.peer_hints, vec!["1.2.3.4:6881".parse::<SocketAddr>().unwrap()]);

        let m2 = parse_magnet("magnet:?xt=urn:btih:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA").unwrap();
        assert_eq!(m2.info_hash.as_string(), "0000000000000000000000000000000000000000");
    }

    #[test]
    fn test_torrent_meta_single_and_piece_slices() {
        let dummy = b"d4:infod6:lengthi10e4:name8:test.bin12:piece lengthi16384e6:pieces20:12345678901234567890ee";
        let meta = TorrentMeta::from_torrent_bytes(dummy).unwrap();
        assert_eq!(meta.total_length, 10);
        assert_eq!(meta.num_pieces(), 1);
        assert_eq!(meta.piece_len(0), 10);
        assert_eq!(meta.num_chunks(0), 1);
        assert_eq!(meta.chunk_len(0, 0), 10);
        let slices = meta.piece_to_file_slices(0);
        assert_eq!(slices.len(), 1);
        assert_eq!(slices[0].file_idx, 0);
        assert_eq!(slices[0].len, 10);
    }

    #[test]
    fn test_path_traversal_sanitized() {
        let mut w = bencode::BWriter::with_capacity(256);
        w.start_dict();
        w.push_str("info");
        w.start_dict();
        w.push_str("files");
        w.start_list();
        w.start_dict();
        w.push_str("length");
        w.push_int(5);
        w.push_str("path");
        w.start_list();
        w.push_str("..");
        w.push_str("..");
        w.push_str("evil:file?.mkv");
        w.end();
        w.end();
        w.end();
        w.push_str("name");
        w.push_str("../pack");
        w.push_str("piece length");
        w.push_int(16384);
        w.push_str("pieces");
        w.push_bytes(&[7u8; 20]);
        w.end();
        w.end();
        let meta = TorrentMeta::from_torrent_bytes(&w.finish()).unwrap();
        let rel = meta.file_infos[0].relative_filename.to_string_lossy().to_string();
        assert!(!rel.contains(".."));
        assert!(!rel.contains('?'));
    }
}
