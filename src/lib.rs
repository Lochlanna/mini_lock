//! `MiniLock` is a small, light weight, unfair, FILO mutex that does not use any other locks including spin
//! locks. It makes use of thread parking and thread yielding along with a FILO queue to provide
//! a self contained priority inversion safe mutex.
//!
//! `MiniLock` provides only try lock and lock functions. It does not provide any cancellable locking
//! functionality. This restriction allows it to use itself as the lock to modify the queue. Only
//! threads which hold the lock are allowed to modify/remove themselves from the queue.

#![allow(dead_code)]
#![warn(missing_docs)]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::module_name_repetitions)]
#![warn(clippy::undocumented_unsafe_blocks)]

mod spinwait;
use std::cell::Cell;
use std::{ptr, thread};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread::{current, Thread};
use lock_api::GuardSend;
use crate::spinwait::SpinWait;

const LOCKED_BIT: usize = 0b1;
const PTR_MASK: usize = !LOCKED_BIT;

trait Tagged {
    fn get_ptr(self)->*const Node;
    fn get_flag(self)->bool;
}

impl Tagged for usize {
    fn get_ptr(self) -> *const Node {
        (self & PTR_MASK) as *const Node
    }

    fn get_flag(self) -> bool {
        (self & LOCKED_BIT) == LOCKED_BIT
    }
}


#[repr(align(2))]
struct Node {
    next: *const Self,
    waker: Cell<Option<Thread>>
}

impl Node {
    const fn new(waker: Thread) -> Self {
        Self {
            next: ptr::null(),
            waker: Cell::new(Some(waker)),
        }
    }

    fn as_usize_ptr(&self)->usize {
        ptr::from_ref(self) as usize
    }
}


/// A raw locking primitive. Holds the head of the FIFO list and also the state of the lock
pub struct RawMiniLock{
    head: AtomicUsize,
    thread_id_locked: AtomicU64
}

impl Default for RawMiniLock {
    fn default() -> Self {
        Self::new()
    }
}

impl RawMiniLock {
    
    /// Create a new Mini Lock
    #[must_use] pub const fn new()-> Self {
        Self {
            head: AtomicUsize::new(0),
            thread_id_locked: AtomicU64::new(0)
        }
    }


    fn push_or_lock(&self, node: &mut Node) -> bool {
        assert_eq!(node.next, ptr::null());

        let mut head = 0;
        loop {
            if head.get_flag() {
                // it's locked, so we will just try and push the node onto the list
                node.next = head.get_ptr();
                match self.head.compare_exchange(head, node.as_usize_ptr() | LOCKED_BIT, Ordering::Release, Ordering::Relaxed) {
                    Err(new_head) => head = new_head,
                    Ok(_) => return false, // we didn't lock the lock, but we did push the node
                }
            } else {
                // it's not locked. Try and grab the lock!
                match self.head.compare_exchange(head, head | LOCKED_BIT, Ordering::Acquire, Ordering::Relaxed) {
                    Err(new_head) => head = new_head,
                    Ok(_) => return true, // we locked the lock
                }
            }
        }
    }

    /// Pops a thread handle off of the fron of the wait queue
    /// 
    /// # Safety
    /// 
    /// The lock must be held by the calling thread to modify the queue
    unsafe fn pop(&self)->Option<Thread> {
        // check that it's locked. It needs to be locked by current thread too but we can't check this
        debug_assert!(self.head.load(Ordering::Acquire).get_flag());

        let mut head = self.head.load(Ordering::Acquire);

        assert_ne!(head, 0);

        while head != LOCKED_BIT {
            // SAFETY: dereferencing here is safe as we have already check that the pointer is not null with the assert
            // since we have the lock and all threads that add to the queue must sleep until worken we know that this memory
            // will be safe to access
            let head_ref = unsafe {head.get_ptr().as_ref().expect("got a null pointer from lock head")};
            let next = head_ref.next;
            if let Err(new_head) = self.head.compare_exchange(head, next as usize | LOCKED_BIT, Ordering::Relaxed, Ordering::Acquire) {
                head = new_head;
            } else {
                // success!
                let result = head_ref.waker.take();
                debug_assert!(result.is_some());
                return result;
            }
        }

        None
    }

    fn thread_waiting(&self)->bool {
        self.head.load(Ordering::Acquire) & PTR_MASK != 0
    }
}

#[allow(clippy::undocumented_unsafe_blocks)]
unsafe impl lock_api::RawMutex for RawMiniLock {
    #[allow(clippy::declare_interior_mutable_const)]
    const INIT: Self = Self::new();
    type GuardMarker = GuardSend;

    fn lock(&self) {
        loop {
            let Err(mut head) = self.head.compare_exchange(0, LOCKED_BIT, Ordering::Acquire, Ordering::Acquire) else {
                return;
            };
            let mut spinner: SpinWait<3, 3> = SpinWait::new();
            while spinner.spin() {
                if let Err(new_head) = self.head.compare_exchange(head & PTR_MASK, head | LOCKED_BIT, Ordering::Acquire, Ordering::Relaxed) {
                    head = new_head;
                } else {
                    return;
                }
            }

            // we will push ourselves onto the queue and then sleep
            let thread_handle = current();
            // TODO can we check for attempting to lock with the same thread here?
            let mut node = Node::new(thread_handle);

            if self.push_or_lock(&mut node) {
                return;
            }
            thread::park();
        }
    }

