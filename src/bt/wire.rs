use anyhow::{bail, Context, Result};
use bytes::{BufMut, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::bencode::{self, BWriter};
use super::metainfo::Id20;

const PSTR: &[u8; 19] = b"BitTorrent protocol";
pub const MY_EXT_UT_METADATA: u8 = 1;
pub const MY_EXT_UT_PEX: u8 = 2;

#[derive(Debug, Clone, Copy)]
pub struct Handshake {
    pub reserved: [u8; 8],
    pub info_hash: Id20,
    pub peer_id: [u8; 20],
}

impl Handshake {
    pub fn new(info_hash: Id20, peer_id: [u8; 20]) -> Self {
        let mut reserved = [0u8; 8];
        reserved[5] = 0x10; // BEP 10 Extension Protocol
        reserved[7] = 0x05; // BEP 6 Fast Extension (0x04) + BEP 5 DHT (0x01)
        Self { reserved, info_hash, peer_id }
    }

    pub fn supports_ext(&self) -> bool {
        (self.reserved[5] & 0x10) != 0
    }

    pub fn supports_fast(&self) -> bool {
        (self.reserved[7] & 0x04) != 0
    }

    pub fn to_bytes(self) -> [u8; 68] {
        let mut out = [0u8; 68];
        out[0] = 19;
        out[1..20].copy_from_slice(PSTR);
        out[20..28].copy_from_slice(&self.reserved);
        out[28..48].copy_from_slice(&self.info_hash.0);
        out[48..68].copy_from_slice(&self.peer_id);
        out
    }

    pub fn from_bytes(b: &[u8; 68]) -> Result<Self> {
        if b[0] != 19 || &b[1..20] != PSTR {
            bail!("invalid bittorrent handshake header");
        }
        let mut reserved = [0u8; 8];
        reserved.copy_from_slice(&b[20..28]);
        let mut ih = [0u8; 20];
        ih.copy_from_slice(&b[28..48]);
        let mut peer_id = [0u8; 20];
        peer_id.copy_from_slice(&b[48..68]);
        Ok(Self {
            reserved,
            info_hash: Id20(ih),
            peer_id,
        })
    }
}

pub async fn write_handshake<W: AsyncWrite + Unpin>(w: &mut W, hs: &Handshake) -> Result<()> {
    w.write_all(&hs.to_bytes()).await?;
    w.flush().await?;
    Ok(())
}

pub async fn read_handshake<R: AsyncRead + Unpin>(r: &mut R) -> Result<Handshake> {
    let mut buf = [0u8; 68];
    r.read_exact(&mut buf).await?;
    Handshake::from_bytes(&buf)
}

#[derive(Debug, Clone)]
pub enum WireMsg {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Bytes),
    Request { index: u32, begin: u32, length: u32 },
    Piece { index: u32, begin: u32, data: Bytes },
    Cancel { index: u32, begin: u32, length: u32 },
    Port(u16),
    // BEP 6 Fast Extension
    SuggestPiece(u32),
    HaveAll,
    HaveNone,
    RejectRequest { index: u32, begin: u32, length: u32 },
    AllowedFast(u32),
    // BEP 10 Extension Protocol
    Extended { ext_id: u8, payload: Bytes },
}

impl WireMsg {
    pub fn encode_into(&self, dst: &mut BytesMut) {
        match self {
            WireMsg::KeepAlive => dst.put_u32(0),
            WireMsg::Choke => {
                dst.put_u32(1);
                dst.put_u8(0);
            }
            WireMsg::Unchoke => {
                dst.put_u32(1);
                dst.put_u8(1);
            }
            WireMsg::Interested => {
                dst.put_u32(1);
                dst.put_u8(2);
            }
            WireMsg::NotInterested => {
                dst.put_u32(1);
                dst.put_u8(3);
            }
            WireMsg::Have(p) => {
                dst.put_u32(5);
                dst.put_u8(4);
                dst.put_u32(*p);
            }
            WireMsg::Bitfield(b) => {
                dst.put_u32(1 + b.len() as u32);
                dst.put_u8(5);
                dst.extend_from_slice(b);
            }
            WireMsg::Request { index, begin, length } => {
                dst.put_u32(13);
                dst.put_u8(6);
                dst.put_u32(*index);
                dst.put_u32(*begin);
                dst.put_u32(*length);
            }
            WireMsg::Piece { index, begin, data } => {
                dst.put_u32(9 + data.len() as u32);
                dst.put_u8(7);
                dst.put_u32(*index);
                dst.put_u32(*begin);
                dst.extend_from_slice(data);
            }
            WireMsg::Cancel { index, begin, length } => {
                dst.put_u32(13);
                dst.put_u8(8);
                dst.put_u32(*index);
                dst.put_u32(*begin);
                dst.put_u32(*length);
            }
            WireMsg::Port(port) => {
                dst.put_u32(3);
                dst.put_u8(9);
                dst.put_u16(*port);
            }
            WireMsg::SuggestPiece(p) => {
                dst.put_u32(5);
                dst.put_u8(13);
                dst.put_u32(*p);
            }
            WireMsg::HaveAll => {
                dst.put_u32(1);
                dst.put_u8(14);
            }
            WireMsg::HaveNone => {
                dst.put_u32(1);
                dst.put_u8(15);
            }
            WireMsg::RejectRequest { index, begin, length } => {
                dst.put_u32(13);
                dst.put_u8(16);
                dst.put_u32(*index);
                dst.put_u32(*begin);
                dst.put_u32(*length);
            }
            WireMsg::AllowedFast(p) => {
                dst.put_u32(5);
                dst.put_u8(17);
                dst.put_u32(*p);
            }
            WireMsg::Extended { ext_id, payload } => {
                dst.put_u32(2 + payload.len() as u32);
                dst.put_u8(20);
                dst.put_u8(*ext_id);
                dst.extend_from_slice(payload);
            }
        }
    }
}

