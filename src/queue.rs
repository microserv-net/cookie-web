//! The job queue: at most `max_running` jobs hold a slot (run on the model) at a time; the others
//! wait in first-come, first-served order. A resumed job goes to the front of the line.
//!
//! A [`Slot`] is released when dropped and handed straight to the next waiter, so a slot can't leak
//! when a job is cancelled or crashes, even mid-handover.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use tokio::sync::oneshot;

pub struct Queue {
    inner: Mutex<Inner>,
}

struct Inner {
    max_running: usize,
    running: usize,
    waiting: VecDeque<Waiter>,
}

struct Waiter {
    id: String,
    tx: oneshot::Sender<Slot>,
}

/// A place in line.
pub struct Ticket(Result<Slot, oneshot::Receiver<Slot>>);

impl Ticket {
    /// Is the slot already ours?
    pub fn is_ready(&self) -> bool {
        self.0.is_ok()
    }

    pub async fn wait(self) -> Slot {
        match self.0 {
            Ok(slot) => slot,
            // The sender lives in the queue; it is only dropped by `remove`, which is used for jobs
            // that are going away anyway.
            Err(rx) => match rx.await {
                Ok(slot) => slot,
                Err(_) => std::future::pending().await,
            },
        }
    }
}

/// The right to run. Dropping it lets the next job in.
pub struct Slot {
    queue: Arc<Queue>,
    armed: bool,
}

impl Drop for Slot {
    fn drop(&mut self) {
        if self.armed {
            self.queue.release();
        }
    }
}

impl Queue {
    pub fn new(max_running: usize) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner { max_running: max_running.max(1), running: 0, waiting: VecDeque::new() }),
        })
    }

    /// A slot right now, if one is free and nobody is waiting.
    pub fn try_acquire(self: &Arc<Self>) -> Option<Slot> {
        let mut i = self.inner.lock().unwrap();
        if i.running < i.max_running && i.waiting.is_empty() {
            i.running += 1;
            return Some(Slot { queue: self.clone(), armed: true });
        }
        None
    }

    /// Take a place in line now (so the position is known at once); wait for the slot with
    /// [`Ticket::wait`]. `front` = skip the line (a job that was already running and was paused).
    pub fn enqueue(self: &Arc<Self>, id: &str, front: bool) -> Ticket {
        let mut i = self.inner.lock().unwrap();
        if i.running < i.max_running && (i.waiting.is_empty() || front) {
            i.running += 1;
            return Ticket(Ok(Slot { queue: self.clone(), armed: true }));
        }
        let (tx, rx) = oneshot::channel();
        let w = Waiter { id: id.to_string(), tx };
        if front {
            i.waiting.push_front(w)
        } else {
            i.waiting.push_back(w)
        }
        Ticket(Err(rx))
    }

    /// Wait for a slot (see [`Queue::enqueue`]).
    pub async fn acquire(self: &Arc<Self>, id: &str, front: bool) -> Slot {
        self.enqueue(id, front).wait().await
    }

    fn release(self: &Arc<Self>) {
        loop {
            let w = {
                let mut i = self.inner.lock().unwrap();
                match i.waiting.pop_front() {
                    Some(w) => w,
                    None => {
                        i.running = i.running.saturating_sub(1);
                        return;
                    }
                }
            };
            // hand the slot over directly (the running count stays the same)
            match w.tx.send(Slot { queue: self.clone(), armed: true }) {
                Ok(()) => return,
                // that waiter is gone (cancelled): don't release twice, try the next one
                Err(mut slot) => slot.armed = false,
            }
        }
    }

    /// 1-based place in line, if waiting.
    pub fn position(&self, id: &str) -> Option<usize> {
        self.inner.lock().unwrap().waiting.iter().position(|w| w.id == id).map(|p| p + 1)
    }

    /// Take a waiting job out of the line (it is being cancelled).
    pub fn remove(&self, id: &str) {
        self.inner.lock().unwrap().waiting.retain(|w| w.id != id);
    }

    pub fn waiting(&self) -> usize {
        self.inner.lock().unwrap().waiting.len()
    }

    pub fn running(&self) -> usize {
        self.inner.lock().unwrap().running
    }

    pub fn capacity(&self) -> usize {
        self.inner.lock().unwrap().max_running
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn first_come_first_served_and_front_of_line() {
        let q = Queue::new(1);
        let a = q.acquire("a", false).await;
        let q2 = q.clone();
        let b = tokio::spawn(async move { q2.acquire("b", false).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let q3 = q.clone();
        let c = tokio::spawn(async move { q3.acquire("c", false).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!((q.position("b"), q.position("c")), (Some(1), Some(2)));
        // a resumed job jumps the line
        let q4 = q.clone();
        let r = tokio::spawn(async move { q4.acquire("r", true).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(q.position("r"), Some(1));
        drop(a);
        let r = r.await.unwrap();
        assert_eq!(q.position("b"), Some(1));
        drop(r);
        let b = b.await.unwrap();
        drop(b);
        let c = c.await.unwrap();
        assert_eq!(q.running(), 1);
        drop(c);
        assert_eq!(q.running(), 0);
    }

    #[tokio::test]
    async fn a_cancelled_waiter_does_not_leak_the_slot() {
        let q = Queue::new(1);
        let a = q.acquire("a", false).await;
        let q2 = q.clone();
        let gone = tokio::spawn(async move { q2.acquire("gone", false).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        gone.abort(); // its receiver is dropped while waiting
        let _ = gone.await;
        let q3 = q.clone();
        let next = tokio::spawn(async move { q3.acquire("next", false).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(a);
        let n = tokio::time::timeout(Duration::from_secs(1), next).await.expect("slot handed on").unwrap();
        assert_eq!(q.running(), 1);
        drop(n);
        assert_eq!(q.running(), 0);
    }
}
