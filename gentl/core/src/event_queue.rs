//! GenTL event queue: producer threads push, consumer threads pop with
//! timeout, plus the EventKill / unregister abort semantics of GenTL 1.6
//! §4.2 and §6.3.7.
//!
//! One `EventQueue` backs one registered event object. The data stream core
//! pushes `EVENT_NEW_BUFFER` entries (correlated by core buffer id in
//! [`Entry::id`]); the producer's `EventGetData` implementation waits on it.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// Event-specific correlation id; for `EVENT_NEW_BUFFER` the core
    /// buffer id the event belongs to.
    pub id: u64,
    /// Serialized event data, if any. `EVENT_NEW_BUFFER` data is assembled
    /// at retrieval time (it contains the module handle value), so those
    /// entries stay payload-less.
    pub payload: Vec<u8>,
}

#[derive(Default)]
struct QState {
    entries: VecDeque<Entry>,
    /// Abort requests not yet consumed by a waiter (EventKill semantics:
    /// the next `wait` call returns `Killed`, even if entries are pending).
    kills: u64,
    /// Number of events pushed since creation (`EVENT_NUM_FIRED`); not
    /// affected by kills.
    fired: u64,
    /// Set on unregister (`GCUnregisterEvent`): current and future waits
    /// return `Dead`.
    dead: bool,
}

/// Result of a [`EventQueue::wait`] call.
#[derive(Debug)]
pub enum WaitResult {
    Entry(Entry),
    /// An abort request was consumed; no entry was removed from the queue.
    Killed,
    /// The queue was unregistered while waiting.
    Dead,
    /// The timeout expired before anything arrived.
    Timeout,
}