pub async fn write_batch<W: AsyncWrite + Unpin>(w: &mut W, scratch: &mut BytesMut, msgs: &[WireMsg]) -> Result<()> {
    if msgs.is_empty() {
        return Ok(());
    }
    scratch.clear();
    for m in msgs {
        m.encode_into(scratch);
    }
    w.write_all(scratch).await?;
    w.flush().await?;
    Ok(())
}

pub struct PeerReader<R> {
    inner: R,
    buf: BytesMut,
}

const MAX_FRAME_LEN: usize = 4 * 1024 * 1024;

impl<R: AsyncRead + Unpin> PeerReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            buf: BytesMut::with_capacity(64 * 1024),
        }
    }

    pub async fn read_msg(&mut self) -> Result<Option<WireMsg>> {
        loop {
            if self.buf.len() >= 4 {
                let len = u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]) as usize;
                if len > MAX_FRAME_LEN {
                    bail!("oversized peer frame ({len} bytes)");
                }
                let total = 4 + len;
                if self.buf.len() >= total {
                    let mut frame = self.buf.split_to(total);
                    let _ = frame.split_to(4);
                    if len == 0 {
                        return Ok(Some(WireMsg::KeepAlive));
                    }
                    return Ok(Some(parse_frame(frame)?));
                }
                let need = total - self.buf.len();
                if self.buf.capacity() < need {
                    self.buf.reserve(need);
                }
            } else if self.buf.capacity() < 16_384 {
                self.buf.reserve(16_384);
            }
            let n = self.inner.read_buf(&mut self.buf).await?;
            if n == 0 {
                if self.buf.is_empty() {
                    return Ok(None);
                }
                bail!("peer EOF mid-frame");
            }
        }
    }
}

fn parse_frame(mut frame: BytesMut) -> Result<WireMsg> {
    let msg_id = frame[0];
    let _ = frame.split_to(1);
    match msg_id {
        0 => Ok(WireMsg::Choke),
        1 => Ok(WireMsg::Unchoke),
        2 => Ok(WireMsg::Interested),
        3 => Ok(WireMsg::NotInterested),
        4 => {
            if frame.len() != 4 {
                bail!("bad Have len {}", frame.len());
            }
            Ok(WireMsg::Have(u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]])))
        }
        5 => Ok(WireMsg::Bitfield(frame.freeze())),
        6 | 8 | 16 => {
            if frame.len() != 12 {
                bail!("bad Request/Cancel/Reject len {}", frame.len());
            }
            let index = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
            let begin = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]);
            let length = u32::from_be_bytes([frame[8], frame[9], frame[10], frame[11]]);
            match msg_id {
                6 => Ok(WireMsg::Request { index, begin, length }),
                8 => Ok(WireMsg::Cancel { index, begin, length }),
                _ => Ok(WireMsg::RejectRequest { index, begin, length }),
            }
        }
        7 => {
            if frame.len() < 8 {
                bail!("bad Piece len {}", frame.len());
            }
            let index = u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]);
            let begin = u32::from_be_bytes([frame[4], frame[5], frame[6], frame[7]]);
            let _ = frame.split_to(8);
            Ok(WireMsg::Piece { index, begin, data: frame.freeze() })
        }
        9 => {
            if frame.len() != 2 {
                bail!("bad Port len {}", frame.len());
            }
            Ok(WireMsg::Port(u16::from_be_bytes([frame[0], frame[1]])))
        }
        13 => {
            if frame.len() != 4 {
                bail!("bad SuggestPiece len {}", frame.len());
            }
            Ok(WireMsg::SuggestPiece(u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]])))
        }
        14 => Ok(WireMsg::HaveAll),
        15 => Ok(WireMsg::HaveNone),
        17 => {
            if frame.len() != 4 {
                bail!("bad AllowedFast len {}", frame.len());
            }
            Ok(WireMsg::AllowedFast(u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]])))
        }
        20 => {
            if frame.is_empty() {
                bail!("empty Extended frame");
            }
            let ext_id = frame[0];
            let _ = frame.split_to(1);
            Ok(WireMsg::Extended { ext_id, payload: frame.freeze() })
        }
        other => bail!("unsupported peer message id {other}"),
    }
}

