use anyhow::{bail, Context, Result};
use bytes::{Bytes, BytesMut};
use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, watch, Notify};

use super::metainfo::TorrentMeta;

const HAVE_MAGIC: &[u8; 8] = b"BLHAVE01";
const MIN_RAM_CACHE_BYTES: usize = 8 * 1024 * 1024;
const MAX_RAM_CACHE_BYTES: usize = 512 * 1024 * 1024;

struct RamCache {
    pieces: HashMap<usize, (Bytes, u64)>,
    total_bytes: usize,
    seq: u64,
}

impl RamCache {
    fn new() -> Self {
        Self {
            pieces: HashMap::new(),
            total_bytes: 0,
            seq: 0,
        }
    }

    fn get(&mut self, piece: usize) -> Option<Bytes> {
        self.seq = self.seq.wrapping_add(1);
        let s = self.seq;
        if let Some((b, touched)) = self.pieces.get_mut(&piece) {
            *touched = s;
            return Some(b.clone());
        }
        None
    }

    fn insert(&mut self, piece: usize, data: Bytes, budget: usize, cursor_piece: usize) {
        self.seq = self.seq.wrapping_add(1);
        let s = self.seq;
        if let Some((old, touched)) = self.pieces.get_mut(&piece) {
            self.total_bytes = self.total_bytes.saturating_sub(old.len());
            self.total_bytes = self.total_bytes.saturating_add(data.len());
            *old = data;
            *touched = s;
        } else {
            self.total_bytes = self.total_bytes.saturating_add(data.len());
            self.pieces.insert(piece, (data, s));
        }

        while self.total_bytes > budget && self.pieces.len() > 2 {
            // Protect pieces inside active forward playback window first.
            let protect_end = cursor_piece.saturating_add(32);
            let evict_key = self
                .pieces
                .iter()
                .filter(|(p, _)| **p != piece && (**p < cursor_piece || **p > protect_end))
                .min_by_key(|(_, (_, touched))| *touched)
                .map(|(p, _)| *p)
                .or_else(|| {
                    self.pieces
                        .iter()
                        .filter(|(p, _)| **p != piece)
                        .min_by_key(|(_, (_, touched))| *touched)
                        .map(|(p, _)| *p)
                });
            let Some(k) = evict_key else {
                break;
            };
            if let Some((removed, _)) = self.pieces.remove(&k) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.len());
            }
        }
    }
}

enum FlushCmd {
    WritePiece { piece: usize, data: Bytes },
    SaveHave,
    Sync(oneshot::Sender<()>),
    Shutdown,
}

pub struct TorrentStorage {
    save_dir: PathBuf,
    meta: Arc<TorrentMeta>,
    readahead: Arc<AtomicI64>,
    have: Mutex<Vec<bool>>,
    selected_files: Mutex<HashSet<usize>>,
    ram: Mutex<RamCache>,
    piece_notify: Notify,
    flush_tx: mpsc::UnboundedSender<FlushCmd>,
    dirty_since_save: AtomicBool,
}

impl TorrentStorage {
    pub fn open(
        save_dir: PathBuf,
        meta: Arc<TorrentMeta>,
        readahead: Arc<AtomicI64>,
    ) -> Result<Arc<Self>> {
        std::fs::create_dir_all(&save_dir).context("create torrent save dir")?;
        write_meta_if_missing(&save_dir, &meta.info_bytes);

        let num_pieces = meta.num_pieces();
        let initial_have = load_have_bitmap(&save_dir, num_pieces);
        let (flush_tx, flush_rx) = mpsc::unbounded_channel();

        let storage = Arc::new(Self {
            save_dir: save_dir.clone(),
            meta: meta.clone(),
            readahead,
            have: Mutex::new(initial_have),
            selected_files: Mutex::new(HashSet::new()),
            ram: Mutex::new(RamCache::new()),
            piece_notify: Notify::new(),
            flush_tx,
            dirty_since_save: AtomicBool::new(false),
        });

        let weak = Arc::downgrade(&storage);
        std::thread::Builder::new()
            .name("bt-disk-flush".into())
            .spawn(move || disk_flush_worker(weak, flush_rx))
            .context("spawn bt-disk-flush thread")?;

        Ok(storage)
    }

