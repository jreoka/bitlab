use torrent::metainfo::Mode;

pub fn torrent_name(meta: &torrent::metainfo::Metainfo) -> String {
    match &meta.info.mode {
        Mode::Single { name, .. } => name.clone(),
        Mode::Multiple { name, .. } => name.clone(),
    }
}

pub fn torrent_files(meta: &torrent::metainfo::Metainfo) -> (Vec<String>, Vec<i64>) {
    match &meta.info.mode {
        Mode::Single { name, length } => (vec![name.clone()], vec![*length as i64]),
        Mode::Multiple { files, .. } => {
            let mut p = Vec::with_capacity(files.len());
            let mut s = Vec::with_capacity(files.len());
            for f in files {
                p.push(f.path.join("/"));
                s.push(f.length as i64);
            }
            (p, s)
        }
    }
}

pub fn torrent_total(meta: &torrent::metainfo::Metainfo) -> i64 {
    meta.info.total_size() as i64
}
