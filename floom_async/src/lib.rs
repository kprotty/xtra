mod lock;

use std::{
    cell::UnsafeCell,
    future::Future,
    pin::Pin,
    collections::VecDeque,
    task::{Waker, Context, Poll},
    sync::{Arc, atomic::{Ordering, AtomicUsize}},
};

pub fn unbounded<T>() -> (Sender<T>, Receiver<T>) {
    let shared = Arc::new(Shared {
        recv_signal: Signal::new(),
        senders: AtomicUsize::new(1),
        inner: lock::Lock::new(Inner {
            queue: VecDeque::new(),
            is_disconnected: false,
        })
    });

    (
        Sender { shared: shared.clone() },
        Receiver { shared, local_queue: VecDeque::new() },
    )
}

struct Inner<T> {
    queue: VecDeque<T>,
    is_disconnected: bool,
}

struct Shared<T> {
    recv_signal: Signal,
    senders: AtomicUsize,
    inner: lock::Lock<Inner<T>>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum RecvError {
    Disconnected,
}

pub struct Receiver<T> {
    shared: Arc<Shared<T>>,
    local_queue: VecDeque<T>,
}

impl<T> Receiver<T> {
    pub async fn recv_async(&mut self) -> Result<T, RecvError> {
        loop {
            let local_queue = &mut self.local_queue;
            if let Some(item) = local_queue.pop_front() {
                return Ok(item);
            }
            
            let is_disconnected = self.shared.inner.locked(|inner| {
                std::mem::swap(local_queue, &mut inner.queue);
                inner.is_disconnected
            });
            
            if local_queue.len() != 0 {
                continue;
            } else if is_disconnected {
                return Err(RecvError::Disconnected);
            } else {
                self.shared.recv_signal.wait().await;
            }
        }
    }
}

pub struct Sender<T> {
    shared: Arc<Shared<T>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        self.shared.senders.fetch_add(1, Ordering::Relaxed);
        Self { shared: self.shared.clone() }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        if self.shared.senders.fetch_sub(1, Ordering::Relaxed) == 1 {
            self.shared.inner.locked(|inner| inner.is_disconnected = true);
            self.shared.recv_signal.notify();
        }
    }
}

impl<T> Sender<T> {
    pub fn send(&self, item: T) {
        self.shared.inner.locked(|inner| inner.queue.push_back(item));
        self.shared.recv_signal.notify();
    }
}

const EMPTY: usize = 0b00;
const WAITING: usize = 0b01;
const NOTIFIED: usize = 0b10;
const UPDATING: usize = 0b11;

struct Signal {
    state: AtomicUsize,
    waker: UnsafeCell<Option<Waker>>,
}

unsafe impl Sync for Signal {}

impl Signal {
    pub const fn new() -> Self {
        Self {
            state: AtomicUsize::new(EMPTY),
            waker: UnsafeCell::new(None),
        }
    }

    fn update(
        &self,
        new_waker: Option<Waker>,
        consume_old_waker: impl FnOnce(Waker),
    ) {
        let waker_ref = unsafe { &mut *self.waker.get() };
        if let Some(old_waker) = std::mem::replace(waker_ref, new_waker) {
            consume_old_waker(old_waker)
        }
    }

    pub fn notify(&self) {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            match state & 0b11 {
                EMPTY | WAITING | UPDATING => match self.state.compare_exchange_weak(
                    state,
                    NOTIFIED,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(WAITING) => return self.update(None, |waker| waker.wake()),
                    Ok(_) => return,
                    Err(e) => state = e,
                },
                NOTIFIED => return,
                _ => unreachable!(),
            }
        }
    }

    pub fn wait(&self) -> impl Future<Output = ()> + '_ + Send {
        struct WaitFuture<'a> {
            signal: &'a Signal,
        }

        impl<'a> Future for WaitFuture<'a> {
            type Output = ();

            fn poll(self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Self::Output> {
                let mut state = self.signal.state.load(Ordering::Relaxed);
                loop {
                    match state & 0b11 {
                        EMPTY | WAITING => match self.signal.state.compare_exchange_weak(
                            state,
                            UPDATING,
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Err(e) => state = e,
                            Ok(_) => {
                                self.signal.update(Some(ctx.waker().clone()), std::mem::drop);
                                match self.signal.state.compare_exchange(
                                    UPDATING,
                                    WAITING,
                                    Ordering::Release,
                                    Ordering::Relaxed,
                                ) {
                                    Ok(_) => return Poll::Pending,
                                    Err(e) => {
                                        state = e;
                                        if state == NOTIFIED {
                                            self.signal.update(None, std::mem::drop);
                                        }
                                    },
                                }
                            },
                        },
                        NOTIFIED => {
                            self.signal.state.store(EMPTY, Ordering::Relaxed);
                            return Poll::Ready(());
                        },
                        UPDATING | _ => unreachable!(),
                    }
                }
            }
        }

        impl<'a> Drop for WaitFuture<'a> {
            fn drop(&mut self) {
                if self.signal.state.load(Ordering::Relaxed) == WAITING {
                    match self.signal.state.compare_exchange(
                        WAITING,
                        EMPTY,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => self.signal.update(None, std::mem::drop),
                        Err(NOTIFIED) => self.signal.state.store(EMPTY, Ordering::Relaxed),
                        Err(_) => {},
                    }
                }
            }
        }

        WaitFuture { signal: self }
    }
}