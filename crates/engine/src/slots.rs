//! One ffmpeg slot budget for the whole engine: playback takes a slot at once, background runs
//! queue and are preempted while playback needs the machine. Spec: `docs/spec/jobs.md` section 3.
use crate::lock::Locked;
use fliks_engine_core::api::JobPriority;
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, watch};

/// `cores - 1`, at least 1; `available_parallelism` already applies affinity and cgroup quotas.
/// [ts:common/utils/ffmpeg-slots.ts:47-54]
pub fn auto_size() -> usize {
    cpu_quota().saturating_sub(1).max(1)
}

/// Cores this process may use. `available_parallelism` already applies affinity and cgroup quotas.
pub fn cpu_quota() -> usize {
    std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .max(1)
}

struct Waiter {
    id: u64,
    priority: JobPriority,
    grant: oneshot::Sender<watch::Receiver<bool>>,
}

struct State {
    size: usize,
    /// Software playback runs. These consume the CPU budget.
    playback: usize,
    /// Hardware encodes. They do not consume the CPU budget; sprites still see them.
    gpu: usize,
    /// Background holders in grant order, each with its "preempted" flag.
    running: Vec<(u64, watch::Sender<bool>)>,
    queue: Vec<Waiter>,
    next_id: u64,
}

pub struct Slots {
    st: Mutex<State>,
}

impl Slots {
    pub fn new(size: usize) -> Arc<Self> {
        Arc::new(Self {
            st: Mutex::new(State {
                size,
                playback: 0,
                gpu: 0,
                running: vec![],
                queue: vec![],
                next_id: 0,
            }),
        })
    }

    /// `streaming.ffmpegSlots`: absent or 0 restores the derived size.
    pub fn set_size(&self, configured: Option<u32>) {
        let mut st = self.st.locked();
        st.size = configured
            .filter(|n| *n > 0)
            .map_or_else(auto_size, |n| n as usize);
        rebalance(&mut st);
    }

    pub fn size(&self) -> usize {
        self.st.locked().size
    }

    /// Playback runs in use, software and hardware (a throttled run's excluded).
    pub fn playback_in_use(&self) -> usize {
        let st = self.st.locked();
        st.playback + st.gpu
    }

    /// Background runs queued for a slot.
    pub fn waiting(&self) -> usize {
        self.st.locked().queue.len()
    }

    /// Never waits: playback runs first, past the size if need be. A hardware encode (`cpu` false)
    /// barely uses the quota, so it takes no slot from background work.
    pub fn playback(self: &Arc<Self>, cpu: bool) -> PlaybackSlot {
        self.playback_delta(true, cpu);
        PlaybackSlot {
            slots: self.clone(),
            idle: Mutex::new(false),
            cpu,
        }
    }

    /// Waits for a background slot: FIFO within a priority. Dropping the future leaves the queue.
    pub async fn background(self: &Arc<Self>, priority: JobPriority) -> BackgroundSlot {
        let (tx, rx) = oneshot::channel();
        let ticket = {
            let mut st = self.st.locked();
            let id = st.next_id;
            st.next_id += 1;
            let at = st
                .queue
                .iter()
                .position(|w| w.priority > priority)
                .unwrap_or(st.queue.len());
            st.queue.insert(
                at,
                Waiter {
                    id,
                    priority,
                    grant: tx,
                },
            );
            rebalance(&mut st);
            Ticket {
                slots: self.clone(),
                id,
            }
        };
        // The ticket is released on drop whether or not the grant arrived.
        let preempted = rx
            .await
            .expect("a queued waiter is only dropped by its ticket");
        BackgroundSlot {
            _ticket: ticket,
            preempted,
        }
    }

    fn release(&self, id: u64) {
        let mut st = self.st.locked();
        st.queue.retain(|w| w.id != id);
        st.running.retain(|(r, _)| *r != id);
        rebalance(&mut st);
    }

    fn playback_delta(&self, up: bool, cpu: bool) {
        let mut st = self.st.locked();
        let n = if cpu { &mut st.playback } else { &mut st.gpu };
        if up {
            *n += 1;
        } else {
            *n = n.saturating_sub(1);
        }
        if cpu {
            rebalance(&mut st);
        }
    }
}

/// Grants waiters up to `size - playback` and preempts the newest holders past it.
fn rebalance(st: &mut State) {
    let allowed = st.size.saturating_sub(st.playback);
    while st.running.len() < allowed && !st.queue.is_empty() {
        let w = st.queue.remove(0);
        let (tx, rx) = watch::channel(false);
        if w.grant.send(rx).is_ok() {
            st.running.push((w.id, tx));
        }
    }
    for (i, (_, tx)) in st.running.iter().enumerate() {
        tx.send_if_modified(|p| std::mem::replace(p, i >= allowed) != (i >= allowed));
    }
}

