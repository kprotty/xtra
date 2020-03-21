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
}
#[cfg(unix)]
impl<T> Lock<T> {
    pub fn locked<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
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
            std::thread::yield_now();
        }
    }
}

#[cfg(windows)]
impl<T> Lock<T> {
    const WAKE: usize = 1 << 8;
    const WAIT: usize = 1 << 9;

    pub fn locked<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
        if self
            .byte_state()
            .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            self.lock_slow();
        }
        let result = f(unsafe { &mut *self.value.get() });
        self.unlock();
        result
    }

    fn byte_state(&self) -> &std::sync::atomic::AtomicU8 {
        unsafe { &*(&self.state as *const _ as *const _) }
    }

    #[cold]
    fn lock_slow(&self) {
        let mut spin: usize = 0;
        let handle = Self::get_nt_handle();
        let max_spin = Self::get_max_spin();

        loop {
            let waiters = self.state.load(Ordering::Relaxed);
            if waiters & 1 == 0 {
                if self
                    .byte_state()
                    .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
                    .is_ok()
                {
                    return;
                }
            } else if spin < max_spin {
                spin = spin.wrapping_add(1);
                (0..(1 << spin)).for_each(|_| std::sync::atomic::spin_loop_hint());
            } else if self
                .state
                .compare_exchange_weak(
                    waiters,
                    (waiters + Self::WAIT) | 1,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                self.invoke_nt_event(handle, Self::nt_wait());
                self.state.fetch_sub(Self::WAKE, Ordering::Relaxed);
                spin = 0;
            }
        }
    }

    fn unlock(&self) {
        self.byte_state().store(0, Ordering::Release);
        loop {
            let waiters = self.state.load(Ordering::Relaxed);
            if (waiters < Self::WAIT) || (waiters & 1 != 0) || (waiters & Self::WAKE != 0) {
                return;
            }
            if self
                .state
                .compare_exchange_weak(
                    waiters,
                    (waiters - Self::WAIT) + Self::WAKE,
                    Ordering::Release,
                    Ordering::Relaxed,
                )
                .is_ok()
            {
                let handle = Self::get_nt_handle();
                self.invoke_nt_event(handle, Self::nt_notify());
                return;
            }
            std::sync::atomic::spin_loop_hint();
        }
    }

    fn get_max_spin() -> usize {
        #[cfg(target_arch = "x86")]
        use std::arch::x86::{__cpuid, CpuidResult};
        #[cfg(target_arch = "x86_64")]
        use std::arch::x86_64::{__cpuid, CpuidResult};

        use std::{
            slice::from_raw_parts,
            str::from_utf8_unchecked,
            hint::unreachable_unchecked,
        };

        static IS_AMD: AtomicUsize = AtomicUsize::new(0);
        let is_amd = unsafe {
            match IS_AMD.load(Ordering::Relaxed) {
                0 => {
                    let CpuidResult { ebx, ecx, edx, .. } = __cpuid(0);
                    let vendor = &[ebx, edx, ecx] as *const _ as *const u8;
                    let vendor = from_utf8_unchecked(from_raw_parts(vendor, 3 * 4));
                    let is_amd = vendor == "AuthenticAMD";
                    IS_AMD.store((is_amd as usize) + 1, Ordering::Relaxed);
                    is_amd
                },
                1 => false,
                2 => true,
                _ => unreachable_unchecked(),
            }
        };

        if is_amd { 0 } else { 6 }
    }

    fn invoke_nt_event(&self, handle: usize, invoke: &AtomicUsize) {
        unsafe {
            let invoke = invoke.load(Ordering::Relaxed);
            let invoke: extern "stdcall" fn(
                KeyHandle: usize,
                Key: usize,
                Alertable: u8,
                TimeoutPtr: usize,
            ) -> u32 = std::mem::transmute(invoke);
            let key = &self.state as *const _ as usize;
            let status = invoke(handle, key, 0, 0);
            debug_assert_eq!(status, 0);
        }
    }

    fn nt_notify() -> &'static AtomicUsize {
        static NT_NOTIFY: AtomicUsize = AtomicUsize::new(0);
        &NT_NOTIFY
    }

    fn nt_wait() -> &'static AtomicUsize {
        static NT_WAIT: AtomicUsize = AtomicUsize::new(0);
        &NT_WAIT
    }

    fn get_nt_handle() -> usize {
        use std::num::NonZeroUsize;
        #[link(name = "kernel32")]
        extern "stdcall" {
            fn CloseHandle(handle: usize) -> i32;
            fn GetModuleHandleW(moduleName: *const u16) -> usize;
            fn GetProcAddress(module: usize, procName: *const u8) -> usize;
        }

        macro_rules! try_all {
            ($($body:tt)*) => ((|| Some({ $($body)* }))())
        };

        static NT_HANDLE: AtomicUsize = AtomicUsize::new(0);
        NonZeroUsize::new(NT_HANDLE.load(Ordering::Acquire))
            .map(|handle| handle.get())
            .or_else(|| unsafe {
                try_all! {
                    let dll = NonZeroUsize::new(GetModuleHandleW(
                        (&[
                            b'n' as u16,
                            b't' as u16,
                            b'd' as u16,
                            b'l' as u16,
                            b'l' as u16,
                            b'.' as u16,
                            b'd' as u16,
                            b'l' as u16,
                            b'l' as u16,
                            0 as u16,
                        ]).as_ptr()
                    ))?.get();

                    let notify = GetProcAddress(dll, b"NtReleaseKeyedEvent\0".as_ptr());
                    let notify = NonZeroUsize::new(notify)?.get();
                    Self::nt_notify().store(notify, Ordering::Relaxed);

                    let wait = GetProcAddress(dll, b"NtWaitForKeyedEvent\0".as_ptr());
                    let wait = NonZeroUsize::new(wait)?.get();
                    Self::nt_wait().store(wait, Ordering::Relaxed);

                    let create = GetProcAddress(dll, b"NtCreateKeyedEvent\0".as_ptr());
                    let create = NonZeroUsize::new(create)?.get();
                    let create: extern "stdcall" fn(
                        KeyHandle: &mut usize,
                        DesiredAccess: u32,
                        ObjectAttributes: usize,
                        Flags: u32,
                    ) -> u32 = std::mem::transmute(create);

                    let mut handle = 0;
                    let status = create(&mut handle, 0x80000000 | 0x40000000, 0, 0);
                    if status != 0 {
                        return None;
                    }

                    NT_HANDLE
                        .compare_exchange(0, handle, Ordering::AcqRel, Ordering::Acquire)
                        .map(|_| handle)
                        .unwrap_or_else(|new_handle| {
                            let status = CloseHandle(handle);
                            debug_assert_eq!(status, 1, "NtCreateKeyedEvent leaked handle");
                            new_handle
                        })
                }
            })
            .expect("OsSignal on windows requires Nt Keyed Events (WinXP+)")
    }
}