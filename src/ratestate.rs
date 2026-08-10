//! Rate-state persistence: the window-budget and calibration memory that
//! must survive a restart so the proxy never draws a burst of upstream 429s
//! right after rebooting.
//!
//! The pool and governor are resilience-critical runtime state, so this is
//! best-effort persistence with a deliberately conservative failure mode:
//! any read problem degrades to a *fresh window* (warn + continue), never a
//! hard boot error. A fresh window is exactly what the proxy had before this
//! feature — restarts were safe, they just forgot the recent past. The file
//! is a versioned JSONL written atomically (tmp + rename + dir fsync), so a
//! crash mid-write leaves the previous complete state behind.
//!
//! Both the per-lane window timestamps and the governor's model caps are
//! epoch-truncated to whole seconds. The truncation error is at most ±1s and
//! always errs toward *remembering fewer* sends, which makes the restored
//! window slightly more conservative than reality — the safe direction.
//!
//! Recovery also carries a slow-start ramp ([`Ramp`]): for the first
//! `ramp_secs` after a restart the pool admits only `ramp_factor` of its
//! budget, mirroring the upstream's own post-restart relaxation. The ramp
//! only arms when the persisted state is recent (see [`RAMP_STALE_CUTOFF`]) —
//! after a long downtime the upstream window is empty anyway and ramping
//! would just strangle throughput for no safety.

use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::{Duration, Instant};

use metrics::counter;

use crate::pool::LaneState;

/// FILENAME lives in DATA_DIR next to `config.json` and `history.jsonl`.
pub const FILE_NAME: &str = "ratestate.jsonl";

/// Schema version; on a mismatch the file is treated as garbled and dropped.
const VERSION: u32 = 1;

/// Upper bound on how long the state file can be stale; the saver persists on
/// change anyway, so this is only the fallback cadence.
pub const SAVE_INTERVAL: Duration = Duration::from_secs(30);

/// Restored state older than this does not arm the slow-start ramp: the
/// upstream's window is empty after a long downtime, so ramping would only
/// strangle traffic, not protect it.
pub const RAMP_STALE_CUTOFF: Duration = Duration::from_secs(5 * 60);

/// How far a persisted timestamp may be in the future before we mistrust it
/// (clock skew / corruption); anything beyond gets clamped to "now".
const MAX_FUTURE_SKEW_SECS: u64 = 5;

/// Upper bound on a future-dated bench deadline worth honoring. Benches come
/// from `Retry-After` (or a 10s default), so anything further out than a few
/// minutes is corruption, not a lane respecting the upstream's wishes.
const MAX_BENCH_AHEAD_SECS: u64 = 5 * 60;

/// One lane's rate state, rebuilt from the file. Ages are reconstructed as
/// virtual `Instant`s measured from boot, so the window math downstream is
/// ordinary monotonic time — the same shape the pool snapshots for saving.
pub type RestoredLane = crate::pool::LaneState;

/// Everything recovered from a state file, or None when none existed/readable.
pub struct Restored {
    /// Per-key lane state, keys that are no longer configured will be dropped
    /// by the pool when it rebuilds.
    pub lanes: Vec<RestoredLane>,
    /// Governed model concurrency caps (0 = ungoverned is never stored).
    pub governor: HashMap<String, usize>,
    /// How old the file was at load; `None` means no file existed/loaded.
    pub file_age: Option<Duration>,
    /// Count of lanes whose persisted line was unreadable (corrupted).
    pub dropped: u64,
}

/// Build the lane half of the persisted state: in-window sends only, so the
/// file never grows with dead history.
fn lane_line(now: u64, now_instant: Instant, lane: &LaneState) -> String {
    let sent: Vec<u64> = lane
        .sent
        .iter()
        .map(|t| {
            now.saturating_sub(
                now_instant
                    .checked_duration_since(*t)
                    .map_or(0, |d| d.as_secs()),
            )
        })
        .collect();
    let cooldown = match now_instant.checked_duration_since(lane.cooldown_until) {
        // The bench is already over: persist its expiry as a past epoch.
        Some(d) => now.saturating_sub(d.as_secs()),
        // The bench is still active: persist its wall-clock expiry so a reload
        // can reconstruct the remaining time instead of reopening early.
        None => now.saturating_add(
            lane.cooldown_until
                .saturating_duration_since(now_instant)
                .as_secs(),
        ),
    };
    serde_json::json!({
        "v": VERSION,
        "kind": "lane",
        "key": lane.key,
        "rpm": lane.rpm,
        "sent": sent,
        "cooldown": cooldown,
        "factor": (lane.factor * 10000.0).round() / 10000.0,
    })
    .to_string()
}

