use std::fmt;
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::{Arc, Mutex, MutexGuard};

/// V2's maximum UDP application payload in bytes.
pub const MAX_UDP_APPLICATION_BYTES: usize = 1_200;

struct DatagramSlot {
    bytes: Box<[u8; MAX_UDP_APPLICATION_BYTES]>,
    len: usize,
}

impl DatagramSlot {
    fn new() -> Self {
        Self {
            bytes: Box::new([0; MAX_UDP_APPLICATION_BYTES]),
            len: 0,
        }
    }
}

struct PoolInner {
    capacity: usize,
    free: Mutex<Vec<DatagramSlot>>,
}

/// A fixed-capacity pool of preallocated UDP application buffers.
///
/// Cloning a pool only clones its `Arc`. `try_lease` pops existing storage, and
/// dropping a lease returns that storage to the pre-sized free list.
#[derive(Clone)]
pub struct DatagramPool {
    inner: Arc<PoolInner>,
}

impl DatagramPool {
    /// Allocates exactly `capacity` fixed-size datagram slots.
    #[must_use]
    pub fn new(capacity: NonZeroUsize) -> Self {
        let capacity = capacity.get();
        let mut free = Vec::with_capacity(capacity);
        for _ in 0..capacity {
            free.push(DatagramSlot::new());
        }

        Self {
            inner: Arc::new(PoolInner {
                capacity,
                free: Mutex::new(free),
            }),
        }
    }

    /// Returns the fixed number of slots allocated at construction.
    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Returns the number of slots currently available for leasing.
    #[must_use]
    pub fn available(&self) -> usize {
        lock_unpoisoned(&self.inner.free).len()
    }

    /// Attempts to borrow one slot without waiting for storage to be created.
    #[must_use]
    pub fn try_lease(&self) -> Option<DatagramLease> {
        let mut slot = lock_unpoisoned(&self.inner.free).pop()?;
        slot.len = 0;
        Some(DatagramLease {
            slot: Some(slot),
            pool: Arc::clone(&self.inner),
        })
    }
}

impl fmt::Debug for DatagramPool {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatagramPool")
            .field("capacity", &self.capacity())
            .field("available", &self.available())
            .finish()
    }
}

/// A preallocated datagram slot that automatically returns to its pool.
pub struct DatagramLease {
    slot: Option<DatagramSlot>,
    pool: Arc<PoolInner>,
}

impl DatagramLease {
    /// Returns the initialized packet bytes selected by `set_len`.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        let slot = self.slot();
        &slot.bytes[..slot.len]
    }

    /// Returns all writable storage in this fixed-size slot.
    ///
    /// Call `set_len` after writing packet bytes.
    pub fn buffer_mut(&mut self) -> &mut [u8] {
        self.slot_mut().bytes.as_mut_slice()
    }

    /// Sets the initialized packet length.
    pub fn set_len(&mut self, len: usize) -> Result<(), DatagramLengthError> {
        if len > MAX_UDP_APPLICATION_BYTES {
            return Err(DatagramLengthError {
                actual: len,
                maximum: MAX_UDP_APPLICATION_BYTES,
            });
        }
        self.slot_mut().len = len;
        Ok(())
    }

    /// Clears the packet length while retaining the slot storage.
    pub fn clear(&mut self) {
        self.slot_mut().len = 0;
    }

    /// Returns the current packet length.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slot().len
    }

    /// Returns whether the current packet length is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn slot(&self) -> &DatagramSlot {
        self.slot
            .as_ref()
            .expect("a live datagram lease always owns a slot")
    }

    fn slot_mut(&mut self) -> &mut DatagramSlot {
        self.slot
            .as_mut()
            .expect("a live datagram lease always owns a slot")
    }
}

impl AsRef<[u8]> for DatagramLease {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Deref for DatagramLease {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_bytes()
    }
}

impl fmt::Debug for DatagramLease {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DatagramLease")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

impl Drop for DatagramLease {
    fn drop(&mut self) {
        if let Some(mut slot) = self.slot.take() {
            slot.len = 0;
            let mut free = lock_unpoisoned(&self.pool.free);
            debug_assert!(free.len() < self.pool.capacity);
            free.push(slot);
        }
    }
}

/// A requested datagram length exceeded the fixed V2 application limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DatagramLengthError {
    /// Rejected length.
    pub actual: usize,
    /// Fixed maximum length.
    pub maximum: usize,
}

impl fmt::Display for DatagramLengthError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "datagram length {} exceeds {} bytes",
            self.actual, self.maximum
        )
    }
}

impl std::error::Error for DatagramLengthError {}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pool_exhaustion_and_drop_recycle_are_bounded() {
        let pool = DatagramPool::new(NonZeroUsize::new(2).unwrap());

        let mut first = pool.try_lease().unwrap();
        let second = pool.try_lease().unwrap();
        first.buffer_mut()[..4].copy_from_slice(b"RLB2");
        first.set_len(4).unwrap();

        assert_eq!(first.as_bytes(), b"RLB2");
        assert_eq!(pool.available(), 0);
        assert!(pool.try_lease().is_none());

        drop(first);
        assert_eq!(pool.available(), 1);

        let recycled = pool.try_lease().unwrap();
        assert!(recycled.is_empty());
        assert_eq!(pool.available(), 0);

        drop(recycled);
        drop(second);
        assert_eq!(pool.available(), pool.capacity());
    }

    #[test]
    fn lease_rejects_lengths_above_the_wire_limit() {
        let pool = DatagramPool::new(NonZeroUsize::new(1).unwrap());
        let mut lease = pool.try_lease().unwrap();

        assert_eq!(
            lease.set_len(MAX_UDP_APPLICATION_BYTES + 1),
            Err(DatagramLengthError {
                actual: MAX_UDP_APPLICATION_BYTES + 1,
                maximum: MAX_UDP_APPLICATION_BYTES,
            })
        );
        assert!(lease.is_empty());
    }
}