pub struct EventQueue {
    inner: Mutex<QState>,
    cv: Condvar,
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl EventQueue {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(QState::default()),
            cv: Condvar::new(),
        }
    }

    /// Push one event. Signals without a registered event object are
    /// silently discarded (GenTL 1.6 §4.2); after unregister pushes drop.
    pub fn push(&self, entry: Entry) {
        let mut q = self.inner.lock().unwrap();
        if q.dead {
            return;
        }
        q.entries.push_back(entry);
        q.fired += 1;
        self.cv.notify_all();
    }

    /// Wait for one event, an abort, or the timeout. A consumed kill takes
    /// priority over pending entries (spec §6.3.7.5: EventKill has higher
    /// priority than delivering the next event).
    pub fn wait(&self, timeout: Duration) -> WaitResult {
        let q = self.inner.lock().unwrap();
        if q.dead {
            return WaitResult::Dead;
        }
        let (mut q, _) = self
            .cv
            .wait_timeout_while(q, timeout, |q| {
                !q.dead && q.kills == 0 && q.entries.is_empty()
            })
            .unwrap();
        if q.dead {
            return WaitResult::Dead;
        }
        if q.kills > 0 {
            q.kills -= 1;
            return WaitResult::Killed;
        }
        match q.entries.pop_front() {
            Some(e) => WaitResult::Entry(e),
            None => WaitResult::Timeout,
        }
    }

    /// `EventKill`: the next `wait` returns `Killed`, whether or not a
    /// thread is currently waiting and whether or not entries are pending.
    pub fn kill_one(&self) {
        self.inner.lock().unwrap().kills += 1;
        self.cv.notify_all();
    }

    /// `GCUnregisterEvent`: abort all pending and future waits; pending
    /// entries are silently discarded.
    pub fn mark_dead(&self) {
        let mut q = self.inner.lock().unwrap();
        q.dead = true;
        q.entries.clear();
        self.cv.notify_all();
    }

    pub fn is_dead(&self) -> bool {
        self.inner.lock().unwrap().dead
    }

    /// `EventFlush`: empty the event data queue.
    pub fn flush(&self) {
        self.inner.lock().unwrap().entries.clear();
    }

    /// Drop all entries for which `keep` returns false. Used by
    /// `DSFlushQueue` to remove the event entries of discarded output
    /// buffers.
    pub fn retain(&self, mut keep: impl FnMut(&Entry) -> bool) {
        self.inner.lock().unwrap().entries.retain(|e| keep(e));
    }

    /// Hand an entry back after a failed retrieval (e.g. the consumer
    /// buffer was too small); the entry keeps its queue position.
    pub fn restore_front(&self, entry: Entry) {
        self.inner.lock().unwrap().entries.push_front(entry);
    }

    pub fn len(&self) -> usize {
        self.inner.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `EVENT_NUM_FIRED`.
    pub fn fired(&self) -> u64 {
        self.inner.lock().unwrap().fired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    const SHORT: Duration = Duration::from_millis(50);

    #[test]
    fn push_pop_order_and_fired() {
        let q = EventQueue::new();
        assert!(q.is_empty());
        q.push(Entry { id: 1, payload: vec![1] });
        q.push(Entry { id: 2, payload: vec![] });
        assert_eq!(q.len(), 2);
        assert_eq!(q.fired(), 2);
        match q.wait(SHORT) {
            WaitResult::Entry(e) => assert_eq!(e.id, 1),
            other => panic!("expected entry, got {:?}", as_str(&other)),
        }
        match q.wait(SHORT) {
            WaitResult::Entry(e) => assert_eq!(e.id, 2),
            other => panic!("expected entry, got {:?}", as_str(&other)),
        }
        assert!(matches!(q.wait(Duration::ZERO), WaitResult::Timeout));
        assert_eq!(q.fired(), 2, "pop must not affect the fired counter");
    }

    #[test]
    fn kill_priority_over_pending_entries() {
        let q = EventQueue::new();
        q.push(Entry { id: 7, payload: vec![] });
        q.kill_one();
        // Kill wins even though an entry is pending; entry stays queued.
        assert!(matches!(q.wait(Duration::ZERO), WaitResult::Killed));
        match q.wait(Duration::ZERO) {
            WaitResult::Entry(e) => assert_eq!(e.id, 7),
            other => panic!("expected entry, got {:?}", as_str(&other)),
        }
        // Kill with no waiter pending: the *next* wait aborts.
        q.kill_one();
        assert!(matches!(q.wait(Duration::ZERO), WaitResult::Killed));
        assert!(matches!(q.wait(Duration::ZERO), WaitResult::Timeout));
    }

    #[test]
    fn blocked_waiter_woken_by_kill() {
        let q = Arc::new(EventQueue::new());
        let t = {
            let q = q.clone();
            std::thread::spawn(move || q.wait(Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(20));
        q.kill_one();
        assert!(matches!(t.join().unwrap(), WaitResult::Killed));
    }

    #[test]
    fn mark_dead_aborts_waiters_and_pushes() {
        let q = Arc::new(EventQueue::new());
        let t = {
            let q = q.clone();
            std::thread::spawn(move || q.wait(Duration::from_secs(10)))
        };
        std::thread::sleep(Duration::from_millis(20));
        q.mark_dead();
        assert!(matches!(t.join().unwrap(), WaitResult::Dead));
        // New pushes are discarded, new waits return Dead immediately.
        q.push(Entry { id: 2, payload: vec![] });
        assert_eq!(q.len(), 0);
        assert!(matches!(q.wait(Duration::ZERO), WaitResult::Dead));
    }

    #[test]
    fn flush_retain_restore() {
        let q = EventQueue::new();
        for id in 1..=4 {
            q.push(Entry { id, payload: vec![] });
        }
        q.retain(|e| e.id % 2 == 0);
        assert_eq!(q.len(), 2); // 2, 4
        match q.wait(Duration::ZERO) {
            WaitResult::Entry(e) => assert_eq!(e.id, 2),
            other => panic!("expected entry, got {:?}", as_str(&other)),
        }
        let entry = match q.wait(Duration::ZERO) {
            WaitResult::Entry(e) => e,
            other => panic!("expected entry, got {:?}", as_str(&other)),
        };
        q.restore_front(entry);
        match q.wait(Duration::ZERO) {
            WaitResult::Entry(e) => assert_eq!(e.id, 4),
            other => panic!("expected entry, got {:?}", as_str(&other)),
        }
        q.push(Entry { id: 9, payload: vec![] });
        q.flush();
        assert!(q.is_empty());
    }

    fn as_str(r: &WaitResult) -> &'static str {
        match r {
            WaitResult::Entry(_) => "entry",
            WaitResult::Killed => "killed",
            WaitResult::Dead => "dead",
            WaitResult::Timeout => "timeout",
        }
    }
}
