//! Transaction ID and per-variable lock state for the Manager.
//!
//! Simplified implementation for issue #19. The existing transaction.rs
//! and lock.rs provide the full actor-based infrastructure for future use.

use crate::runtime::ast::Value;
use std::collections::{HashMap, HashSet};
use std::time::SystemTime;

/// A globally unique transaction identifier.
/// Older timestamp = higher priority (for future wait-die implementation).
/// Higher iteration = higher priority among retries of the same transaction.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TxnId {
    /// Wall-clock creation time, used for priority ordering.
    pub timestamp: SystemTime,
    /// Incremented on retry so retried transactions gain priority.
    pub iteration: u32,
}

impl TxnId {
    pub fn new() -> Self {
        TxnId {
            timestamp: SystemTime::now(),
            iteration: 0,
        }
    }

    /// Return a new TxnId with the same timestamp but higher iteration,
    /// for use when retrying an aborted transaction.
    pub fn retry(&self) -> Self {
        TxnId {
            timestamp: self.timestamp,
            iteration: self.iteration + 1,
        }
    }
}

impl PartialOrd for TxnId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TxnId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Older timestamp = smaller = higher priority
        // Higher iteration = higher priority among retries
        self.timestamp
            .cmp(&other.timestamp)
            .then(other.iteration.cmp(&self.iteration))
    }
}

/// Per-variable lock state used by the Manager.
/// Multiple readers are allowed simultaneously; writers are exclusive.
#[derive(Debug, Clone)]
pub enum VarLock {
    Unlocked,
    ReadLocked(HashSet<TxnId>),
    WriteLocked(TxnId),
}

impl VarLock {
    pub fn new() -> Self {
        VarLock::Unlocked
    }

    /// Try to acquire a read lock. Succeeds unless write-locked.
    pub fn try_read(&mut self, txn_id: &TxnId) -> bool {
        match self {
            VarLock::Unlocked => {
                let mut set = HashSet::new();
                set.insert(txn_id.clone());
                *self = VarLock::ReadLocked(set);
                true
            }
            VarLock::ReadLocked(set) => {
                set.insert(txn_id.clone());
                true
            }
            VarLock::WriteLocked(_) => false,
        }
    }

    /// Try to acquire an exclusive write lock. Fails if any lock is held.
    pub fn try_write(&mut self, txn_id: &TxnId) -> bool {
        match self {
            VarLock::Unlocked => {
                *self = VarLock::WriteLocked(txn_id.clone());
                true
            }
            _ => false,
        }
    }

    /// Release a read lock held by txn_id.
    pub fn release_read(&mut self, txn_id: &TxnId) {
        if let VarLock::ReadLocked(set) = self {
            set.remove(txn_id);
            if set.is_empty() {
                *self = VarLock::Unlocked;
            }
        }
    }

    /// Release the write lock if currently held by txn_id.
    pub fn release_write(&mut self, txn_id: &TxnId) {
        if matches!(self, VarLock::WriteLocked(tid) if tid == txn_id) {
            *self = VarLock::Unlocked;
        }
    }

    /// Upgrade a read lock held solely by txn_id to a write lock.
    /// Needed for read-then-write patterns (e.g. x = x + 1).
    /// Returns true if upgrade succeeded or var is already write-locked by txn_id.
    pub fn upgrade_to_write(&mut self, txn_id: &TxnId) -> bool {
        match self {
            VarLock::ReadLocked(set) if set.len() == 1 && set.contains(txn_id) => {
                *self = VarLock::WriteLocked(txn_id.clone());
                true
            }
            VarLock::WriteLocked(tid) if tid == txn_id => true,
            _ => false,
        }
    }

    /// Release any lock (read or write) held by txn_id.
    pub fn release(&mut self, txn_id: &TxnId) {
        match self {
            VarLock::ReadLocked(_) => self.release_read(txn_id),
            VarLock::WriteLocked(_) => self.release_write(txn_id),
            VarLock::Unlocked => {}
        }
    }
}

/// Composite state for a single variable, consolidating value, lock, and
/// transaction history into one structure instead of three separate maps.
#[derive(Debug, Clone)]
pub struct VarState {
    /// Current value of the variable.
    pub value: crate::runtime::ast::Value,
    /// Lock state for 2-phase locking.
    pub lock: VarLock,
    /// Most recent transaction to write this variable.
    pub latest_write_txn: Option<TxnId>,
}

impl VarState {
    pub fn new(value: crate::runtime::ast::Value) -> Self {
        VarState {
            value,
            lock: VarLock::new(),
            latest_write_txn: None,
        }
    }
}

