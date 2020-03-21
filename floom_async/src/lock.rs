use std::{
    ops::Deref,
    cell::UnsafeCell,
    sync::atomic::{Ordering, AtomicUsize},
};

#[cfg_attr(target_arch = "x86", repr(align(64)))]
#[cfg_attr(target_arch = "x86_64", repr(align(128)))]
struct CachePadded<T> {
    value: T,
}

impl<T> Deref for CachePadded<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.value
    }
}

pub struct Lock<T> {
    state: CachePadded<AtomicUsize>,
    value: UnsafeCell<T>,
}

unsafe impl<T> Sync for Lock<T> {}

impl<T> Lock<T> {
    pub fn new(value: T) -> Self {
        Self {
            state: CachePadded {
                value: AtomicUsize::new(0)
            },
            value: UnsafeCell::new(value),
        }
    }

    pub fn locked<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        #![allow(unused)]
        let stack_var: usize = 42;
        let mut spin: usize = &stack_var as *const _ as usize;
        
        loop {
            if self.state.load(Ordering::Relaxed) == 0 {
                if self
                    .state
                    .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    let result = f(unsafe { &mut *self.value.get() });
                    self.state.store(0, Ordering::Release);
                    return result;
                }
            }
            if cfg!(unix) {
                std::thread::yield_now();
            } else {
                if cfg!(target_pointer_width = "64") {
                    spin ^= spin.wrapping_shl(13);
                    spin ^= spin.wrapping_shr(7);
                    spin ^= spin.wrapping_shl(17);
                } else {
                    spin ^= spin.wrapping_shl(13);
                    spin ^= spin.wrapping_shr(17);
                    spin ^= spin.wrapping_shl(5);
                }
                for _ in 0..(spin % 64) {
                    std::sync::atomic::spin_loop_hint();
                }
            }
        }
    }
}