use core::sync::atomic::{spin_loop_hint, AtomicBool, Ordering};

pub struct SpinMutex {
    lock: AtomicBool,
}

unsafe impl lock_api::RawMutex for SpinMutex {
    const INIT: Self = SpinMutex { lock: AtomicBool::new(false) };

    type GuardMarker = lock_api::GuardSend;

    fn lock(&self) {
        while !self.try_lock() {
            spin_loop_hint();
        }
    }

    fn try_lock(&self) -> bool {
        self.lock.compare_and_swap(false, true, Ordering::Release)
    }

    unsafe fn unlock(&self) {
        self.lock.store(false, Ordering::Release);
    }
}