/// Per-transaction state, owned by the code executing a transaction and passed
/// around during execution rather than stored on the Manager. A single Manager
/// may eventually run multiple transactions concurrently, so transaction state
/// must not live on the Manager.
#[derive(Debug)]
pub struct Transaction {
    /// Globally unique transaction identifier.
    pub id: TxnId,
    /// Variables this transaction currently holds a lock on.
    pub locked: HashSet<String>,
    /// Values already read in this transaction (avoids re-fetching, including
    /// redundant network round-trips for remote reads).
    pub read_cache: HashMap<String, Value>,
    /// Values written by this transaction, buffered and applied to the
    /// service only on successful commit (so a failed transaction leaves no
    /// partial writes).
    pub written: HashMap<String, Value>,
}

impl Transaction {
    pub fn new(id: TxnId) -> Self {
        Transaction {
            id,
            locked: HashSet::new(),
            read_cache: HashMap::new(),
            written: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn txn(n: u64) -> TxnId {
        TxnId {
            timestamp: SystemTime::UNIX_EPOCH + Duration::from_millis(n),
            iteration: 0,
        }
    }

    fn assert_unlocked(lock: &VarLock) {
        assert!(matches!(lock, VarLock::Unlocked));
    }

    fn assert_readers(lock: &VarLock, expected: &[TxnId]) {
        match lock {
            VarLock::ReadLocked(readers) => {
                assert_eq!(readers.len(), expected.len());
                for txn_id in expected {
                    assert!(readers.contains(txn_id));
                }
            }
            other => panic!("expected read lock, got {:?}", other),
        }
    }

    fn assert_writer(lock: &VarLock, expected: &TxnId) {
        assert!(matches!(lock, VarLock::WriteLocked(owner) if owner == expected));
    }

    #[test]
    fn test_unlocked_accepts_read_lock() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn_id));
        assert_readers(&lock, &[txn_id]);
    }

    #[test]
    fn test_unlocked_accepts_write_lock() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&txn_id));
        assert_writer(&lock, &txn_id);
    }

    #[test]
    fn test_multiple_read_locks_can_coexist() {
        let txn1 = txn(1);
        let txn2 = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn1));
        assert!(lock.try_read(&txn2));
        assert_readers(&lock, &[txn1, txn2]);
    }

    #[test]
    fn test_write_lock_blocks_read_lock_from_another_transaction() {
        let writer = txn(1);
        let reader = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&writer));
        assert!(!lock.try_read(&reader));
        assert_writer(&lock, &writer);
    }

    #[test]
    fn test_read_lock_blocks_write_lock_from_another_transaction() {
        let reader = txn(1);
        let writer = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&reader));
        assert!(!lock.try_write(&writer));
        assert_readers(&lock, &[reader]);
    }

    #[test]
    fn test_releasing_one_of_multiple_read_locks_keeps_remaining_read_lock() {
        let txn1 = txn(1);
        let txn2 = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn1));
        assert!(lock.try_read(&txn2));
        lock.release(&txn1);

        assert_readers(&lock, &[txn2]);
    }

    #[test]
    fn test_releasing_last_read_lock_unlocks() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn_id));
        lock.release(&txn_id);

        assert_unlocked(&lock);
    }

    #[test]
    fn test_releasing_write_lock_unlocks() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&txn_id));
        lock.release(&txn_id);

        assert_unlocked(&lock);
    }

    #[test]
    fn test_releasing_read_lock_with_wrong_transaction_id_does_not_unlock() {
        let owner = txn(1);
        let wrong_txn = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&owner));
        lock.release(&wrong_txn);

        assert_readers(&lock, &[owner]);
    }

    #[test]
    fn test_releasing_write_lock_with_wrong_transaction_id_does_not_unlock() {
        let owner = txn(1);
        let wrong_txn = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&owner));
        lock.release(&wrong_txn);

        assert_writer(&lock, &owner);
    }

    #[test]
    fn test_sole_reader_can_upgrade_to_write_lock() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn_id));
        assert!(lock.upgrade_to_write(&txn_id));

        assert_writer(&lock, &txn_id);
    }

    #[test]
    fn test_reader_cannot_upgrade_when_other_readers_exist() {
        let txn1 = txn(1);
        let txn2 = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_read(&txn1));
        assert!(lock.try_read(&txn2));
        assert!(!lock.upgrade_to_write(&txn1));

        assert_readers(&lock, &[txn1, txn2]);
    }

    #[test]
    fn test_write_lock_upgrade_is_idempotent_for_owner() {
        let txn_id = txn(1);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&txn_id));
        assert!(lock.upgrade_to_write(&txn_id));

        assert_writer(&lock, &txn_id);
    }

    #[test]
    fn test_wrong_transaction_cannot_upgrade_write_lock() {
        let owner = txn(1);
        let wrong_txn = txn(2);
        let mut lock = VarLock::new();

        assert!(lock.try_write(&owner));
        assert!(!lock.upgrade_to_write(&wrong_txn));

        assert_writer(&lock, &owner);
    }
}
