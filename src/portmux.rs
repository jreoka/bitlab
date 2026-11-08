pub const BT_PROTOCOL: &str = "BitTorrent protocol";

pub const HTTP_METHODS: &[&str] = &[
    "GET ", "HEAD ", "POST ", "PUT ", "PATCH ", "DELETE ", "OPTIONS ", "TRACE ", "CONNECT ", "PRI ",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SniffTarget {
    Http,
    Torrent,
}

/// Classify the first bytes of a connection, mirroring Go's portmux sniff:
/// 0x13 + "BitTorrent protocol" goes to the torrent client; anything that
/// positively matches an HTTP method token goes to HTTP; everything else
/// (MSE-encrypted handshakes look like noise) goes to the torrent client.
pub fn classify_prefix(buf: &[u8]) -> Option<SniffTarget> {
    if buf.is_empty() {
        return None;
    }
    if buf[0] == BT_PROTOCOL.len() as u8 {
        if buf.len() < 1 + BT_PROTOCOL.len() {
            return None; // need more bytes
        }
        return Some(SniffTarget::Torrent);
    }
    let s = String::from_utf8_lossy(buf);
    let s = s.as_ref();
    for m in HTTP_METHODS {
        if *s == **m {
            return Some(SniffTarget::Http);
        }
        // exact prefix "GET " with trailing space already handled; longer buf
        if s.len() >= m.len() && &s[..m.len()] == *m {
            return Some(SniffTarget::Http);
        }
    }
    // Could still be a prefix of a method?
    let mut possible = false;
    for m in HTTP_METHODS {
        if m.starts_with(s) {
            possible = true;
            break;
        }
    }
    if possible && buf.len() < 8 {
        return None; // need more bytes
    }
    Some(SniffTarget::Torrent)
}

/// Read the classification prefix from an async stream with a timeout.
pub async fn sniff_stream<S>(stream: &mut S, timeout: std::time::Duration) -> anyhow::Result<(SniffTarget, Vec<u8>)>
where
    S: tokio::io::AsyncReadExt + Unpin,
{
    let mut buf: Vec<u8> = vec![];
    let mut one = [0u8; 1];
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => anyhow::bail!("sniff timeout"),
            r = stream.read_exact(&mut one) => {
                r?;
                buf.push(one[0]);
                if buf[0] == BT_PROTOCOL.len() as u8 {
                    if buf.len() >= 1 + BT_PROTOCOL.len() {
                        return Ok((SniffTarget::Torrent, buf));
                    }
                    continue;
                }
                match classify_prefix(&buf) {
                    Some(t) => return Ok((t, buf)),
                    None => {
                        if buf.len() >= 32 {
                            return Ok((SniffTarget::Torrent, buf));
                        }
                        continue;
                    }
                }
            }
        }
    }
}
