pub fn allocated_size(path: &std::path::Path, md: &std::fs::Metadata) -> i64 {
    #[cfg(unix)]
    {
        let _ = path;
        use std::os::unix::fs::MetadataExt;
        let blocks = md.blocks() as i64;
        // st_blocks is in 512-byte units
        let b = blocks * 512;
        if b > 0 {
            return b;
        }
        md.len() as i64
    }
    #[cfg(windows)]
    {
        // Approximate NTFS allocated size: round up to 4KiB clusters for
        // sparse torrent files the apparent size overstates the footprint.
        // We do not query FileStandardInfo here to avoid windows-sys dep;
        // cluster rounding is a good approximation.
        let len = md.len() as i64;
        if md.file_type().is_dir() {
            return 0;
        }
        let _ = path;
        ((len + 4095) / 4096) * 4096
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
        Ok(stat.f_bavail as i64 * stat.f_frsize as i64)
    }
    #[cfg(windows)]
    {
        // Use sysinfo crate for cross-platform disk query.
        use sysinfo::{Disks, System};
        let _ = System::new;
        let disks = Disks::new_with_refreshed_list();
        let target = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let mut best: Option<i64> = None;
        for d in disks.list() {
            let mp = d.mount_point();
            if target.starts_with(mp) {
                let avail = d.available_space() as i64;
                match best {
                    None => best = Some(avail),
                    Some(b) if mp.as_os_str().len() > 0 && avail < b => best = Some(avail),
                    _ => {}
                }
            }
        }
        // Fallback: first disk's available space
        if let Some(v) = best {
            return Ok(v);
        }
        if let Some(d) = disks.list().first() {
            return Ok(d.available_space() as i64);
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
        if root.is_dir() {
            for entry in walkdir::WalkDir::new(&root).into_iter().filter_map(|e| e.ok()) {
                if entry.file_type().is_file() {
                    let mut perms = match entry.metadata().and_then(|m| Ok(m.permissions())) {
                        Ok(p) => p,
                        Err(_) => continue,
                    };
                    #[allow(clippy::permissions_set_readonly_false)]
                    perms.set_readonly(false);
                    let _ = tokio::fs::set_permissions(entry.path(), perms).await;
                }
            }
        } else if root.is_file() {
            if let Ok(md) = tokio::fs::metadata(&root).await {
                let mut p = md.permissions();
                p.set_readonly(false);
                let _ = tokio::fs::set_permissions(&root, p).await;
            }
        }
    }
    tokio::fs::remove_dir_all(path).await.or_else(|e| {
        if path.is_file() {
            std::fs::remove_file(path).map_err(|_| e)
        } else {
            Err(e)
        }
    })
}