#[derive(Debug, Clone, Default)]
pub struct PeerExtInfo {
    pub ut_metadata_id: Option<u8>,
    pub ut_pex_id: Option<u8>,
    pub metadata_size: Option<usize>,
    pub reqq: Option<u32>,
}

pub fn build_ext_handshake(metadata_size: Option<usize>) -> WireMsg {
    let mut w = BWriter::with_capacity(96);
    w.start_dict();
    w.push_str("m");
    w.start_dict();
    w.push_str("ut_metadata");
    w.push_int(MY_EXT_UT_METADATA as i64);
    w.push_str("ut_pex");
    w.push_int(MY_EXT_UT_PEX as i64);
    w.end();
    if let Some(sz) = metadata_size {
        w.push_str("metadata_size");
        w.push_int(sz as i64);
    }
    w.push_str("reqq");
    w.push_int(250);
    w.push_str("v");
    w.push_str("bitlab");
    w.end();
    WireMsg::Extended {
        ext_id: 0,
        payload: Bytes::from(w.finish()),
    }
}

pub fn parse_ext_handshake(payload: &[u8]) -> Result<PeerExtInfo> {
    let val = bencode::decode(payload).context("decode ext handshake")?;
    let mut out = PeerExtInfo::default();
    if let Some(m) = val.dict_get(b"m") {
        if let Some(id) = m.dict_i64(b"ut_metadata").and_then(|v| u8::try_from(v).ok()).filter(|&v| v > 0) {
            out.ut_metadata_id = Some(id);
        }
        if let Some(id) = m.dict_i64(b"ut_pex").and_then(|v| u8::try_from(v).ok()).filter(|&v| v > 0) {
            out.ut_pex_id = Some(id);
        }
    }
    if let Some(sz) = val.dict_i64(b"metadata_size").and_then(|v| usize::try_from(v).ok()).filter(|&v| v > 0 && v <= 16 * 1024 * 1024) {
        out.metadata_size = Some(sz);
    }
    if let Some(reqq) = val.dict_i64(b"reqq").and_then(|v| u32::try_from(v).ok()) {
        out.reqq = Some(reqq);
    }
    Ok(out)
}

pub fn build_ut_metadata_request(peer_ext_id: u8, piece: u32) -> WireMsg {
    let mut w = BWriter::with_capacity(40);
    w.start_dict();
    w.push_str("msg_type");
    w.push_int(0);
    w.push_str("piece");
    w.push_int(piece as i64);
    w.end();
    WireMsg::Extended {
        ext_id: peer_ext_id,
        payload: Bytes::from(w.finish()),
    }
}

pub fn build_ut_metadata_data(peer_ext_id: u8, piece: u32, total_size: usize, slice: &[u8]) -> WireMsg {
    let mut w = BWriter::with_capacity(64 + slice.len());
    w.start_dict();
    w.push_str("msg_type");
    w.push_int(1);
    w.push_str("piece");
    w.push_int(piece as i64);
    w.push_str("total_size");
    w.push_int(total_size as i64);
    w.end();
    w.buf.extend_from_slice(slice);
    WireMsg::Extended {
        ext_id: peer_ext_id,
        payload: Bytes::from(w.finish()),
    }
}

pub enum UtMetadataMsg {
    Request { piece: u32 },
    Data { piece: u32, total_size: Option<usize>, bytes: Bytes },
    Reject { piece: u32 },
}

