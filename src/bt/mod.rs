mod bencode;
mod dht;
mod metainfo;
mod storage;
mod torrent;
mod tracker;
mod transport;
mod upnp;
mod wire;

use anyhow::{bail, Result};
use rand::RngCore;
use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::atomic::AtomicI64;
use std::sync::{Arc, RwLock};

use dht::{DhtConfig, DhtNode};
pub use metainfo::Id20;
pub use torrent::TorrentHandle;
use torrent::TorrentDeps;
use tracker::TrackerClient;
use transport::{TransportConfig, TransportManager};
use upnp::UpnpGuard;

pub struct BtConfig {
    pub media_dir: PathBuf,
    pub listen_port: u16,
    pub enable_dht: bool,
    pub enable_upnp: bool,
    pub enable_utp: bool,
    pub bind_v4_only: bool,
    pub allow_private: bool,
    pub http: reqwest::Client,
    pub readahead: Arc<AtomicI64>,
}

pub struct BtSession {
    cfg: BtConfig,
    dht: Option<Arc<DhtNode>>,
    tracker: Arc<TrackerClient>,
    transport: Arc<TransportManager>,
    _upnp: Option<UpnpGuard>,
    torrents: RwLock<HashMap<Id20, Arc<TorrentHandle>>>,
}

impl BtSession {
    pub async fn start(cfg: BtConfig) -> Result<Arc<Self>> {
        let mut peer_id = *b"-BL0001-000000000000";
        rand::rng().fill_bytes(&mut peer_id[8..]);

        let transport = TransportManager::start(
            TransportConfig {
                listen_port: cfg.listen_port,
                bind_v4_only: cfg.bind_v4_only,
                enable_utp: cfg.enable_utp,
                allow_private: cfg.allow_private,
                ..Default::default()
            },
            peer_id,
        )
        .await?;

        let actual_port = transport.listen_port();
        let dht = if cfg.enable_dht {
            let dht_cfg = DhtConfig {
                state_path: Some(cfg.media_dir.join("dht.json")),
                bind_ip: if cfg.bind_v4_only {
                    Ipv4Addr::LOCALHOST
                } else {
                    Ipv4Addr::UNSPECIFIED
                },
                port: actual_port,
                enable_ipv6: !cfg.bind_v4_only,
                use_default_bootstrap: !cfg.allow_private,
                bootstrap_addrs: Vec::new(),
                allow_private: cfg.allow_private,
                ..Default::default()
            };
            match DhtNode::start(dht_cfg).await {
                Ok(n) => Some(n),
                Err(e) => {
                    tracing::warn!("dht start failed (continuing without dht): {e}");
                    None
                }
            }
        } else {
            None
        };

        let _upnp = if cfg.enable_upnp && cfg.listen_port != 0 {
            UpnpGuard::spawn(actual_port)
        } else {
            None
        };

        let tracker = TrackerClient::new(cfg.http.clone(), cfg.allow_private).await?;

        Ok(Arc::new(Self {
            cfg,
            dht,
            tracker,
            transport,
            _upnp,
            torrents: RwLock::new(HashMap::new()),
        }))
    }

    pub fn readahead_handle(&self) -> Arc<AtomicI64> {
        self.cfg.readahead.clone()
    }

    pub async fn add_magnet_or_hash(
        &self,
        raw: &str,
        extra_trackers: &[String],
        save_dir: PathBuf,
        paused: bool,
    ) -> Result<Arc<TorrentHandle>> {
        let trimmed = raw.trim();
        let magnet = if trimmed.to_ascii_lowercase().starts_with("magnet:") {
            metainfo::parse_magnet(trimmed)?
        } else {
            let ih = Id20::from_hex(trimmed)?;
            metainfo::MagnetLink {
                info_hash: ih,
                name: None,
                trackers: Vec::new(),
                peer_hints: Vec::new(),
            }
        };

        if let Some(existing) = self.torrents.read().unwrap().get(&magnet.info_hash).cloned() {
            existing.add_peers(magnet.peer_hints);
            if !paused {
                existing.unpause();
            }
            return Ok(existing);
        }

        let initial_meta = storage::read_saved_meta(&save_dir)
            .and_then(|raw_info| {
                metainfo::TorrentMeta::from_info_bytes(raw_info, Some(magnet.info_hash)).ok()
            })
            .map(Arc::new);

        let mut trackers = magnet.trackers;
        for t in extra_trackers {
            if !trackers.contains(t) {
                trackers.push(t.clone());
            }
        }

        let handle = TorrentHandle::spawn(
            magnet.info_hash,
            magnet.name,
            save_dir,
            trackers,
            magnet.peer_hints,
            initial_meta,
            paused,
            self.make_deps(),
        );
        self.torrents
            .write()
            .unwrap()
            .insert(magnet.info_hash, handle.clone());
        Ok(handle)
    }

    pub async fn add_torrent_bytes(
        &self,
        torrent_bytes: &[u8],
        expected_ih: Option<Id20>,
        extra_trackers: &[String],
        save_dir: PathBuf,
        paused: bool,
    ) -> Result<Arc<TorrentHandle>> {
        let meta = metainfo::TorrentMeta::from_torrent_bytes(torrent_bytes)?;
        if let Some(exp) = expected_ih {
            if meta.info_hash != exp {
                bail!(
                    "torrent info_hash mismatch: expected {}, got {}",
                    exp.as_string(),
                    meta.info_hash.as_string()
                );
            }
        }

        if let Some(existing) = self.torrents.read().unwrap().get(&meta.info_hash).cloned() {
            existing.inject_meta_bytes(meta.info_bytes);
            if !paused {
                existing.unpause();
            }
            return Ok(existing);
        }

        let mut trackers = meta.trackers.clone();
        for t in extra_trackers {
            if !trackers.contains(t) {
                trackers.push(t.clone());
            }
        }
        let ih = meta.info_hash;
        let handle = TorrentHandle::spawn(
            ih,
            Some(meta.name.clone()),
            save_dir,
            trackers,
            Vec::new(),
            Some(Arc::new(meta)),
            paused,
            self.make_deps(),
        );
        self.torrents.write().unwrap().insert(ih, handle.clone());
        Ok(handle)
    }

