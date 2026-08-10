// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 ugpibd contributors
//
// Instrument locking (VISA viLock semantics; HiSLIP AsyncLock / AsyncLockInfo
// is the first protocol mapped onto it).
//
// This lives in `frontend` rather than in a protocol module because the thing
// a lock protects is the *instrument*, which every front-end shares: a lock
// taken over one protocol must exclude I/O arriving over another, or it is not
// a lock. The registry is therefore one per daemon, not one per server.
//
// A lock is held by a *session*, not by a connection: in HiSLIP it is
// requested on the async channel and enforced on the sync channel, and it goes
// away when the session does. Locks are scoped per resource — a client that
// locks the DMM at GPIB 23 must not lock out a client talking to the counter
// at GPIB 3 — so the registry is a map keyed by the instrument's resource key
// (`gpib<PAD>`).
//
// Semantics follow VISA, which is what callers actually experience:
//
// - An *exclusive* lock (empty lock string) is granted only when nobody else
//   holds anything.
// - A *shared* lock (non-empty lock string) is granted when the resource is
//   free or already shared under the same name. A different name is a
//   conflict, not a second sharing group.
// - Locks nest. `viLock` may be called repeatedly on one session, and the
//   resource stays locked until the matching number of releases.
// - I/O from a session that holds no lock is refused while anybody else holds
//   one, which is what lets VISA report `VI_ERROR_RSRC_LOCKED`. An advisory
//   lock is worse than no lock: callers assume exclusive access and quietly
//   interleave.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::Notify;

/// Control code of an `AsyncLockResponse`, for both request and release.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockResponse {
    Failure = 0,
    SuccessExclusive = 1,
    SuccessShared = 2,
    Error = 3,
}

impl LockResponse {
    pub fn control_code(self) -> u8 {
        self as u8
    }

    pub fn is_success(self) -> bool {
        matches!(self, Self::SuccessExclusive | Self::SuccessShared)
    }
}

/// A lock-holder identity. Front-ends namespace their own ids into this so a
/// HiSLIP session and a VXI-11 link can never collide: the *tables* must be
/// shared across protocols (a lock is a lock, whoever took it), but the
/// identities must not be.
pub type HolderId = u64;

/// A HiSLIP session's registry identity.
pub fn hislip_id(session_id: u16) -> HolderId {
    0x0001_0000_0000 | HolderId::from(session_id)
}

/// A VXI-11 link's registry identity. Locks belong to the *link*, not the
/// connection (VXI-11 RULE B.6.72 speaks of links), so two links on one
/// connection contend like strangers.
pub fn vxi11_id(lid: i32) -> HolderId {
    0x0002_0000_0000 | HolderId::from(lid as u32)
}

/// Who holds what on one resource.
#[derive(Debug, Default)]
struct LockTable {
    /// Session holding the exclusive lock, and how deep its nesting is.
    exclusive: Option<(HolderId, u32)>,
    /// The name the shared holders agreed on. `None` when none are left.
    shared_name: Option<String>,
    /// Shared holders and their nesting depth.
    shared: HashMap<HolderId, u32>,
}

impl LockTable {
    fn is_empty(&self) -> bool {
        self.exclusive.is_none() && self.shared.is_empty()
    }

    /// Number of distinct sessions holding any lock here.
    fn holders(&self) -> u32 {
        let exclusive_only = match self.exclusive {
            Some((id, _)) if !self.shared.contains_key(&id) => 1,
            _ => 0,
        };
        self.shared.len() as u32 + exclusive_only
    }

    /// A session may do I/O when it holds a lock itself — a shared holder
    /// among other shared holders qualifies — or when the resource is free.
    fn grants_access(&self, id: HolderId) -> bool {
        let holds = matches!(self.exclusive, Some((holder, _)) if holder == id)
            || self.shared.contains_key(&id);
        holds || self.is_empty()
    }
}

