pub fn allocated_size(path: &std::path::Path, md: &std::fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        let _ = path;
        use std::os::unix::fs::MetadataExt;
        let blocks = md.blocks() as i64;
        // st_blocks is in 512-byte units.
        // For sparse files, st_blocks correctly reflects only the blocks actually allocated on disk.
        // If a file is completely unwritten/sparse (or empty), blocks is 0, meaning 0 bytes allocated.
        blocks * 512
    }
    #[cfg(windows)]
    {
        if md.file_type().is_dir() {
            return 0;
        }
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        extern "system" {
            fn GetCompressedFileSizeW(lpFileName: *const u16, lpFileSizeHigh: *mut u32) -> u32;
            fn GetLastError() -> u32;
            fn SetLastError(dwErrCode: u32);
        }
        let mut high: u32 = 0;
        unsafe { SetLastError(0) };
        let low = unsafe { GetCompressedFileSizeW(wide.as_ptr(), &mut high) };
        const INVALID_FILE_SIZE: u32 = 0xFFFFFFFF;
        if low == INVALID_FILE_SIZE && unsafe { GetLastError() } != 0 {
            md.len() as i64
        } else {
            (((high as u64) << 32) | (low as u64)) as i64
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        md.len() as i64
    }
}

pub fn free_disk_space(path: &std::path::Path) -> anyhow::Result<i64> {
    #[cfg(unix)]
    {
        // Use libc statvfs via std? libc crate is available.
        let cpath = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| anyhow::anyhow!("{e}"))?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::statvfs(cpath.as_ptr(), &mut stat as *mut _) };
        if r != 0 {
            anyhow::bail!("statvfs failed");
        }
        let avail = (stat.f_bavail as u128).saturating_mul(stat.f_frsize as u128);
        Ok(avail.min(i64::MAX as u128) as i64)
    }
    #[cfg(windows)]
    {
        // Use sysinfo crate for cross-platform disk query.
        use sysinfo::Disks;
        fn strip_verbatim(p: &std::path::Path) -> std::path::PathBuf {
            let s = p.to_string_lossy();
            if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
                std::path::PathBuf::from(format!(r"\\{rest}"))
            } else if let Some(rest) = s.strip_prefix(r"\\?\") {
                std::path::PathBuf::from(rest)
            } else {
                p.to_path_buf()
            }
        }
        let disks = Disks::new_with_refreshed_list();
        let target = strip_verbatim(&path.canonicalize().unwrap_or_else(|_| path.to_path_buf()));
        let mut best: Option<(usize, i64)> = None;
        for d in disks.list() {
            let mp = strip_verbatim(d.mount_point());
            let mp_len = mp.as_os_str().len();
            if mp_len > 0 && target.starts_with(&mp) {
                let avail = d.available_space().min(i64::MAX as u64) as i64;
                match best {
                    None => best = Some((mp_len, avail)),
                    Some((best_len, _)) if mp_len > best_len => best = Some((mp_len, avail)),
                    _ => {}
                }
            }
        }
        if let Some((_, v)) = best {
            return Ok(v);
        }
        if let Some(d) = disks.list().first() {
            return Ok(d.available_space().min(i64::MAX as u64) as i64);
        }
        anyhow::bail!("free disk space not supported")
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        anyhow::bail!("free disk space not supported")
    }
}

pub async fn remove_all_async(path: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        // Clear read-only bits first (torrent storage marks files read-only).
        let root = path.to_path_buf();
        let _ = tokio::task::spawn_blocking(move || {
            if root.is_dir() {
                for entry in walkdir::WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
                    if entry.file_type().is_file() {
                        if let Ok(md) = entry.metadata() {
                            let mut perms = md.permissions();
                            #[allow(clippy::permissions_set_readonly_false)]
                            perms.set_readonly(false);
                            let _ = std::fs::set_permissions(entry.path(), perms);
                        }
                    }
                }
            } else if root.is_file() {
                if let Ok(md) = std::fs::metadata(&root) {
                    let mut p = md.permissions();
                    #[allow(clippy::permissions_set_readonly_false)]
                    p.set_readonly(false);
                    let _ = std::fs::set_permissions(&root, p);
                }
            }
        })
        .await;
    }
    let mut last_err = None;
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        if let Ok(md) = tokio::fs::symlink_metadata(path).await {
            if !md.file_type().is_dir() {
                match tokio::fs::remove_file(path).await {
                    Ok(()) => return Ok(()),
                    Err(e) => {
                        last_err = Some(e);
                        continue;
                    }
                }
            }
        }
        match tokio::fs::remove_dir_all(path).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("remove_all failed")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_allocated_size_empty_file() {
        let dir = std::env::temp_dir().join(format!("bitlab_test_alloc_{}", rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let file_path = dir.join("empty.bin");
        std::fs::File::create(&file_path).unwrap();
        let md = std::fs::metadata(&file_path).unwrap();
        let size = allocated_size(&file_path, &md);
        assert_eq!(size, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