fn governor_line(limits: &HashMap<String, usize>) -> String {
    serde_json::json!({
        "v": VERSION,
        "kind": "governor",
        "limits": limits,
    })
    .to_string()
}

/// Best-effort directory fsync so the rename is durable too (same pattern as
/// the metrics-history writer).
#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

/// Persist one full state snapshot. The snapshot itself must be taken by the
/// caller outside of any pool write lock; the file write here is only ever
/// atomic replacement, so a reader sees either the old or the new state.
pub fn save(
    dir: &Path,
    lanes: &[LaneState],
    limits: &HashMap<String, usize>,
) -> std::io::Result<()> {
    let path = dir.join(FILE_NAME);
    if lanes.is_empty() && limits.is_empty() {
        // Nothing to remember; a stale file would even re-arm a ramp for no
        // reason (empty windows are the norm after a wipe).
        return Ok(());
    }
    let now = crate::unix_now();
    let now_instant = Instant::now();
    let tmp = path.with_file_name(format!("{}.tmp", FILE_NAME));
    let mut buf = String::new();
    for lane in lanes {
        buf.push_str(&lane_line(now, now_instant, lane));
        buf.push('\n');
    }
    buf.push_str(&governor_line(limits));
    buf.push('\n');
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(buf.as_bytes())?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    sync_parent_directory(&path)?;
    Ok(())
}

/// Load and validate a state file. `None` = no usable file (missing, or an
/// IO error reading it) — both mean the caller starts fresh and the ramp
/// stays off. Per-line corruption drops only that lane, never the boot.
pub fn load(dir: &Path) -> Option<Restored> {
    let path = dir.join(FILE_NAME);
    let file_age = fs::metadata(&path)
        .ok()?
        .modified()
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok());
    let file = match fs::File::open(&path) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %path.display(), "cannot read ratestate ({e}); starting with a fresh window");
            return None;
        }
    };
    let now = crate::unix_now();
    let now_instant = Instant::now();
    let mut lanes = Vec::new();
    let mut governor: HashMap<String, usize> = HashMap::new();
    let mut dropped = 0u64;
    for (idx, line) in BufReader::new(file).lines().enumerate() {
        let Ok(line) = line else {
            dropped += 1;
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            dropped += 1;
            tracing::warn!(path = %path.display(), line = idx, "unreadable rate-state line; dropping it");
            continue;
        };
        if v.get("v").and_then(|x| x.as_u64()) != Some(VERSION as u64) {
            dropped += 1;
            continue;
        }
        match v.get("kind").and_then(|k| k.as_str()) {
            Some("lane") => {
                let Some(key) = v.get("key").and_then(|k| k.as_str()) else {
                    dropped += 1;
                    continue;
                };
                let rpm = v.get("rpm").and_then(|r| r.as_u64()).unwrap_or(0) as usize;
                let mut sent = Vec::new();
                if let Some(list) = v.get("sent").and_then(|s| s.as_array()) {
                    for ts in list {
                        if let Some(e) = ts.as_u64() {
                            // Only in-window sends are worth remembering; the
                            // pool prunes its window anyway, so this is just a
                            // file-size guard.
                            if e >= now.saturating_sub(61) && e <= now + MAX_FUTURE_SKEW_SECS {
                                sent.push(epoch_to_instant(now, now_instant, e));
                            }
                        }
                    }
                }
                let cooldown_epoch = v.get("cooldown").and_then(|c| c.as_u64()).unwrap_or(now);
                let cooldown_until = if cooldown_epoch > now + MAX_BENCH_AHEAD_SECS {
                    // A bench this far in the future is corruption, not an
                    // upstream request; reopen the lane.
                    now_instant
                } else {
                    epoch_to_instant(now, now_instant, cooldown_epoch)
                };
                let cooldown_until = if cooldown_until <= now_instant {
                    // Expired bench restores as "open".
                    now_instant
                } else {
                    cooldown_until
                };
                let factor = v
                    .get("factor")
                    .and_then(|f| f.as_f64())
                    .unwrap_or(1.0)
                    .clamp(0.01, 1.0);
                lanes.push(RestoredLane {
                    key: key.to_owned(),
                    rpm,
                    sent: sent.into(),
                    cooldown_until,
                    factor,
                });
            }
            Some("governor") => {
                if let Some(map) = v.get("limits").and_then(|l| l.as_object()) {
                    for (model, cap) in map {
                        if let Some(c) = cap.as_u64().filter(|c| *c > 0) {
                            governor.insert(model.clone(), c as usize);
                        }
                    }
                }
            }
            _ => dropped += 1,
        }
    }
    let restored = lanes.len() as u64;
    if restored > 0 || !governor.is_empty() {
        tracing::info!(
            path = %path.display(),
            restored_lanes = restored,
            dropped,
            governor_models = governor.len(),
            "recovered rate-limit state; resuming the upstream pacing window"
        );
    }
    counter!("nimproxy_restore_count", "outcome" => "restored").increment(restored);
    counter!("nimproxy_restore_count", "outcome" => "dropped").increment(dropped);
    Some(Restored {
        lanes,
        governor,
        file_age,
        dropped,
    })
}