/// Daemon-wide lock state, one table per resource.
#[derive(Debug, Default)]
pub struct LockRegistry {
    /// Std mutex, not tokio's: every operation is a few map lookups and never
    /// awaits, and a session's locks must be releasable from `Drop`, which is
    /// not async.
    tables: Mutex<HashMap<String, LockTable>>,
    /// Woken whenever a lock is released, so requests waiting out their
    /// timeout re-test immediately rather than polling.
    released: Notify,
}

impl LockRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Request a lock, waiting up to `timeout` for a conflicting one to go
    /// away. An empty `lock_string` requests an exclusive lock, anything else
    /// a shared lock under that name.
    pub async fn request(
        &self,
        resource: &str,
        id: HolderId,
        lock_string: &str,
        timeout: Duration,
    ) -> LockResponse {
        let deadline = Instant::now() + timeout;
        loop {
            // Subscribe before testing. A release landing between the test and
            // the wait would otherwise go unnoticed and cost the full timeout.
            let released = self.released.notified();
            if let Some(granted) = self.try_request(resource, id, lock_string) {
                return granted;
            }
            let now = Instant::now();
            if now >= deadline {
                return LockResponse::Failure;
            }
            let _ = tokio::time::timeout(deadline - now, released).await;
        }
    }

    /// One attempt at acquiring. `None` means "conflicts right now".
    fn try_request(&self, resource: &str, id: HolderId, lock_string: &str) -> Option<LockResponse> {
        let mut tables = self.tables.lock().unwrap();
        let table = tables.entry(resource.to_string()).or_default();

        let granted = if lock_string.is_empty() {
            match table.exclusive.as_mut() {
                // Already ours: nest.
                Some((holder, depth)) if *holder == id => {
                    *depth += 1;
                    Some(LockResponse::SuccessExclusive)
                }
                Some(_) => None,
                // Exclusive means exclusive: any other session's shared lock
                // blocks it too.
                None if table.shared.keys().any(|&holder| holder != id) => None,
                None => {
                    table.exclusive = Some((id, 1));
                    Some(LockResponse::SuccessExclusive)
                }
            }
        } else {
            let exclusive_elsewhere = matches!(table.exclusive, Some((holder, _)) if holder != id);
            let name_clash = matches!(&table.shared_name, Some(name) if name != lock_string);
            if exclusive_elsewhere || name_clash {
                None
            } else {
                table.shared_name = Some(lock_string.to_string());
                *table.shared.entry(id).or_insert(0) += 1;
                Some(LockResponse::SuccessShared)
            }
        };

        if granted.is_none() && table.is_empty() {
            tables.remove(resource);
        }
        granted
    }

    /// Release one level of whatever lock this session holds.
    pub fn release(&self, resource: &str, id: HolderId) -> LockResponse {
        let mut tables = self.tables.lock().unwrap();
        let Some(table) = tables.get_mut(resource) else {
            return LockResponse::Error;
        };

        let response = if matches!(table.exclusive, Some((holder, _)) if holder == id) {
            let (_, depth) = table.exclusive.as_mut().unwrap();
            *depth -= 1;
            if *depth == 0 {
                table.exclusive = None;
            }
            LockResponse::SuccessExclusive
        } else if let Some(depth) = table.shared.get_mut(&id) {
            *depth -= 1;
            if *depth == 0 {
                table.shared.remove(&id);
                if table.shared.is_empty() {
                    table.shared_name = None;
                }
            }
            LockResponse::SuccessShared
        } else {
            // Releasing a lock we never held. Not a protocol violation the
            // connection should die for, but not a success either.
            return LockResponse::Error;
        };

        if table.is_empty() {
            tables.remove(resource);
        }
        drop(tables);
        self.released.notify_waiters();
        response
    }

    /// Drop every lock this session holds, at every nesting level. Called when
    /// the session ends — otherwise a client that crashes mid-lock would lock
    /// the instrument out for good.
    pub fn release_all(&self, id: HolderId) {
        let mut tables = self.tables.lock().unwrap();
        let mut freed = false;
        tables.retain(|_, table| {
            if matches!(table.exclusive, Some((holder, _)) if holder == id) {
                table.exclusive = None;
                freed = true;
            }
            if table.shared.remove(&id).is_some() {
                freed = true;
                if table.shared.is_empty() {
                    table.shared_name = None;
                }
            }
            !table.is_empty()
        });
        drop(tables);
        if freed {
            self.released.notify_waiters();
        }
    }

    /// `AsyncLockInfo`: is an exclusive lock held, and by how many sessions is
    /// this resource locked at all.
    pub fn info(&self, resource: &str) -> (bool, u32) {
        let tables = self.tables.lock().unwrap();
        match tables.get(resource) {
            Some(table) => (table.exclusive.is_some(), table.holders()),
            None => (false, 0),
        }
    }

    /// Park until this session may do I/O — it holds a lock itself, or nobody
    /// else does.
    ///
    /// The spec's answer to locked-out traffic is to leave it unprocessed, not
    /// to refuse it (§2.6.1): Data, DataEND and Trigger stay in the input
    /// buffer, TCP applies the backpressure, and the client blocks until either
    /// the lock frees or its own timeout fires. HiSLIP has no "resource locked"
    /// reply and none is to be invented, so this waits rather than answering.
    pub async fn wait_for_access(&self, resource: &str, id: HolderId) {
        loop {
            // Subscribe before testing, so a release in between is not missed.
            let released = self.released.notified();
            if self.has_access(resource, id) {
                return;
            }
            released.await;
        }
    }

    /// May this session do I/O right now? True when it holds a lock itself, or
    /// when nobody else does.
    pub fn has_access(&self, resource: &str, id: HolderId) -> bool {
        let tables = self.tables.lock().unwrap();
        match tables.get(resource) {
            Some(table) => table.grants_access(id),
            None => true,
        }
    }

    /// Does this holder currently hold any lock on the resource? Distinct
    /// from `has_access`, which is also true on a free resource. VXI-11
    /// needs the distinction twice: RULE B.6.72 makes a re-lock by the
    /// holder an *error* (VXI-11 locks do not nest, unlike VISA/HiSLIP
    /// ones), and RULE B.6.80 answers an unlock without a lock with 12.
    pub fn holds(&self, resource: &str, id: HolderId) -> bool {
        let tables = self.tables.lock().unwrap();
        match tables.get(resource) {
            Some(table) => {
                matches!(table.exclusive, Some((holder, _)) if holder == id)
                    || table.shared.contains_key(&id)
            }
            None => false,
        }
    }

    /// Bounded [`Self::wait_for_access`]: park until this holder may do I/O
    /// or the timeout runs out, reporting which. VXI-11's waitlock flag
    /// (RULES B.6.17/B.6.18 and kin) is a wait with a client-chosen bound,
    /// where HiSLIP's locked-out traffic waits indefinitely.
    pub async fn wait_for_access_timeout(
        &self,
        resource: &str,
        id: HolderId,
        timeout: Duration,
    ) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            // Subscribe before testing, so a release in between is not missed.
            let released = self.released.notified();
            if self.has_access(resource, id) {
                return true;
            }
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let _ = tokio::time::timeout(deadline - now, released).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RES: &str = "gpib23";
    const NOW: Duration = Duration::ZERO;

    #[tokio::test]
    async fn exclusive_lock_excludes_everyone_else() {
        let locks = LockRegistry::new();
        assert_eq!(
            locks.request(RES, 1, "", NOW).await,
            LockResponse::SuccessExclusive
        );
        assert_eq!(locks.request(RES, 2, "", NOW).await, LockResponse::Failure);
        assert_eq!(
            locks.request(RES, 2, "shared", NOW).await,
            LockResponse::Failure
        );
        assert!(locks.has_access(RES, 1));
        assert!(!locks.has_access(RES, 2));
        assert_eq!(locks.info(RES), (true, 1));
    }

    #[tokio::test]
    async fn a_free_resource_is_open_to_all() {
        let locks = LockRegistry::new();
        assert!(locks.has_access(RES, 1));
        assert!(locks.has_access(RES, 2));
        assert_eq!(locks.info(RES), (false, 0));
    }

    #[tokio::test]
    async fn locks_are_scoped_to_one_resource() {
        let locks = LockRegistry::new();
        locks.request(RES, 1, "", NOW).await;
        assert!(locks.has_access("gpib3", 2));
        assert_eq!(
            locks.request("gpib3", 2, "", NOW).await,
            LockResponse::SuccessExclusive
        );
    }

    #[tokio::test]
    async fn shared_locks_agree_on_a_name() {
        let locks = LockRegistry::new();
        assert_eq!(
            locks.request(RES, 1, "k1", NOW).await,
            LockResponse::SuccessShared
        );
        assert_eq!(
            locks.request(RES, 2, "k2", NOW).await,
            LockResponse::Failure
        );
        assert_eq!(
            locks.request(RES, 2, "k1", NOW).await,
            LockResponse::SuccessShared
        );
        assert_eq!(locks.info(RES), (false, 2));
        assert!(locks.has_access(RES, 1));
        assert!(locks.has_access(RES, 2));
        // Anyone else is still locked out, and cannot take it exclusively.
        assert!(!locks.has_access(RES, 3));
        assert_eq!(locks.request(RES, 3, "", NOW).await, LockResponse::Failure);
    }

    #[tokio::test]
    async fn releasing_a_lock_we_never_held_is_an_error() {
        let locks = LockRegistry::new();
        assert_eq!(locks.release(RES, 1), LockResponse::Error);
        locks.request(RES, 1, "", NOW).await;
        assert_eq!(locks.release(RES, 2), LockResponse::Error);
        assert_eq!(locks.release(RES, 1), LockResponse::SuccessExclusive);
        assert_eq!(locks.release(RES, 1), LockResponse::Error);
    }

    #[tokio::test]
    async fn locks_nest() {
        let locks = LockRegistry::new();
        locks.request(RES, 1, "", NOW).await;
        locks.request(RES, 1, "", NOW).await;
        assert_eq!(locks.release(RES, 1), LockResponse::SuccessExclusive);
        // Still held: one release, two acquisitions.
        assert_eq!(locks.request(RES, 2, "", NOW).await, LockResponse::Failure);
        assert_eq!(locks.release(RES, 1), LockResponse::SuccessExclusive);
        assert_eq!(
            locks.request(RES, 2, "", NOW).await,
            LockResponse::SuccessExclusive
        );
    }

    #[tokio::test]
    async fn a_disconnect_frees_every_level() {
        let locks = LockRegistry::new();
        locks.request(RES, 1, "", NOW).await;
        locks.request(RES, 1, "", NOW).await;
        locks.request("gpib3", 1, "k", NOW).await;
        locks.release_all(1);
        assert_eq!(locks.info(RES), (false, 0));
        assert!(locks.has_access(RES, 2));
        assert!(locks.has_access("gpib3", 2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_conflicting_request_waits_out_its_timeout() {
        let locks = LockRegistry::new();
        locks.request(RES, 1, "", NOW).await;
        let started = Instant::now();
        let answer = locks.request(RES, 2, "", Duration::from_millis(500)).await;
        assert_eq!(answer, LockResponse::Failure);
        assert!(started.elapsed() >= Duration::from_millis(500));
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiting_request_is_granted_as_soon_as_the_lock_frees() {
        let locks = std::sync::Arc::new(LockRegistry::new());
        locks.request(RES, 1, "", NOW).await;

        let holder = locks.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            holder.release(RES, 1);
        });

        let started = Instant::now();
        let answer = locks.request(RES, 2, "", Duration::from_secs(30)).await;
        assert_eq!(answer, LockResponse::SuccessExclusive);
        // Granted on the release, not after waiting the whole 30 s out.
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