struct Ticket {
    slots: Arc<Slots>,
    id: u64,
}

impl Drop for Ticket {
    fn drop(&mut self) {
        self.slots.release(self.id);
    }
}

pub struct BackgroundSlot {
    _ticket: Ticket,
    /// True while playback needs this slot: pause the run.
    pub preempted: watch::Receiver<bool>,
}

pub struct PlaybackSlot {
    slots: Arc<Slots>,
    idle: Mutex<bool>,
    cpu: bool,
}

impl PlaybackSlot {
    /// A paused (throttled) playback run lends its slot back to background work.
    pub fn set_idle(&self, idle: bool) {
        let mut i = self.idle.locked();
        if *i != idle {
            *i = idle;
            self.slots.playback_delta(!idle, self.cpu);
        }
    }
}

impl Drop for PlaybackSlot {
    fn drop(&mut self) {
        if !*self.idle.locked() {
            self.slots.playback_delta(false, self.cpu);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn granted(
        f: &mut (impl Future<Output = BackgroundSlot> + Unpin),
    ) -> Option<BackgroundSlot> {
        tokio::time::timeout(Duration::from_millis(20), f)
            .await
            .ok()
    }

    #[tokio::test]
    async fn background_queues_fifo_within_priority() {
        let s = Slots::new(1);
        let a = s.background(JobPriority::Normal).await;
        let mut low = Box::pin(s.background(JobPriority::Low));
        let mut n1 = Box::pin(s.background(JobPriority::Normal));
        let n2 = Box::pin(s.background(JobPriority::Normal));
        let mut high = Box::pin(s.background(JobPriority::High));
        assert!(granted(&mut low).await.is_none() && granted(&mut high).await.is_none());
        drop(a);
        let h = granted(&mut high).await.expect("high first");
        assert!(granted(&mut n1).await.is_none());
        drop(h);
        let x = granted(&mut n1).await.expect("then the older normal");
        drop(n2); // a waiter that gave up is skipped
        drop(x);
        assert!(granted(&mut low).await.is_some());
    }

    #[tokio::test]
    async fn playback_preempts_and_resumes_the_newest_first() {
        let s = Slots::new(2);
        let old = s.background(JobPriority::Normal).await;
        let new = s.background(JobPriority::Normal).await;
        let mut queued = Box::pin(s.background(JobPriority::High));
        let p = s.playback(true);
        assert_eq!(
            (*old.preempted.borrow(), *new.preempted.borrow()),
            (false, true)
        );
        let p2 = s.playback(true);
        assert!(*old.preempted.borrow() && *new.preempted.borrow());
        // A throttled run lends its slot back.
        p2.set_idle(true);
        assert!(!*old.preempted.borrow() && *new.preempted.borrow());
        drop(p);
        assert!(!*new.preempted.borrow());
        assert!(granted(&mut queued).await.is_none(), "budget still full");
        drop(p2);
        drop(old);
        assert!(granted(&mut queued).await.is_some());
    }

    #[tokio::test]
    async fn playback_never_waits_and_a_shrink_preempts() {
        let s = Slots::new(1);
        let b = s.background(JobPriority::Normal).await;
        let (p1, p2) = (s.playback(true), s.playback(true));
        assert_eq!(s.playback_in_use(), 2);
        assert!(*b.preempted.borrow());
        drop((p1, p2));
        assert!(!*b.preempted.borrow());
        s.set_size(Some(3));
        let c = s.background(JobPriority::Normal).await;
        s.set_size(Some(1));
        assert!(!*b.preempted.borrow() && *c.preempted.borrow());
        s.set_size(None);
        assert_eq!(s.size(), auto_size());
    }

    #[tokio::test]
    async fn hardware_playback_leaves_the_cpu_budget() {
        let s = Slots::new(1);
        let job = s.background(JobPriority::Normal).await;
        let hw1 = s.playback(false);
        let hw2 = s.playback(false);
        assert_eq!(s.playback_in_use(), 2);
        assert!(!*job.preempted.borrow(), "two hardware transcodes, one job");
        let cpu = s.playback(true);
        assert!(*job.preempted.borrow());
        cpu.set_idle(true);
        assert!(!*job.preempted.borrow());
        drop(cpu);
        drop((hw1, hw2));
        assert_eq!(s.playback_in_use(), 0);
    }
}
