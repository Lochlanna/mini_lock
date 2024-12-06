use core::hint::spin_loop;

// Wastes some CPU time for the given number of iterations,
// using a hint to indicate to the CPU that we are spinning.
#[inline]
fn cpu_relax(iterations: u32) {
    for _ in 0..iterations {
        spin_loop();
    }
}

/// A counter used to perform exponential backoff in spin loops.
#[derive(Default)]
pub struct SpinWait<const NUM_SPINS: u32, const NUM_YIELDS: u32> {
    counter: u32,
}

impl<const NUM_SPINS: u32, const NUM_YIELDS: u32> SpinWait<NUM_SPINS, NUM_YIELDS> {
    /// Creates a new `SpinWait`.
    #[inline]
    pub const fn new() -> Self {
        Self { counter: 0 }
    }

    /// Resets a `SpinWait` to its initial state.
    #[inline]
    pub fn reset(&mut self) {
        self.counter = 0;
    }

    /// Spins until the sleep threshold has been reached.
    ///
    /// This function returns whether the sleep threshold has been reached, at
    /// which point further spinning has diminishing returns and the thread
    /// should be parked instead.
    ///
    /// The spin strategy will initially use a CPU-bound loop but will fall back
    /// to yielding the CPU to the OS after a few iterations.
    #[inline]
    pub fn spin(&mut self) -> bool {
        if self.counter >= NUM_SPINS + NUM_YIELDS {
            return false;
        }
        self.counter += 1;
        if self.counter <= NUM_SPINS {
            let iterations = (1 << self.counter).min(1 << 3);
            cpu_relax(iterations);
        } else {
            std::thread::yield_now();
        }
        true
    }
}
