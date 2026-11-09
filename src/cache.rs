use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone)]
pub struct CacheEntry {
    pub name: String,
    pub path: PathBuf,
    pub bytes: i64,
    pub modified: SystemTime,
    pub partial: bool,
}

#[derive(Debug, Clone)]
pub struct CacheUnit {
    pub entry: CacheEntry,
    pub infohash: String,
    pub loaded: bool,
    pub readers: i32,
    pub last_use: SystemTime,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheStats {
    pub used_bytes: i64,
    pub budget_bytes: i64,
    pub reserve_bytes: i64,
    pub free_bytes: i64,
    pub entries: usize,
    pub partial_bytes: i64,
    pub orphan_bytes: i64,
    pub orphan_entries: usize,
    pub loaded_entries: usize,
    pub evicted_bytes: i64,
    pub evicted_entries: i64,
    pub last_sweep_unix: i64,
    pub retain_minutes: i64,
    pub grace_minutes: i64,
}

impl Default for CacheStats {
    fn default() -> Self {
        Self {
            used_bytes: 0,
            budget_bytes: 0,
            reserve_bytes: 0,
            free_bytes: -1,
            entries: 0,
            partial_bytes: 0,
            orphan_bytes: 0,
            orphan_entries: 0,
            loaded_entries: 0,
            evicted_bytes: 0,
            evicted_entries: 0,
            last_sweep_unix: 0,
            retain_minutes: 0,
            grace_minutes: 0,
        }
    }
}

pub fn cache_skip(name: &str) -> bool {
    match name {
        "bitlab.db" | "bitlab.db-wal" | "bitlab.db-shm" | "bitlab.db-journal" | "ADMIN_PASSWORD" | "lost+found" => true,
        _ => name.is_empty() || name.starts_with('.'),
    }
}

pub fn scan_cache(root: &Path) -> anyhow::Result<Vec<CacheEntry>> {
    use std::collections::HashMap;
    struct Agg {
        bytes: i64,
        modified: SystemTime,
        partial: bool,
    }
    let mut m: HashMap<String, Agg> = HashMap::new();
    let root_clean = root.to_path_buf();
    for entry in walkdir::WalkDir::new(root).min_depth(1).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        let rel = match p.strip_prefix(&root_clean) {
            Ok(r) => r,
            Err(_) => continue,
        };
        if rel.as_os_str().is_empty() {
            continue;
        }
        let top = rel.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
        if cache_skip(&top) {
            continue;
        }
        let a = m.entry(top.clone()).or_insert(Agg { bytes: 0, modified: std::time::UNIX_EPOCH, partial: false });
        if entry.file_type().is_dir() {
            continue;
        }
        let md = match entry.metadata() {
            Ok(md) => md,
            Err(_) => continue,
        };
        a.bytes += crate::platform::allocated_size(p, &md);
        if let Ok(mt) = md.modified() {
            if mt > a.modified {
                a.modified = mt;
            }
        }
        if let Some(fname) = p.file_name().and_then(|s| s.to_str()) {
            if fname.ends_with(".part") {
                a.partial = true;
            }
        }
    }
    let mut out = vec![];
    for (name, a) in m {
        out.push(CacheEntry {
            name: name.clone(),
            path: root.join(&name),
            bytes: a.bytes,
            modified: a.modified,
            partial: a.partial,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn plan_cache(
    units: &[CacheUnit],
    used: i64,
    budget: i64,
    reserve: i64,
    free_space: i64,
    now: SystemTime,
    grace: std::time::Duration,
    retain: std::time::Duration,
) -> Vec<(usize, String)> {
    let protected = |u: &CacheUnit| -> bool {
        if u.readers > 0 {
            return true;
        }
        now.duration_since(u.last_use).map(|d| d < grace).unwrap_or(false)
    };
    let mut order: Vec<usize> = (0..units.len()).collect();
    order.sort_by(|&a, &b| units[a].last_use.cmp(&units[b].last_use));

    let mut plan: Vec<(usize, String)> = vec![];
    let mut taken = vec![false; units.len()];
    let mut freed: i64 = 0;

    if !retain.is_zero() {
        for &i in &order {
            let u = &units[i];
            if protected(u) {
                continue;
            }
            let age = now.duration_since(u.last_use).unwrap_or_default();
            if age <= retain {
                continue;
            }
            plan.push((i, "retention".to_string()));
            taken[i] = true;
            freed += u.entry.bytes;
        }
    }
    let mut used_adj = used - freed;
    let mut free_adj = if free_space >= 0 { free_space + freed } else { -1 };

    for &i in &order {
        if taken[i] || protected(&units[i]) {
            continue;
        }
        let over_budget = budget > 0 && used_adj > budget;
        let under_reserve = reserve > 0 && free_adj >= 0 && free_adj < reserve;
        if !over_budget && !under_reserve {
            break;
        }
        let reason = if !over_budget && under_reserve { "reserve" } else { "budget" };
        plan.push((i, reason.to_string()));
        used_adj -= units[i].entry.bytes;
        if free_adj >= 0 {
            free_adj += units[i].entry.bytes;
        }
    }
    plan
}
