//! Pausing and resuming a running job.
//!
//! A pause takes effect immediately for model calls (the request to Ollama is dropped, which stops the
//! generation — the CPU is free at once) and at the next step for everything else. A paused job gives
//! up its queue slot, so the next queued job runs meanwhile; on resume it goes to the front of the line
//! and carries on where it stopped (an interrupted model call is simply made again). Time spent paused
//! doesn't count against the job's time budgets.

use std::{
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use tokio::sync::watch;

use crate::{
    events::Events,
    queue::{Queue, Slot},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PauseState {
    /// Running normally (or not started).
    Active,
    /// Pause requested; it stops at the next step.
    Pausing,
    /// Stopped, slot released.
    Paused,
    /// Resume requested; waiting for a slot (at the front of the line).
    Resuming,
}

pub struct Control {
    job_id: String,
    queue: Option<Arc<Queue>>,
    want_pause: watch::Sender<bool>,
    slot: Mutex<Option<Slot>>,
    state: Mutex<PauseState>,
    /// Total time spent paused, plus the start of the current pause.
    paused: Mutex<(Duration, Option<Instant>)>,
    events: Mutex<Events>,
    /// Several steps can run at once (parallel page reads); only one of them handles the pause.
    hold_lock: tokio::sync::Mutex<()>,
}

impl Control {
    /// A control for a job scheduled through `queue`.
    pub fn new(job_id: &str, queue: Option<Arc<Queue>>) -> Arc<Self> {
        let (want_pause, _) = watch::channel(false);
        Arc::new(Self {
            job_id: job_id.to_string(),
            queue,
            want_pause,
            slot: Mutex::new(None),
            state: Mutex::new(PauseState::Active),
            paused: Mutex::new((Duration::ZERO, None)),
            events: Mutex::new(Events::none()),
            hold_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// A control nobody will pause (CLI runs, tests).
    pub fn detached() -> Arc<Self> {
        Self::new("", None)
    }

    pub fn set_events(&self, events: Events) {
        *self.events.lock().unwrap() = events;
    }

    pub fn set_slot(&self, slot: Slot) {
        *self.slot.lock().unwrap() = Some(slot);
    }

    /// Give the slot back (the job ended or was cancelled).
    pub fn release_slot(&self) {
        self.slot.lock().unwrap().take();
    }

    pub fn state(&self) -> PauseState {
        *self.state.lock().unwrap()
    }

    pub fn is_pause_requested(&self) -> bool {
        *self.want_pause.borrow()
    }

    /// Ask the job to pause. Returns false if it was already paused or pausing.
    pub fn pause(&self) -> bool {
        if self.is_pause_requested() {
            return false;
        }
        self.want_pause.send_replace(true);
        let mut st = self.state.lock().unwrap();
        if *st == PauseState::Active {
            *st = PauseState::Pausing;
        }
        true
    }

    /// Ask a paused job to carry on. Returns false if it wasn't paused.
    pub fn resume(&self) -> bool {
        if !self.is_pause_requested() {
            return false;
        }
        self.want_pause.send_replace(false);
        let mut st = self.state.lock().unwrap();
        if *st == PauseState::Pausing {
            *st = PauseState::Active; // it never got to stop
        }
        true
    }

    /// Time spent paused so far (including a pause in progress).
    pub fn paused_for(&self) -> Duration {
        let p = self.paused.lock().unwrap();
        p.0 + p.1.map(|s| s.elapsed()).unwrap_or_default()
    }

    /// If a pause was requested: stop here, free the slot, wait for resume, get a slot back.
    pub async fn checkpoint(&self) {
        if self.is_pause_requested() {
            let _one = self.hold_lock.lock().await;
            // another step may have paused and resumed meanwhile
            if self.is_pause_requested() {
                self.hold().await;
            }
        }
    }

    async fn hold(&self) {
        let events = self.events.lock().unwrap().clone();
        *self.state.lock().unwrap() = PauseState::Paused;
        self.paused.lock().unwrap().1 = Some(Instant::now());
        self.release_slot();
        events.status("Paused");
        let mut rx = self.want_pause.subscribe();
        let _ = rx.wait_for(|want| !*want).await;
        *self.state.lock().unwrap() = PauseState::Resuming;
        if let Some(q) = &self.queue {
            events.status("Resuming — waiting for a free slot");
            let slot = q.acquire(&self.job_id, true).await;
            self.set_slot(slot);
        }
        {
            let mut p = self.paused.lock().unwrap();
            if let Some(s) = p.1.take() {
                p.0 += s.elapsed();
            }
        }
        *self.state.lock().unwrap() = PauseState::Active;
        events.status("Resumed");
    }

    /// Run a piece of work that should stop the moment a pause is requested (a model call): if one
    /// is, the work is dropped, the job pauses, and on resume the work starts again from scratch.
    pub async fn pausable<T, F, Fut>(&self, mut make: F) -> T
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = T>,
    {
        loop {
            self.checkpoint().await;
            let mut rx = self.want_pause.subscribe();
            tokio::select! {
                out = make() => return out,
                _ = rx.wait_for(|want| *want) => {
                    tracing::info!("job {}: paused during a model call; it will be repeated on resume", self.job_id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[tokio::test]
    async fn pause_interrupts_work_and_resume_repeats_it() {
        let c = Control::detached();
        let starts = Arc::new(AtomicUsize::new(0));
        let c2 = c.clone();
        let s2 = starts.clone();
        let task = tokio::spawn(async move {
            c2.pausable(|| {
                let s = s2.clone();
                async move {
                    let n = s.fetch_add(1, Ordering::SeqCst);
                    // the first attempt would take forever; the repeat is quick
                    if n == 0 {
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                    }
                    "done"
                }
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert!(c.pause());
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(c.state(), PauseState::Paused);
        let paused_at = c.paused_for();
        assert!(c.resume());
        assert_eq!(task.await.unwrap(), "done");
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        assert_eq!(c.state(), PauseState::Active);
        assert!(c.paused_for() >= paused_at);
    }

    #[tokio::test]
    async fn a_paused_job_gives_its_slot_to_the_next_one() {
        let q = Queue::new(1);
        let c = Control::new("a", Some(q.clone()));
        c.set_slot(q.acquire("a", false).await);
        let q2 = q.clone();
        let next = tokio::spawn(async move { q2.acquire("b", false).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        c.pause();
        let c2 = c.clone();
        let held = tokio::spawn(async move { c2.checkpoint().await });
        let b = tokio::time::timeout(Duration::from_secs(1), next).await.expect("b got the slot").unwrap();
        c.resume();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(c.state(), PauseState::Resuming, "waiting at the front of the line");
        assert_eq!(q.position("a"), Some(1));
        drop(b);
        held.await.unwrap();
        assert_eq!(c.state(), PauseState::Active);
        assert_eq!(q.running(), 1);
    }
}
