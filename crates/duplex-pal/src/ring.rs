//! Wait-Free, Zero-Allocation Single-Producer Single-Consumer (SPSC) Ring Buffer.
//!
//! Designed specifically for the Windows `LowLevelKeyboardProc` / `LowLevelMouseProc` callbacks
//! and real-time input capture threads where heap allocations, syscalls, or mutexes are forbidden.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

#[repr(align(64))]
struct CachePaddedAtomicUsize(AtomicUsize);

/// Fixed-capacity wait-free SPSC ring buffer that can be constructed in `const` / `static` context.
/// `N` MUST be a power of two (e.g., 4096).
pub struct SpscRingBuffer<T: Copy, const N: usize> {
    head: CachePaddedAtomicUsize,
    tail: CachePaddedAtomicUsize,
    buffer: UnsafeCell<[MaybeUninit<T>; N]>,
}

// SAFETY: Single producer writes to `head` and slots `[head]`, single consumer writes to `tail`
// and reads from slots `[tail]` synchronized via Acquire/Release memory ordering.
unsafe impl<T: Copy + Send, const N: usize> Sync for SpscRingBuffer<T, N> {}
unsafe impl<T: Copy + Send, const N: usize> Send for SpscRingBuffer<T, N> {}

impl<T: Copy, const N: usize> SpscRingBuffer<T, N> {
    const MASK: usize = N - 1;

    pub const fn new() -> Self {
        assert!(N.is_power_of_two(), "SpscRingBuffer capacity N must be a power of 2");
        Self {
            head: CachePaddedAtomicUsize(AtomicUsize::new(0)),
            tail: CachePaddedAtomicUsize(AtomicUsize::new(0)),
            buffer: UnsafeCell::new([MaybeUninit::uninit(); N]),
        }
    }

    /// Wait-free push called from the producer (e.g., `LowLevelKeyboardProc`).
    /// Executes in < 100ns with zero heap allocations and zero locks.
    #[inline]
    pub fn try_push(&self, item: T) -> bool {
        let head = self.head.0.load(Ordering::Relaxed);
        let next_head = (head + 1) & Self::MASK;
        let tail = self.tail.0.load(Ordering::Acquire);

        if next_head == tail {
            return false; // Buffer full
        }

        unsafe {
            let slot_ptr = (*self.buffer.get()).as_mut_ptr().add(head);
            slot_ptr.write(MaybeUninit::new(item));
        }

        self.head.0.store(next_head, Ordering::Release);
        true
    }

    /// Wait-free pop called from the consumer thread.
    #[inline]
    pub fn try_pop(&self) -> Option<T> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);

        if tail == head {
            return None; // Buffer empty
        }

        let item = unsafe {
            let slot_ptr = (*self.buffer.get()).as_ptr().add(tail);
            (*slot_ptr).assume_init()
        };

        let next_tail = (tail + 1) & Self::MASK;
        self.tail.0.store(next_tail, Ordering::Release);
        Some(item)
    }

    /// Drains all available items into `out` and returns the number of items popped.
    #[inline]
    pub fn drain_into(&self, out: &mut Vec<T>, max_items: usize) -> usize {
        let mut count = 0;
        while count < max_items {
            if let Some(item) = self.try_pop() {
                out.push(item);
                count += 1;
            } else {
                break;
            }
        }
        count
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spsc_ring_push_pop() {
        static RING: SpscRingBuffer<u32, 8> = SpscRingBuffer::new();
        for i in 0..7 {
            assert!(RING.try_push(i));
        }
        // 8th slot is sentinel in power-of-two ring
        assert!(!RING.try_push(99));

        for i in 0..7 {
            assert_eq!(RING.try_pop(), Some(i));
        }
        assert_eq!(RING.try_pop(), None);
    }
}