    pub fn get(&self, ih: &Id20) -> Option<Arc<TorrentHandle>> {
        self.torrents.read().unwrap().get(ih).cloned()
    }

    pub fn unpause(&self, ih: &Id20) {
        if let Some(h) = self.get(ih) {
            h.unpause();
        }
    }

    pub fn delete(&self, ih: &Id20) {
        if let Some(h) = self.torrents.write().unwrap().remove(ih) {
            h.shutdown();
        }
    }

    pub fn shutdown(&self) {
        let handles: Vec<Arc<TorrentHandle>> = self
            .torrents
            .write()
            .unwrap()
            .drain()
            .map(|(_, h)| h)
            .collect();
        for h in handles {
            h.shutdown();
        }
        if let Some(dht) = &self.dht {
            dht.save_state();
        }
    }

    fn make_deps(&self) -> Arc<TorrentDeps> {
        Arc::new(TorrentDeps {
            dht: self.dht.clone(),
            tracker: self.tracker.clone(),
            transport: self.transport.clone(),
            readahead: self.cfg.readahead.clone(),
            allow_private: self.cfg.allow_private,
        })
    }
}

impl Drop for BtSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bencode::BWriter;
    use futures::StreamExt;
    use sha1::{Digest, Sha1};

    #[tokio::test]
    async fn test_bt_session_torrent_bytes_and_magnet_race() {
        let base = std::env::temp_dir().join(format!("bitlab_sess_{}", rand::random::<u32>()));
        let _ = std::fs::create_dir_all(&base);
        let payload = vec![0x5Cu8; 40 * 1024];
        let mut pieces_cat = Vec::new();
        for ch in payload.chunks(32 * 1024) {
            pieces_cat.extend_from_slice(&Sha1::digest(ch));
        }

        let mut bw = BWriter::with_capacity(256);
        bw.start_dict();
        bw.push_str("info");
        bw.start_dict();
        bw.push_str("length");
        bw.push_int(payload.len() as i64);
        bw.push_str("name");
        bw.push_str("sample.bin");
        bw.push_str("piece length");
        bw.push_int(32 * 1024);
        bw.push_str("pieces");
        bw.push_bytes(&pieces_cat);
        bw.end();
        bw.end();
        let dot_torrent = bw.finish();

        let sess = BtSession::start(BtConfig {
            media_dir: base.clone(),
            listen_port: 0,
            enable_dht: false,
            enable_upnp: false,
            enable_utp: true,
            bind_v4_only: true,
            allow_private: true,
            http: reqwest::Client::new(),
            readahead: Arc::new(AtomicI64::new(8 << 20)),
        })
        .await
        .unwrap();

        let h = sess
            .add_torrent_bytes(&dot_torrent, None, &[], base.join("t1"), false)
            .await
            .unwrap();
        let meta = h.wait_metadata(std::time::Duration::from_secs(2)).await.unwrap();
        assert_eq!(meta.name, "sample.bin");
        assert_eq!(h.files().len(), 1);

        // Verify injecting `.torrent` bytes into a live magnet handle wakes `wait_metadata` immediately.
        let ih_hex = meta.info_hash.as_string();
        assert!(sess.get(&meta.info_hash).is_some());

        let sess2 = BtSession::start(BtConfig {
            media_dir: base.join("s2"),
            listen_port: 0,
            enable_dht: false,
            enable_upnp: false,
            enable_utp: false,
            bind_v4_only: true,
            allow_private: true,
            http: reqwest::Client::new(),
            readahead: Arc::new(AtomicI64::new(8 << 20)),
        })
        .await
        .unwrap();
        let hs2 = sess2
            .add_magnet_or_hash(&ih_hex, &[], base.join("t3"), false)
            .await
            .unwrap();
        assert!(hs2.files().is_empty());

        let waiter = {
            let hs2_c = hs2.clone();
            tokio::spawn(async move { hs2_c.wait_metadata(std::time::Duration::from_secs(2)).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        let injected = sess2
            .add_torrent_bytes(&dot_torrent, Some(meta.info_hash), &[], base.join("t3"), false)
            .await
            .unwrap();
        assert_eq!(injected.info_hash(), meta.info_hash);

        let waited_meta = waiter.await.unwrap().unwrap();
        assert_eq!(waited_meta.name, "sample.bin");
        assert_eq!(hs2.files().len(), 1);
        let st_check = hs2.open_stream(0, 0, 1);
        assert!(st_check.is_ok());
        drop(st_check);

        // Exercise zero-copy stream close on shutdown cleanly.
        let mut s = hs2.open_stream(0, 0, payload.len() as u64).unwrap();
        sess2.delete(&meta.info_hash);
        let item = s.next().await;
        assert!(matches!(item, Some(Err(_))));

        sess.shutdown();
        sess2.shutdown();
        let _ = std::fs::remove_dir_all(&base);
    }
}
