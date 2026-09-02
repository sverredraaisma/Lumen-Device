//! Simulated NVS.
//!
//! A map, with two operations a map does not have: `reboot`, which keeps
//! everything, and `wipe`, which does not. Those two are the point — most
//! persistence bugs are "the node came back and had forgotten X" or "the node
//! came back and still remembered X", and neither is reachable in a test unless
//! the harness can do both on demand.

use lumen_hal::Storage;
use std::collections::BTreeMap;

/// Largest single value NVS will take. Real ESP32 NVS blobs are capped well
/// below the partition size; picking a number here means an oversized record
/// fails in a scenario rather than on someone's ceiling.
pub const MAX_VALUE_LEN: usize = 4096;

/// Default total budget, in bytes of key plus value. Small on purpose: a
/// replication design that only works with unlimited flash is a design that
/// has not been tested.
pub const DEFAULT_CAPACITY: usize = 64 * 1024;

/// Storage failures. All of them are conditions real NVS produces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StorageError {
    /// The value is longer than [`MAX_VALUE_LEN`].
    ValueTooLong,
    /// The partition is full. The write did not happen; nothing was evicted.
    Full,
    /// `read` was given a buffer shorter than the stored value. The value is
    /// left intact — unlike a dropped datagram, there is nothing to lose by
    /// letting the caller retry with a bigger buffer.
    BufferTooSmall,
}

/// In-memory key/value storage with a capacity and a reboot model.
#[derive(Clone, Debug)]
pub struct SimStorage {
    /// `BTreeMap`, not `HashMap`: [`SimStorage::keys`] and any future
    /// enumeration have to come back in the same order on every run, or a
    /// scenario that iterates storage stops being reproducible.
    entries: BTreeMap<String, Vec<u8>>,
    capacity: usize,
    used: usize,
    writes: u64,
    erases: u64,
    boots: u32,
}

impl SimStorage {
    /// Empty storage with the default capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// Empty storage with a specific capacity, for testing the full path.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: BTreeMap::new(),
            capacity,
            used: 0,
            writes: 0,
            erases: 0,
            boots: 1,
        }
    }

    /// Simulate a power cycle: contents survive, per-boot counters reset.
    ///
    /// It does nothing to the data by design, and the test that asserts so is
    /// the useful one — the whole reason storage exists is that a node that
    /// reboots mid-show has to come back knowing its identity, its mesh key
    /// and the records it had replicated.
    pub fn reboot(&mut self) {
        self.boots += 1;
        self.writes = 0;
        self.erases = 0;
    }

    /// Factory reset: forget everything. A node that comes back like this must
    /// rejoin from scratch, which is a different and much longer code path than
    /// a plain reboot.
    pub fn wipe(&mut self) {
        self.entries.clear();
        self.used = 0;
    }

    /// Keys currently stored, in sorted order.
    pub fn keys(&self) -> Vec<&str> {
        self.entries.keys().map(|k| k.as_str()).collect()
    }

    /// Number of stored entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether anything is stored.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes in use, counting keys as well as values.
    pub fn used_bytes(&self) -> usize {
        self.used
    }

    /// Capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Writes since the last reboot.
    pub fn writes(&self) -> u64 {
        self.writes
    }

    /// Erases since the last reboot.
    pub fn erases(&self) -> u64 {
        self.erases
    }

    /// How many times this storage has booted. Starts at 1.
    pub fn boots(&self) -> u32 {
        self.boots
    }

    /// A value, without needing a buffer. Convenience for assertions only —
    /// the sans-IO side never sees this, it only gets [`Storage::read`].
    pub fn get(&self, key: &str) -> Option<&[u8]> {
        self.entries.get(key).map(|v| v.as_slice())
    }

    fn entry_cost(key: &str, value_len: usize) -> usize {
        key.len() + value_len
    }
}

impl Default for SimStorage {
    fn default() -> Self {
        Self::new()
    }
}

impl Storage for SimStorage {
    type Error = StorageError;

    fn read(&self, key: &str, buf: &mut [u8]) -> Result<Option<usize>, Self::Error> {
        let Some(value) = self.entries.get(key) else {
            return Ok(None);
        };
        if value.len() > buf.len() {
            return Err(StorageError::BufferTooSmall);
        }
        buf[..value.len()].copy_from_slice(value);
        Ok(Some(value.len()))
    }

    fn write(&mut self, key: &str, value: &[u8]) -> Result<(), Self::Error> {
        if value.len() > MAX_VALUE_LEN {
            return Err(StorageError::ValueTooLong);
        }
        let new_cost = Self::entry_cost(key, value.len());
        let old_cost = self
            .entries
            .get(key)
            .map(|v| Self::entry_cost(key, v.len()))
            .unwrap_or(0);
        // Overwrite is accounted as a replacement, not an addition. Getting
        // this wrong would make a node that rewrites one record every second
        // fill its flash in a scenario and nowhere else.
        let projected = self.used - old_cost + new_cost;
        if projected > self.capacity {
            return Err(StorageError::Full);
        }
        self.used = projected;
        self.entries.insert(key.to_string(), value.to_vec());
        self.writes += 1;
        Ok(())
    }