    pub fn meta(&self) -> &Arc<TorrentMeta> {
        &self.meta
    }

    pub fn select_file(&self, file_idx: usize) -> Result<()> {
        let Some(entry) = self.meta.file_infos.get(file_idx) else {
            bail!("file index {file_idx} out of bounds");
        };
        let is_new = self.selected_files.lock().unwrap().insert(file_idx);
        if is_new && !entry.is_padding {
            let disk_path = self.save_dir.join(&entry.relative_filename);
            if !disk_path.exists() && entry.len > 0 {
                // Ensure any pieces overlapping an uncreated file (e.g., boundary pieces
                // previously verified for a sibling file) are fetched cleanly for this file.
                if let Some(range) = self.meta.file_range_to_pieces(file_idx, 0, entry.len) {
                    let mut h = self.have.lock().unwrap();
                    for p in range {
                        if p < h.len() {
                            h[p] = false;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub fn have_piece(&self, piece: usize) -> bool {
        self.have.lock().unwrap().get(piece).copied().unwrap_or(false)
    }

    pub fn have_count(&self) -> usize {
        self.have.lock().unwrap().iter().filter(|&&b| b).count()
    }

    pub fn is_complete(&self) -> bool {
        let h = self.have.lock().unwrap();
        !h.is_empty() && h.iter().all(|&b| b)
    }

    pub fn completed_selected_bytes(&self) -> u64 {
        let selected: Vec<usize> = self.selected_files.lock().unwrap().iter().copied().collect();
        let have = self.have.lock().unwrap();
        let mut total = 0u64;
        for (p, &ok) in have.iter().enumerate() {
            if !ok {
                continue;
            }
            for sl in self.meta.piece_to_file_slices(p) {
                if selected.is_empty() || selected.contains(&sl.file_idx) {
                    total = total.saturating_add(sl.len as u64);
                }
            }
        }
        total
    }

    pub fn have_bitfield_bytes(&self) -> Vec<u8> {
        let h = self.have.lock().unwrap();
        let mut out = vec![0u8; h.len().div_ceil(8)];
        for (i, &bit) in h.iter().enumerate() {
            if bit {
                out[i / 8] |= 1 << (7 - (i % 8));
            }
        }
        out
    }

    pub fn mark_verified_and_flush(&self, piece: usize, data: Bytes, cursor_piece: usize) {
        let budget = self.ram_budget();
        {
            let mut ram = self.ram.lock().unwrap();
            ram.insert(piece, data.clone(), budget, cursor_piece);
        }
        {
            let mut h = self.have.lock().unwrap();
            if piece < h.len() {
                h[piece] = true;
            }
        }
        self.dirty_since_save.store(true, Ordering::Relaxed);
        self.piece_notify.notify_waiters();
        let _ = self.flush_tx.send(FlushCmd::WritePiece { piece, data });
    }

    pub async fn wait_piece(&self, piece: usize, cancel: &mut watch::Receiver<bool>) -> bool {
        loop {
            let notified = self.piece_notify.notified();
            if self.have_piece(piece) {
                return true;
            }
            if *cancel.borrow() {
                return false;
            }
            tokio::select! {
                _ = notified => {}
                _ = cancel.changed() => {
                    if *cancel.borrow() {
                        return false;
                    }
                }
            }
        }
    }

    pub async fn read_piece_slice(&self, piece: usize, offset: usize, len: usize) -> Result<Bytes> {
        if len == 0 {
            return Ok(Bytes::new());
        }
        let plen = self.meta.piece_len(piece) as usize;
        if offset.saturating_add(len) > plen {
            bail!("piece {piece} slice {offset}+{len} out of bounds ({plen})");
        }

        if let Some(full) = self.ram.lock().unwrap().get(piece) {
            if offset + len <= full.len() {
                return Ok(full.slice(offset..offset + len));
            }
        }

        // Flush pending disk writes before reading evicted range from disk so disk matches `have`.
        self.flush_pending().await;

        let meta = self.meta.clone();
        let save_dir = self.save_dir.clone();
        let selected: HashSet<usize> = self.selected_files.lock().unwrap().clone();
        tokio::task::spawn_blocking(move || {
            read_piece_slice_from_disk(&save_dir, &meta, &selected, piece, offset, len)
        })
        .await
        .context("disk read task panicked")?
    }

    pub fn save_fastresume_now(&self) {
        let _ = self.flush_tx.send(FlushCmd::SaveHave);
    }

    pub async fn flush_pending(&self) {
        let (tx, rx) = oneshot::channel();
        if self.flush_tx.send(FlushCmd::Sync(tx)).is_ok() {
            let _ = rx.await;
        }
    }

    fn ram_budget(&self) -> usize {
        let ra = self.readahead.load(Ordering::Relaxed).max(0) as usize;
        // Double the readahead window for verified backward+forward cache, within bounds.
        (ra.saturating_mul(2)).clamp(MIN_RAM_CACHE_BYTES, MAX_RAM_CACHE_BYTES)
    }
}

impl Drop for TorrentStorage {
    fn drop(&mut self) {
        let _ = self.flush_tx.send(FlushCmd::SaveHave);
        let _ = self.flush_tx.send(FlushCmd::Shutdown);
    }
}

fn disk_flush_worker(weak: std::sync::Weak<TorrentStorage>, mut rx: mpsc::UnboundedReceiver<FlushCmd>) {
    let mut open_files: HashMap<usize, File> = HashMap::new();
    let mut writes_since_save = 0usize;
    let mut last_save = Instant::now();

    while let Some(cmd) = rx.blocking_recv() {
        let Some(st) = weak.upgrade() else {
            break;
        };
        match cmd {
            FlushCmd::WritePiece { piece, data } => {
                if let Err(e) = write_piece_slices(&st, &mut open_files, piece, &data) {
                    tracing::warn!("disk write piece {piece} failed: {e}");
                } else {
                    writes_since_save += 1;
                }
                if writes_since_save >= 16 || last_save.elapsed() >= Duration::from_secs(1) {
                    persist_have_bitmap(&st);
                    writes_since_save = 0;
                    last_save = Instant::now();
                }
            }
            FlushCmd::SaveHave => {
                if st.dirty_since_save.swap(false, Ordering::Relaxed) {
                    persist_have_bitmap(&st);
                    writes_since_save = 0;
                    last_save = Instant::now();
                }
            }
            FlushCmd::Sync(tx) => {
                for f in open_files.values() {
                    let _ = f.sync_data();
                }
                if st.dirty_since_save.swap(false, Ordering::Relaxed) {
                    persist_have_bitmap(&st);
                }
                let _ = tx.send(());
            }
            FlushCmd::Shutdown => {
                persist_have_bitmap(&st);
                break;
            }
        }
    }
}

fn write_piece_slices(
    st: &TorrentStorage,
    open_files: &mut HashMap<usize, File>,
    piece: usize,
    data: &[u8],
) -> Result<()> {
    let selected = st.selected_files.lock().unwrap().clone();
    for sl in st.meta.piece_to_file_slices(piece) {
        let entry = &st.meta.file_infos[sl.file_idx];
        if entry.is_padding {
            continue;
        }
        if !selected.is_empty() && !selected.contains(&sl.file_idx) {
            continue;
        }
        let end = sl.piece_offset.saturating_add(sl.len);
        if end > data.len() {
            bail!("piece {piece} slice out of bounds");
        }
        let file = get_or_open_sparse_file(st, open_files, sl.file_idx)?;
        file.seek(SeekFrom::Start(sl.file_offset))?;
        file.write_all(&data[sl.piece_offset..end])?;
    }
    Ok(())
}

fn get_or_open_sparse_file<'a>(
    st: &TorrentStorage,
    open_files: &'a mut HashMap<usize, File>,
    file_idx: usize,
) -> Result<&'a mut File> {
    if let std::collections::hash_map::Entry::Vacant(e) = open_files.entry(file_idx) {
        let entry = &st.meta.file_infos[file_idx];
        let full = st.save_dir.join(&entry.relative_filename);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let existed = full.exists();
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&full)
            .with_context(|| format!("open sparse file {}", full.display()))?;
        #[cfg(windows)]
        mark_sparse_windows(&file);
        if !existed || file.metadata().map(|m| m.len()).unwrap_or(0) != entry.len {
            file.set_len(entry.len)?;
        }
        e.insert(file);
    }
    Ok(open_files.get_mut(&file_idx).unwrap())
}

#[cfg(windows)]
fn mark_sparse_windows(file: &File) {
    use std::os::windows::io::AsRawHandle;
    const FSCTL_SET_SPARSE: u32 = 0x0009_00C4;
    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            h_device: *mut std::ffi::c_void,
            dw_io_control_code: u32,
            lp_in_buffer: *const std::ffi::c_void,
            n_in_buffer_size: u32,
            lp_out_buffer: *mut std::ffi::c_void,
            n_out_buffer_size: u32,
            lp_bytes_returned: *mut u32,
            lp_overlapped: *mut std::ffi::c_void,
        ) -> i32;
    }
    let mut returned: u32 = 0;
    unsafe {
        let _ = DeviceIoControl(
            file.as_raw_handle(),
            FSCTL_SET_SPARSE,
            std::ptr::null(),
            0,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        );
    }
}

fn read_piece_slice_from_disk(
    save_dir: &Path,
    meta: &TorrentMeta,
    selected: &HashSet<usize>,
    piece: usize,
    offset: usize,
    len: usize,
) -> Result<Bytes> {
    let mut out = BytesMut::with_capacity(len);
    out.resize(len, 0);
    let req_end = offset + len;

    for sl in meta.piece_to_file_slices(piece) {
        let sl_start = sl.piece_offset;
        let sl_end = sl.piece_offset + sl.len;
        let ov_start = sl_start.max(offset);
        let ov_end = sl_end.min(req_end);
        if ov_start >= ov_end {
            continue;
        }
        let entry = &meta.file_infos[sl.file_idx];
        if entry.is_padding || (!selected.is_empty() && !selected.contains(&sl.file_idx)) {
            continue;
        }
        let file_pos = sl.file_offset + (ov_start - sl_start) as u64;
        let dst = &mut out[ov_start - offset..ov_end - offset];
        let path = save_dir.join(&entry.relative_filename);
        let file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        read_exact_at(&file, file_pos, dst)?;
    }

    Ok(out.freeze())
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut offset: u64, mut dst: &mut [u8]) -> Result<()> {
    use std::os::unix::fs::FileExt;
    while !dst.is_empty() {
        let n = file.read_at(dst, offset)?;
        if n == 0 {
            bail!("unexpected EOF reading piece slice");
        }
        offset += n as u64;
        dst = &mut dst[n..];
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut offset: u64, mut dst: &mut [u8]) -> Result<()> {
    use std::os::windows::fs::FileExt;
    while !dst.is_empty() {
        let n = file.seek_read(dst, offset)?;
        if n == 0 {
            bail!("unexpected EOF reading piece slice");
        }
        offset += n as u64;
        dst = &mut dst[n..];
    }
    Ok(())
}

pub fn write_meta_if_missing(save_dir: &Path, info_bytes: &[u8]) {
    let p = save_dir.join(".bitlab_meta");
    if p.exists() {
        return;
    }
    let _ = std::fs::create_dir_all(save_dir);
    let tmp = save_dir.join(".bitlab_meta.tmp");
    if std::fs::write(&tmp, info_bytes).is_ok() {
        let _ = std::fs::rename(&tmp, p);
    }
}

pub fn read_saved_meta(save_dir: &Path) -> Option<Bytes> {
    let bytes = std::fs::read(save_dir.join(".bitlab_meta")).ok()?;
    Some(Bytes::from(bytes))
}

fn load_have_bitmap(save_dir: &Path, num_pieces: usize) -> Vec<bool> {
    let mut out = vec![false; num_pieces];
    let Ok(bytes) = std::fs::read(save_dir.join(".bitlab_have")) else {
        return out;
    };
    let expected_len = 8 + 4 + num_pieces.div_ceil(8);
    if bytes.len() != expected_len || &bytes[..8] != HAVE_MAGIC {
        return out;
    }
    let stored_n = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;
    if stored_n != num_pieces {
        return out;
    }
    let bits = &bytes[12..];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = (bits[i / 8] & (1 << (7 - (i % 8)))) != 0;
    }
    out
}

fn persist_have_bitmap(st: &TorrentStorage) {
    let num_pieces = st.meta.num_pieces();
    let bitfield = st.have_bitfield_bytes();
    let mut buf = Vec::with_capacity(12 + bitfield.len());
    buf.extend_from_slice(HAVE_MAGIC);
    buf.extend_from_slice(&(num_pieces as u32).to_be_bytes());
    buf.extend_from_slice(&bitfield);
    let tmp = st.save_dir.join(".bitlab_have.tmp");
    let dst = st.save_dir.join(".bitlab_have");
    if std::fs::write(&tmp, &buf).is_ok() {
        let _ = std::fs::rename(&tmp, dst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bt::bencode::BWriter;
    use sha1::{Digest, Sha1};

    #[tokio::test]
    async fn test_sparse_selected_only_and_ram_eviction_fallback() {
        let dir = std::env::temp_dir().join(format!("bitlab_st_test_{}", rand::random::<u32>()));
        let _ = std::fs::create_dir_all(&dir);

        // Two 20KB files with 32KB pieces => boundary piece 0 spans file 0 (last 12KB) & file 1 (first 12KB).
        let p0 = vec![0xAAu8; 32 * 1024];
        let p1 = vec![0xBBu8; 8 * 1024];
        let mut pieces_cat = Vec::new();
        pieces_cat.extend_from_slice(&Sha1::digest(&p0));
        pieces_cat.extend_from_slice(&Sha1::digest(&p1));

        let mut bw = BWriter::with_capacity(256);
        bw.start_dict();
        bw.push_str("files");
        bw.start_list();
        for (idx, len) in [(20 * 1024i64), (20 * 1024i64)].iter().enumerate() {
            bw.start_dict();
            bw.push_str("length");
            bw.push_int(*len);
            bw.push_str("path");
            bw.start_list();
            bw.push_str(&format!("ep{idx}.bin"));
            bw.end();
            bw.end();
        }
        bw.end();
        bw.push_str("name");
        bw.push_str("pack");
        bw.push_str("piece length");
        bw.push_int(32 * 1024);
        bw.push_str("pieces");
        bw.push_bytes(&pieces_cat);
        bw.end();

        let meta = Arc::new(TorrentMeta::from_info_bytes(Bytes::from(bw.finish()), None).unwrap());
        let readahead = Arc::new(AtomicI64::new(1 << 20));
        let st = TorrentStorage::open(dir.clone(), meta.clone(), readahead).unwrap();

        // Only select file 0.
        st.select_file(0).unwrap();
        st.mark_verified_and_flush(0, Bytes::from(p0.clone()), 0);
        st.flush_pending().await;

        assert!(dir.join("pack/ep0.bin").exists());
        assert!(!dir.join("pack/ep1.bin").exists());

        // Clear RAM cache manually to exercise disk fallback pread.
        st.ram.lock().unwrap().pieces.clear();
        let slice = st.read_piece_slice(0, 1024, 2048).await.unwrap();
        assert_eq!(slice.as_ref(), &vec![0xAAu8; 2048][..]);

        // Reopen fastresume and check selecting unselected ep1 invalidates boundary piece 0 cleanly.
        drop(st);
        let st2 = TorrentStorage::open(dir.clone(), meta, Arc::new(AtomicI64::new(1 << 20))).unwrap();
        assert!(st2.have_piece(0));
        st2.select_file(1).unwrap();
        assert!(!st2.have_piece(0));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