    fn try_lock(&self) -> bool {
        let mut head = 0;
        while !head.get_flag() {
            // it's unlocked. Lets try to lock it
            if let Err(new_head) = self.head.compare_exchange(head, head | LOCKED_BIT, Ordering::Acquire, Ordering::Relaxed) {
                head = new_head;
            } else {
                return true;
            }
        }
        false
    }

    unsafe fn unlock(&self) {
        loop {
            if let Err(head) = self.head.compare_exchange(LOCKED_BIT, 0, Ordering::Release, Ordering::Relaxed) {
                assert!(head.get_flag(), "unlock of unlocked mutex");
            } else {
                return;
            }
            if self.head.compare_exchange(LOCKED_BIT, 0, Ordering::Release, Ordering::Relaxed).is_ok() {
                return;
            }
            // there are waiting nodes to be popped!
            if let Some(waker) = self.pop() {
                // unlock the lock
                self.head.fetch_and(PTR_MASK, Ordering::Release);
                // wake the thread!
                waker.unpark();
                return;
            }
        }
    }

    fn is_locked(&self) -> bool {
        self.head.load(Ordering::Relaxed).get_flag()
    }
}


/// A mutual exclusion primitive useful for protecting shared data
///
/// This mutex will block threads waiting for the lock to become available. The
/// mutex can also be statically initialized or created via a `new`
/// constructor. Each mutex has a type parameter which represents the data that
/// it is protecting. The data can only be accessed through the RAII guards
/// returned from `lock` and `try_lock`, which guarantees that the data is only
/// ever accessed when the mutex is locked.
pub type MiniLock<T> = lock_api::Mutex<RawMiniLock, T>;


/// An RAII implementation of a "scoped lock" of a mutex. When this structure is
/// dropped (falls out of scope), the lock will be unlocked.
///
/// The data protected by the mutex can be accessed through this guard via its
/// `Deref` and `DerefMut` implementations.
pub type MiniLockGuard<'a, T> = lock_api::MutexGuard<'a, RawMiniLock, T>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;
    use std::time::Duration;

    /// Test that the lock initializes correctly and can be locked/unlocked.
    #[test]
    fn test_lock_unlock() {
        let mutex = MiniLock::new(42);
        assert!(!mutex.is_locked());
        {
            let mut guard = mutex.lock();
            assert_eq!(*guard, 42);
            assert!(mutex.is_locked());
            *guard = 43;
        }
        assert!(!mutex.is_locked());
        assert_eq!(*(mutex.lock()), 43);
    }

    /// Test that `try_lock` works and does not block.
    #[test]
    fn test_try_lock() {
        let mutex = MiniLock::new(0);
        let guard = mutex.try_lock();
        assert!(guard.is_some());
        assert!(mutex.is_locked());
        // this shouldn't block and return None immediately because the lock is already locked
        assert!(mutex.try_lock().is_none());
        drop(guard);
        assert!(!mutex.is_locked());
    }

    /// Test contention between multiple threads.
    #[test]
    fn test_thread_contention() {
        let mutex = Arc::new(MiniLock::new(0));
        let num_threads = 4;

        #[cfg(miri)]
        let iterations = 100;
        #[cfg(not(miri))]
        let iterations = 10000;

        let handles: Vec<_> = (0..num_threads)
            .map(|_| {
                let mutex = Arc::clone(&mutex);
                thread::spawn(move || {
                    for _ in 0..iterations {
                        *mutex.lock() += 1;
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().unwrap();
        }

        assert_eq!(*mutex.lock(), num_threads * iterations);
    }

    /// Test that threads block and wake correctly.
    #[test]
    fn test_thread_blocking() {
        let mutex = Arc::new(MiniLock::new(()));
        let barrier = Arc::new(std::sync::Barrier::new(2));

        let mutex_clone = Arc::clone(&mutex);
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            let _guard = mutex_clone.lock();
            barrier_clone.wait();
            // wait for another thread to queue for the lock
            while !unsafe {
                mutex_clone.raw()
            }.thread_waiting() {
                thread::yield_now();
            }
            // force the other thread to block for some time
            thread::sleep(Duration::from_millis(105));
        });

        barrier.wait(); // Ensure the other thread has locked the mutex.
        assert!(mutex.is_locked());

        // Attempting to lock from this thread should block until the other thread unlocks.
        let start = std::time::Instant::now();
        let _guard = mutex.lock();
        let elapsed = start.elapsed();
        // we did block the expected time while waiting for the lock!
        assert!(elapsed >= Duration::from_millis(100));

        handle.join().unwrap();
    }

    /// Test lock reentrancy is not allowed.
    #[test]
    #[should_panic(expected = "deadlock")]
    #[ignore = "don't yet have deadlock detection"]
    fn test_lock_reentrancy() {
        let mutex = MiniLock::new(());
        let _guard = mutex.lock();
        // This should deadlock or panic because MiniLock is not reentrant.
        let _guard2 = mutex.lock();
    }

    /// Test that unlocking an already unlocked mutex panics.
    #[test]
    #[should_panic(expected = "unlock of unlocked mutex")]
    fn test_double_unlock() {
        let mutex = MiniLock::new(42);
        unsafe {
            mutex.force_unlock();
        }
    }
}
