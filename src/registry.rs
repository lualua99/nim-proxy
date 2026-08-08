//! In-flight request registry: the live set of `/v1` requests being paced,
//! sent, or streamed, each with a kill switch the operator dashboard's
//! request queue uses to terminate a request (its own included) with error
//! code `-91` / "Your request has been terminated by the system".
//!
//! The registry answers two questions: *what is in flight right now* (the
//! queue view: client · model · path · phase · age) and *how do I stop one*.
//! A request registers right before dispatch and the registry entry is
//! removed automatically when the request's future is dropped — every exit
//! path (success, error, client hang-up, deadline, kill) unregisters through
//! RAII, so entries can never leak or be reinstated.
//!
//! The kill signal is a `watch` channel latched by `terminate`: request code
//! that already latched a receiver sees `borrow()` flip to `true`, and
//! `changed()` resolves immediately, so kill works both before a request
//! ever reaches its first check and while it is parked in any await.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Instant;

use tokio::sync::watch;

/// The registry itself. Copy-on-write free: entries are short-lived and the
/// lock is only held for microsecond-scale mutations.
pub struct RequestRegistry {
    next_id: AtomicU64,
    inner: RwLock<HashMap<u64, Entry>>,
}

/// One entry. `phase` trails the request's position in the pipeline so the
/// queue view can tell a stalled user from a generator that is mid-flight.
struct Entry {
    client: String,
    model: String,
    path: String,
    started: Instant,
    phase: &'static str,
    kill: watch::Sender<bool>,
}

/// Public, cloneable view of an in-flight request (the queue row).
#[derive(Clone)]
pub struct QueueEntry {
    pub id: u64,
    pub client: String,
    pub model: String,
    pub path: String,
    pub started: Instant,
    pub phase: &'static str,
}

/// Handle handed to the request's executor: the kill receiver for
/// `borrow()`/`changed()` checks, plus an owning guard that unregisters the
/// entry when the request's future is dropped.
pub struct Registered {
    pub id: u64,
    pub kill: watch::Receiver<bool>,
    /// RAII unregister: alive exactly as long as the request future, never
    /// read directly (drop side effect only).
    #[allow(dead_code)]
    leave: Leave,
}

/// Unregister-on-drop. Lives inside [`Registered`], which moves with the
/// request future, so the entry's lifetime is exactly the request's lifetime.
struct Leave {
    id: u64,
    registry: Arc<RequestRegistry>,
}

impl Drop for Leave {
    fn drop(&mut self) {
        self.registry.inner.write().unwrap().remove(&self.id);
    }
}

impl RequestRegistry {
    pub fn new() -> Self {
        Self {
            next_id: AtomicU64::new(0),
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Add a request to the queue. Returns the id and a kill receiver; the
    /// entry is removed when the returned `Registered` (and its guard) drops.
    pub fn register(
        self: &Arc<Self>,
        client: String,
        model: String,
        path: String,
        phase: &'static str,
    ) -> Registered {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (kill, rx) = watch::channel(false);
        self.inner.write().unwrap().insert(
            id,
            Entry {
                client,
                model,
                path,
                started: Instant::now(),
                phase,
                kill,
            },
        );
        Registered {
            id,
            kill: rx,
            leave: Leave {
                id,
                registry: self.clone(),
            },
        }
    }

    /// Latch the request's kill signal. Returns false when the request is no
    /// longer registered (finished, dropped, or already gone).
    pub fn terminate(&self, id: u64) -> bool {
        match self.inner.read().unwrap().get(&id) {
            Some(entry) => {
                let _ = entry.kill.send(true);
                true
            }
            None => false,
        }
    }

    /// Track a request's progress through the pipeline (`queued` → `upstream`).
    pub fn set_phase(&self, id: u64, phase: &'static str) {
        if let Some(entry) = self.inner.write().unwrap().get_mut(&id) {
            entry.phase = phase;
        }
    }

    /// All current entries, oldest first.
    pub fn snapshot(&self) -> Vec<QueueEntry> {
        let mut out: Vec<QueueEntry> = self
            .inner
            .read()
            .unwrap()
            .iter()
            .map(|(&id, e)| QueueEntry {
                id,
                client: e.client.clone(),
                model: e.model.clone(),
                path: e.path.clone(),
                started: e.started,
                phase: e.phase,
            })
            .collect();
        out.sort_by_key(|e| e.started);
        out
    }
}

impl Default for RequestRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reg() -> Arc<RequestRegistry> {
        Arc::new(RequestRegistry::new())
    }

    #[test]
    fn snapshot_lists_entries_oldest_first_and_drop_unregisters() {
        let r = reg();
        let first = r.register(
            "alice".into(),
            "mock/model-a".into(),
            "/v1/chat/completions".into(),
            "queued",
        );
        let second = r.register(
            "bob".into(),
            "mock/model-b".into(),
            "/v1/embeddings".into(),
            "upstream",
        );
        assert!(second.id > first.id);

        let snap = r.snapshot();
        assert_eq!(snap.len(), 2);
        assert_eq!(snap[0].client, "alice");
        assert_eq!(snap[1].client, "bob");
        assert_eq!(snap[0].phase, "queued");

        drop(first);
        let snap = r.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].id, second.id);
        assert_eq!(snap[0].phase, "upstream");
    }

    #[test]
    fn terminate_misses_unknown_ids_and_hits_live_ones() {
        let r = reg();
        assert!(!r.terminate(99));
        let req = r.register(
            "alice".into(),
            "m".into(),
            "/v1/chat/completions".into(),
            "upstream",
        );
        let id = req.id;
        assert!(r.terminate(id));
        assert!(r.terminate(id), "entry lives until the request drops");
    }

    #[test]
    fn phase_updates_are_visible_to_snapshots() {
        let r = reg();
        let req = r.register("u".into(), "m".into(), "/v1/x".into(), "queued");
        r.set_phase(req.id, "upstream");
        assert_eq!(r.snapshot()[0].phase, "upstream");
        r.set_phase(req.id, "upstream");
        drop(req);
    }

    #[tokio::test]
    async fn kill_signal_flips_borrow_and_wakes_changed() {
        let r = reg();
        let req = r.register(
            "alice".into(),
            "m".into(),
            "/v1/chat/completions".into(),
            "upstream",
        );
        let mut kill = req.kill.clone();
        assert!(!*kill.borrow());
        assert!(r.terminate(req.id));
        assert!(*kill.borrow());
        // The first `changed()` — even one started before the kill lands —
        // resolves immediately once the value differs from what was seen.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(kill.changed().await.is_ok());
    }
}