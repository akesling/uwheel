use crate::aggregator::Aggregator;
#[cfg(feature = "rkyv")]
use rkyv::{Archive, Deserialize, Serialize};
use smallvec::SmallVec;

use super::WheelExt;

/// Number of write ahead slots
pub const DEFAULT_WRITE_AHEAD_SLOTS: usize = INLINE_WRITE_AHEAD_SLOTS;

/// Number of slots that will be inlined
const INLINE_WRITE_AHEAD_SLOTS: usize = 64;

/// A fixed-sized Write-ahead Wheel where slots are represented as seconds
#[repr(C)]
#[cfg_attr(feature = "rkyv", derive(Archive, Deserialize, Serialize))]
#[derive(Debug, Clone)]
pub struct WriteAheadWheel<A: Aggregator> {
    capacity: usize,
    slots: SmallVec<[Option<A::MutablePartialAggregate>; INLINE_WRITE_AHEAD_SLOTS]>,
    tail: usize,
    head: usize,
}

impl<A: Aggregator> Default for WriteAheadWheel<A> {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_WRITE_AHEAD_SLOTS)
    }
}

impl<A: Aggregator> WriteAheadWheel<A> {
    pub fn with_capacity(capacity: usize) -> Self {
        assert_capacity!(capacity);
        Self {
            capacity,
            slots: (0..capacity).map(|_| None).collect::<SmallVec<_>>(),
            head: 0,
            tail: 0,
        }
    }
    #[inline]
    pub fn tick(&mut self) -> Option<A::MutablePartialAggregate> {
        // bump head
        self.head = self.wrap_add(self.head, 1);

        if !self.is_empty() {
            let tail = self.tail;
            self.tail = self.wrap_add(self.tail, 1);
            self.slot(tail).take()
        } else {
            None
        }
    }

    /// Returns `true` if the wheel is empty or `false` if it contains slots
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.tail == self.head
    }

    /// Check whether this wheel can write ahead by ´addend` slots
    pub(crate) fn can_write_ahead(&self, addend: u64) -> bool {
        addend as usize <= self.write_ahead_len()
    }

    /// How many write ahead slots are available
    #[inline]
    pub(crate) fn write_ahead_len(&self) -> usize {
        self.capacity - self.len()
    }

    /// Attempts to write `entry` into the Wheel
    #[inline]
    pub fn write_ahead(&mut self, addend: u64, data: A::Input, aggregator: &A) {
        let slot_idx = self.slot_idx_forward_from_head(addend as usize);
        Self::insert(self.slot(slot_idx), data, aggregator);
    }

    #[inline]
    fn slot(&mut self, idx: usize) -> &mut Option<A::MutablePartialAggregate> {
        &mut self.slots[idx]
    }
    #[inline]
    fn insert(slot: &mut Option<A::MutablePartialAggregate>, entry: A::Input, aggregator: &A) {
        match slot {
            Some(window) => aggregator.combine_mutable(window, entry),
            None => *slot = Some(aggregator.lift(entry)),
        }
    }
}

impl<A: Aggregator> WheelExt for WriteAheadWheel<A> {
    fn capacity(&self) -> usize {
        self.capacity
    }
    fn head(&self) -> usize {
        self.head
    }
    fn tail(&self) -> usize {
        self.tail
    }
}