/// Reconstruct a virtual Instant from a persisted epoch: `now - age` for
/// past epochs (a future-skewed send lands at "now"), or `now + lead` for a
/// future-dated epoch (an active bench deadline). Past ages are representation-
/// bounded; the pool prunes an over-age send at first use.
fn epoch_to_instant(now: u64, now_instant: Instant, epoch: u64) -> Instant {
    if epoch > now {
        now_instant + Duration::from_secs(epoch - now)
    } else {
        let age = now - epoch;
        now_instant - Duration::from_secs(age.min(now + 1))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::pool::LaneState;

    /// A unique per-test scratch dir (std-only; removed on drop).
    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "nimproxy-ratestate-test-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::SeqCst)
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn lane(key: &str, rpm: usize) -> LaneState {
        LaneState {
            key: key.to_owned(),
            rpm,
            sent: VecDeque::new(),
            cooldown_until: Instant::now(),
            factor: 1.0,
        }
    }

    #[test]
    fn roundtrip_preserves_window_and_governor() {
        let dir = TestDir::new();
        let now = Instant::now();
        let mut l = lane("key-a", 10);
        l.sent = VecDeque::from([now - Duration::from_secs(5), now - Duration::from_secs(30)]);
        l.factor = 0.64577812;
        let lanes = vec![l];
        let limits = HashMap::from([("model-x".to_owned(), 3usize)]);

        save(&dir.0, &lanes, &limits).unwrap();

        let restored = load(&dir.0).expect("file should load");
        assert_eq!(restored.lanes.len(), 1);
        assert_eq!(restored.lanes[0].key, "key-a");
        assert_eq!(restored.lanes[0].rpm, 10);
        assert_eq!(restored.lanes[0].sent.len(), 2);
        assert_eq!(restored.governor, limits);
        assert_eq!(restored.dropped, 0);
        assert!((restored.lanes[0].factor - 0.6458).abs() < 1e-9);
        assert!(restored.file_age.is_some());
    }

    #[test]
    fn stale_sends_are_pruned_on_load() {
        let dir = TestDir::new();
        let mut l = lane("key-a", 10);
        l.sent = VecDeque::from([
            Instant::now() - Duration::from_secs(5),
            Instant::now() - Duration::from_secs(120),
        ]);
        save(&dir.0, &[l], &HashMap::new()).unwrap();
        let restored = load(&dir.0).expect("file should load");
        assert_eq!(
            restored.lanes[0].sent.len(),
            1,
            "only the in-window send survives"
        );
    }

    #[test]
    fn active_bench_survives_roundtrip() {
        let dir = TestDir::new();
        let mut l = lane("key-a", 10);
        l.cooldown_until = Instant::now() + Duration::from_secs(45);
        save(&dir.0, &[l], &HashMap::new()).unwrap();
        let restored = load(&dir.0).expect("file should load");
        assert!(
            restored.lanes[0].cooldown_until > Instant::now() + Duration::from_secs(30),
            "an active bench must not reopen on restart"
        );
    }

    #[test]
    fn far_future_bench_epoch_is_corruption_and_reopens() {
        let dir = TestDir::new();
        let now = crate::unix_now();
        let path = dir.0.join(FILE_NAME);
        // A bench scheduled 10x beyond the honesty window cannot be real.
        let line = serde_json::json!({
            "v": VERSION,
            "kind": "lane",
            "key": "key-a",
            "rpm": 10,
            "sent": [],
            "cooldown": now + MAX_BENCH_AHEAD_SECS * 10,
            "factor": 1.0,
        })
        .to_string();
        fs::write(&path, format!("{line}\n")).unwrap();
        let restored = load(&dir.0).expect("file should load");
        assert!(
            restored.lanes[0].cooldown_until <= Instant::now(),
            "a corrupt bench must reopen the lane"
        );
    }

    #[test]
    fn corrupt_lines_are_dropped_not_fatal() {
        let dir = TestDir::new();
        let good = lane_line(crate::unix_now(), Instant::now(), &lane("key-a", 10));
        let path = dir.0.join(FILE_NAME);
        fs::write(
            &path,
            format!("not json at all\n{good}\n{{\"v\": \"x\"}}\n"),
        )
        .unwrap();
        let restored = load(&dir.0).expect("file should load");
        assert_eq!(restored.lanes.len(), 1, "the valid lane still loads");
        assert_eq!(restored.dropped, 2);
    }

    #[test]
    fn version_mismatch_lines_are_dropped() {
        let dir = TestDir::new();
        let path = dir.0.join(FILE_NAME);
        let line = serde_json::json!({
            "v": VERSION + 1,
            "kind": "lane",
            "key": "key-a",
            "rpm": 10,
            "sent": [],
            "cooldown": 0,
            "factor": 1.0,
        })
        .to_string();
        fs::write(&path, format!("{line}\n")).unwrap();
        let restored = load(&dir.0).expect("file should load");
        assert!(restored.lanes.is_empty());
        assert_eq!(restored.dropped, 1);
    }

    #[test]
    fn governor_zero_caps_are_never_restored() {
        let dir = TestDir::new();
        let path = dir.0.join(FILE_NAME);
        let line = serde_json::json!({
            "v": VERSION,
            "kind": "governor",
            "limits": {"model-a": 0, "model-b": 4},
        })
        .to_string();
        fs::write(&path, format!("{line}\n")).unwrap();
        let restored = load(&dir.0).expect("file should load");
        assert_eq!(
            restored.governor,
            HashMap::from([("model-b".to_owned(), 4usize)])
        );
    }

    #[test]
    fn missing_file_means_fresh_start() {
        let dir = TestDir::new();
        assert!(load(&dir.0).is_none());
    }

    #[test]
    fn empty_file_loads_as_empty_state() {
        let dir = TestDir::new();
        fs::write(dir.0.join(FILE_NAME), "").unwrap();
        let restored = load(&dir.0).expect("empty file is readable");
        assert!(restored.lanes.is_empty());
        assert!(restored.governor.is_empty());
        assert_eq!(restored.dropped, 0);
        assert!(restored.file_age.is_some());
    }

    #[test]
    fn save_skips_writing_when_there_is_nothing_to_remember() {
        let dir = TestDir::new();
        save(&dir.0, &[], &HashMap::new()).unwrap();
        assert!(!dir.0.join(FILE_NAME).exists());
    }

    #[test]
    fn sent_future_skew_beyond_tolerance_is_clamped() {
        let dir = TestDir::new();
        let now = crate::unix_now();
        let path = dir.0.join(FILE_NAME);
        let line = serde_json::json!({
            "v": VERSION,
            "kind": "lane",
            "key": "key-a",
            "rpm": 10,
            "sent": [now + MAX_FUTURE_SKEW_SECS * 100],
            "cooldown": 0,
            "factor": 1.0,
        })
        .to_string();
        fs::write(&path, format!("{line}\n")).unwrap();
        let restored = load(&dir.0).expect("file should load");
        assert!(
            restored.lanes[0].sent.is_empty(),
            "an absurdly future send is dropped"
        );
    }
}
