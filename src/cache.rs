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
    let effective_reserve = if reserve > 0 {
        reserve
    } else {
        crate::config::HEALTH_FREE_FLOOR
    };

    let is_protected_grace = |u: &CacheUnit| -> bool {
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

    // 1. Retention sweep: clean up items older than retention threshold,
    // as long as they are not currently being read or within grace.
    if !retain.is_zero() {
        for &i in &order {
            let u = &units[i];
            if is_protected_grace(u) {
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

    // 2. Budget and reserve enforcement - Pass 1:
    // Evict idle entries (readers == 0) outside the grace window first.
    for &i in &order {
        if taken[i] || is_protected_grace(&units[i]) {
            continue;
        }
        let over_budget = budget > 0 && used_adj > budget;
        let under_reserve = effective_reserve > 0 && free_adj >= 0 && free_adj < effective_reserve;
        if !over_budget && !under_reserve {
            break;
        }
        let reason = if !over_budget && under_reserve { "reserve" } else { "budget" };
        plan.push((i, reason.to_string()));
        taken[i] = true;
        used_adj -= units[i].entry.bytes;
        if free_adj >= 0 {
            free_adj += units[i].entry.bytes;
        }
    }

    // 3. Budget and reserve enforcement - Pass 2:
    // If STILL over budget or under reserve, evict idle entries (readers == 0)
    // even if inside the grace window, oldest LRU first.
    // Active streams (readers > 0) are NEVER evicted, but idle entries cannot be
    // allowed to starve the disk (0 B free) or violate the configured budget.
    for &i in &order {
        let over_budget = budget > 0 && used_adj > budget;
        let under_reserve = effective_reserve > 0 && free_adj >= 0 && free_adj < effective_reserve;
        if !over_budget && !under_reserve {
            break;
        }
        if taken[i] || units[i].readers > 0 {
            continue;
        }
        let reason = if !over_budget && under_reserve { "reserve" } else { "budget" };
        plan.push((i, reason.to_string()));
        taken[i] = true;
        used_adj -= units[i].entry.bytes;
        if free_adj >= 0 {
            free_adj += units[i].entry.bytes;
        }
    }

    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn make_unit(name: &str, bytes: i64, age_secs: u64, readers: i32, now: SystemTime) -> CacheUnit {
        let last_use = now - Duration::from_secs(age_secs);
        CacheUnit {
            entry: CacheEntry {
                name: name.to_string(),
                path: PathBuf::from(name),
                bytes,
                modified: last_use,
                partial: false,
            },
            infohash: name.to_string(),
            loaded: true,
            readers,
            last_use,
        }
    }

    #[test]
    fn test_plan_cache_over_budget_evicts_idle_even_within_grace() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let grace = Duration::from_secs(10 * 60); // 10 minutes grace
        let retain = Duration::from_secs(24 * 3600);

        // 3 movies:
        // Movie 1: 15 GiB, watched 6 minutes ago (within 10m grace), readers = 0
        // Movie 2: 15 GiB, watched 3 minutes ago (within 10m grace), readers = 0
        // Movie 3: 10 GiB, currently playing, readers = 1
        let u1 = make_unit("movie1", 15 * 1024 * 1024 * 1024, 6 * 60, 0, now);
        let u2 = make_unit("movie2", 15 * 1024 * 1024 * 1024, 3 * 60, 0, now);
        let u3 = make_unit("movie3", 10 * 1024 * 1024 * 1024, 10, 1, now);
        let units = vec![u1, u2, u3];

        let used = 40 * 1024 * 1024 * 1024; // 40 GiB
        let budget = 35 * 1024 * 1024 * 1024; // 35 GiB
        let reserve = 2 * 1024 * 1024 * 1024; // 2 GiB
        let free_space = 0; // 0 B free!

        let plan = plan_cache(&units, used, budget, reserve, free_space, now, grace, retain);

        // It should evict movie1 (oldest idle unit)
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0, 0); // index of movie1
        assert_eq!(plan[0].1, "budget");

        // movie3 (active readers = 1) is never evicted
        assert!(!plan.iter().any(|(idx, _)| *idx == 2));
    }

    #[test]
    fn test_plan_cache_prefers_outside_grace_first() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let grace = Duration::from_secs(10 * 60);
        let retain = Duration::from_secs(24 * 3600);

        // Orphan/old: 5 GiB, watched 20 minutes ago (outside grace)
        // Movie 1: 5 GiB, watched 2 minutes ago (inside grace)
        let u1 = make_unit("old_orphan", 5 * 1024 * 1024 * 1024, 20 * 60, 0, now);
        let u2 = make_unit("movie1", 5 * 1024 * 1024 * 1024, 2 * 60, 0, now);
        let units = vec![u1, u2];

        let used = 10 * 1024 * 1024 * 1024;
        let budget = 6 * 1024 * 1024 * 1024; // 6 GiB budget
        let reserve = 2 * 1024 * 1024 * 1024;
        let free_space = 10 * 1024 * 1024 * 1024;

        let plan = plan_cache(&units, used, budget, reserve, free_space, now, grace, retain);

        // Only old_orphan should be evicted because it's outside grace and enough to satisfy budget
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].0, 0);
    }

    #[test]
    fn test_plan_cache_active_stream_never_evicted() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let grace = Duration::from_secs(10 * 60);
        let retain = Duration::from_secs(24 * 3600);

        let u1 = make_unit("active1", 20 * 1024 * 1024 * 1024, 60, 1, now);
        let units = vec![u1];

        let used = 20 * 1024 * 1024 * 1024;
        let budget = 10 * 1024 * 1024 * 1024;
        let reserve = 5 * 1024 * 1024 * 1024;
        let free_space = 0;

        let plan = plan_cache(&units, used, budget, reserve, free_space, now, grace, retain);
        assert!(plan.is_empty());
    }
}