pub fn parse_ut_metadata(payload: &Bytes) -> Result<UtMetadataMsg> {
    let (dict, rest) = bencode::decode_with_rest(payload).context("decode ut_metadata")?;
    let msg_type = dict.dict_i64(b"msg_type").context("ut_metadata missing msg_type")?;
    let piece = dict
        .dict_i64(b"piece")
        .and_then(|v| u32::try_from(v).ok())
        .context("ut_metadata bad piece")?;
    match msg_type {
        0 => Ok(UtMetadataMsg::Request { piece }),
        1 => {
            let total_size = dict.dict_i64(b"total_size").and_then(|v| usize::try_from(v).ok());
            let offset = payload.len() - rest.len();
            Ok(UtMetadataMsg::Data {
                piece,
                total_size,
                bytes: payload.slice(offset..),
            })
        }
        2 => Ok(UtMetadataMsg::Reject { piece }),
        other => bail!("unknown ut_metadata msg_type {other}"),
    }
}

pub fn parse_ut_pex_addrs(payload: &[u8]) -> Vec<std::net::SocketAddr> {
    let Ok(val) = bencode::decode(payload) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(added) = val.dict_bytes(b"added") {
        parse_compact_v4(added, &mut out);
    }
    if let Some(added6) = val.dict_bytes(b"added6") {
        parse_compact_v6(added6, &mut out);
    }
    out
}

pub fn parse_compact_v4(raw: &[u8], out: &mut Vec<std::net::SocketAddr>) {
    for &chunk in raw.as_chunks::<6>().0 {
        let ip = std::net::Ipv4Addr::new(chunk[0], chunk[1], chunk[2], chunk[3]);
        let port = u16::from_be_bytes([chunk[4], chunk[5]]);
        if port != 0 {
            out.push(std::net::SocketAddr::new(std::net::IpAddr::V4(ip), port));
        }
    }
}

pub fn parse_compact_v6(raw: &[u8], out: &mut Vec<std::net::SocketAddr>) {
    for &chunk in raw.as_chunks::<18>().0 {
        let mut octets = [0u8; 16];
        octets.copy_from_slice(&chunk[..16]);
        let ip = std::net::Ipv6Addr::from(octets);
        let port = u16::from_be_bytes([chunk[16], chunk[17]]);
        if port != 0 {
            out.push(std::net::SocketAddr::new(std::net::IpAddr::V6(ip), port));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_handshake_and_batched_wire_roundtrip() {
        let (client, server) = tokio::io::duplex(64 * 1024);
        let (_cr, mut cw) = tokio::io::split(client);
        let (mut sr, _sw) = tokio::io::split(server);

        let ih = Id20([0xAB; 20]);
        let hs = Handshake::new(ih, *b"-BL0001-123456789012");
        assert!(hs.supports_ext());
        assert!(hs.supports_fast());

        tokio::join!(
            async {
                write_handshake(&mut cw, &hs).await.unwrap();
                let mut scratch = BytesMut::new();
                write_batch(
                    &mut cw,
                    &mut scratch,
                    &[
                        WireMsg::HaveAll,
                        WireMsg::AllowedFast(0),
                        WireMsg::Request { index: 0, begin: 0, length: 16384 },
                        WireMsg::Piece { index: 0, begin: 0, data: Bytes::from_static(b"hello-world") },
                        build_ext_handshake(Some(42)),
                    ],
                )
                .await
                .unwrap();
                tokio::io::AsyncWriteExt::shutdown(&mut cw).await.unwrap();
                drop(cw);
            },
            async {
                let got_hs = read_handshake(&mut sr).await.unwrap();
                assert_eq!(got_hs.info_hash, ih);
                let mut reader = PeerReader::new(sr);
                assert!(matches!(reader.read_msg().await.unwrap(), Some(WireMsg::HaveAll)));
                assert!(matches!(reader.read_msg().await.unwrap(), Some(WireMsg::AllowedFast(0))));
                assert!(matches!(reader.read_msg().await.unwrap(), Some(WireMsg::Request { index: 0, begin: 0, length: 16384 })));
                match reader.read_msg().await.unwrap().unwrap() {
                    WireMsg::Piece { index: 0, begin: 0, data } => assert_eq!(&data[..], b"hello-world"),
                    other => panic!("unexpected {other:?}"),
                }
                match reader.read_msg().await.unwrap().unwrap() {
                    WireMsg::Extended { ext_id: 0, payload } => {
                        let info = parse_ext_handshake(&payload).unwrap();
                        assert_eq!(info.ut_metadata_id, Some(MY_EXT_UT_METADATA));
                        assert_eq!(info.metadata_size, Some(42));
                    }
                    other => panic!("unexpected {other:?}"),
                }
                assert!(reader.read_msg().await.unwrap().is_none());
            }
        );
    }
}