    fn erase(&mut self, key: &str) -> Result<(), Self::Error> {
        if let Some(old) = self.entries.remove(key) {
            self.used -= Self::entry_cost(key, old.len());
        }
        // Erasing an absent key succeeds. NVS behaves this way, and a caller
        // that has to check first before every delete grows a race.
        self.erases += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let mut s = SimStorage::new();
        s.write("node/uuid", b"abcd").unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(s.read("node/uuid", &mut buf).unwrap(), Some(4));
        assert_eq!(&buf[..4], b"abcd");
        assert_eq!(s.get("node/uuid"), Some(&b"abcd"[..]));
        assert_eq!(s.len(), 1);
        assert!(!s.is_empty());
        assert_eq!(s.writes(), 1);
    }

    #[test]
    fn a_missing_key_reads_as_none() {
        let s = SimStorage::new();
        let mut buf = [0u8; 4];
        assert_eq!(s.read("nope", &mut buf).unwrap(), None);
        assert_eq!(s.get("nope"), None);
        assert!(s.is_empty());
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(
            SimStorage::default().capacity(),
            SimStorage::new().capacity()
        );
    }

    #[test]
    fn a_short_buffer_errors_and_leaves_the_value_alone() {
        let mut s = SimStorage::new();
        s.write("k", b"0123456789").unwrap();
        let mut small = [0u8; 4];
        assert_eq!(s.read("k", &mut small), Err(StorageError::BufferTooSmall));
        let mut big = [0u8; 16];
        assert_eq!(s.read("k", &mut big).unwrap(), Some(10));
    }

    #[test]
    fn an_empty_value_is_storable_and_distinct_from_absent() {
        let mut s = SimStorage::new();
        s.write("k", b"").unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(s.read("k", &mut buf).unwrap(), Some(0));
        assert_eq!(s.read("other", &mut buf).unwrap(), None);
    }

    #[test]
    fn contents_survive_a_reboot() {
        let mut s = SimStorage::new();
        s.write("mesh/key", b"secret").unwrap();
        assert_eq!(s.boots(), 1);
        s.reboot();
        assert_eq!(s.boots(), 2);
        assert_eq!(s.writes(), 0, "per-boot counters reset");
        assert_eq!(s.get("mesh/key"), Some(&b"secret"[..]));
    }

    #[test]
    fn a_wipe_forgets_everything_and_frees_space() {
        let mut s = SimStorage::new();
        s.write("a", b"1234").unwrap();
        s.write("b", b"5678").unwrap();
        assert_eq!(s.used_bytes(), 10);
        s.wipe();
        assert!(s.is_empty());
        assert_eq!(s.used_bytes(), 0);
        assert_eq!(s.keys(), Vec::<&str>::new());
    }

    #[test]
    fn erase_removes_and_is_idempotent() {
        let mut s = SimStorage::new();
        s.write("a", b"12").unwrap();
        s.erase("a").unwrap();
        assert_eq!(s.get("a"), None);
        assert_eq!(s.used_bytes(), 0);
        s.erase("a").unwrap();
        s.erase("never-existed").unwrap();
        assert_eq!(s.erases(), 3);
    }

    #[test]
    fn keys_come_back_sorted() {
        let mut s = SimStorage::new();
        for k in ["zone/b", "node/a", "src/c"] {
            s.write(k, b"x").unwrap();
        }
        assert_eq!(s.keys(), vec!["node/a", "src/c", "zone/b"]);
    }

    #[test]
    fn an_oversized_value_is_refused() {
        let mut s = SimStorage::new();
        let big = vec![0u8; MAX_VALUE_LEN + 1];
        assert_eq!(s.write("k", &big), Err(StorageError::ValueTooLong));
        assert!(s.write("k", &big[..MAX_VALUE_LEN]).is_ok());
    }

    #[test]
    fn a_full_partition_refuses_the_write_without_evicting() {
        let mut s = SimStorage::with_capacity(16);
        s.write("k", &[0u8; 10]).unwrap();
        assert_eq!(s.write("j", &[0u8; 10]), Err(StorageError::Full));
        assert_eq!(s.get("k").map(|v| v.len()), Some(10));
        assert_eq!(s.used_bytes(), 11);
    }

    #[test]
    fn overwriting_reuses_the_old_entrys_space() {
        let mut s = SimStorage::with_capacity(16);
        s.write("k", &[0u8; 15]).unwrap();
        // Same size again must fit even though 15 + 15 would not.
        s.write("k", &[1u8; 15]).unwrap();
        assert_eq!(s.get("k"), Some(&[1u8; 15][..]));
        assert_eq!(s.used_bytes(), 16);
        assert_eq!(s.write("k", &[0u8; 16]), Err(StorageError::Full));
    }

    #[test]
    fn capacity_is_reported() {
        let s = SimStorage::with_capacity(99);
        assert_eq!(s.capacity(), 99);
    }
}
