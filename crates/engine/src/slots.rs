//! One ffmpeg slot budget for the whole engine: playback takes a slot at once, background runs
//! queue and are preempted while playback needs the machine. Spec: `docs/spec/jobs.md` section 3.
use crate::lock::Locked;
use fliks_engine_core::api::JobPriority;
use std::collections::HashMap;
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

/// Consecutive hardware failures after which a device is skipped, for [`COOL_OFF`].
const MAX_FAILURES: u32 = 3;
const COOL_OFF: std::time::Duration = std::time::Duration::from_secs(60);
/// The CPU as an optimization device.
pub const CPU_DEVICE: &str = "cpu";
const DEFAULT_CPU_WEIGHT: usize = 2;
const DEFAULT_GPU_RESERVE: usize = 1;
/// Below every `JobPriority`: optimization yields to any other background work.
const OPTIMIZE_RANK: u8 = 3;

/// The device an optimization run holds; `gpu` runs take no CPU budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    pub device: String,
    pub gpu: bool,
}

struct Entry {
    id: u64,
    /// Lower first: priority, optimization last. Ties by `id` (age).
    rank: u8,
    /// Share of the CPU budget, 0 for a GPU run. Bounded to `size` when counted.
    weight: usize,
    lane: Option<Lane>,
}

impl Entry {
    fn cost(&self, size: usize) -> usize {
        self.weight.min(size.max(1))
    }

    fn gpu_device(&self) -> Option<&str> {
        self.lane
            .as_ref()
            .filter(|l| l.gpu)
            .map(|l| l.device.as_str())
    }
}

struct Waiter {
    e: Entry,
    grant: oneshot::Sender<watch::Receiver<bool>>,
}

struct Holder {
    e: Entry,
    /// "Preempted" flag.
    tx: watch::Sender<bool>,
}

struct State {
    size: usize,
    /// Software playback runs. These consume the CPU budget.
    playback: usize,
    /// Hardware playback reads by device. They do not consume the CPU budget; sprites still see them.
    gpu_reads: HashMap<String, usize>,
    /// Background holders in grant order.
    running: Vec<Holder>,
    /// Waiting by (rank, age).
    queue: Vec<Waiter>,
    next_id: u64,
    /// Weight of a software optimization (`workerSettings.cpuWeight`).
    cpu_weight: usize,
    /// Hardware reads on a device that pause its GPU optimization; 0 never pauses.
    gpu_reserve: usize,
    /// Per-device override of `gpu_reserve`.
    device_reserve: HashMap<String, usize>,
    /// Consecutive hardware failures per device and when the last one happened.
    failures: HashMap<String, (u32, std::time::Instant)>,
    /// Optimizations at once per device, outside the background job limit.
    device_limit: HashMap<String, usize>,
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
                gpu_reads: HashMap::new(),
                running: vec![],
                queue: vec![],
                next_id: 0,
                cpu_weight: DEFAULT_CPU_WEIGHT,
                gpu_reserve: DEFAULT_GPU_RESERVE,
                device_reserve: HashMap::new(),
                failures: HashMap::new(),
                device_limit: HashMap::new(),
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

    /// The host's optimization settings, from `hello`: the CPU weight of a software run and the
    /// hardware reads on a GPU that pause its background jobs.
    pub fn set_optimize(&self, cpu_weight: Option<u32>, gpu_reserve: Option<u32>) {
        let mut st = self.st.locked();
        st.cpu_weight = cpu_weight.map_or(DEFAULT_CPU_WEIGHT, |w| (w as usize).max(1));
        st.gpu_reserve = gpu_reserve.map_or(DEFAULT_GPU_RESERVE, |n| n as usize);
        rebalance(&mut st);
    }

    /// Hardware reads on `device` that pause its GPU optimization; `None` goes back to the
    /// global reserve, 0 never pauses.
    pub fn set_device_reserve(&self, device: &str, reserve: Option<u32>) {
        let mut st = self.st.locked();
        match reserve {
            Some(n) => st.device_reserve.insert(device.into(), n as usize),
            None => st.device_reserve.remove(device),
        };
        rebalance(&mut st);
    }

    /// Optimizations a device runs at once (default 1).
    pub fn set_device_limit(&self, device: &str, max_jobs: u32) {
        let mut st = self.st.locked();
        st.device_limit
            .insert(device.into(), max_jobs.max(1) as usize);
        rebalance(&mut st);
    }

    /// Playback runs in use, software and hardware (a throttled run's excluded).
    pub fn playback_in_use(&self) -> usize {
        let st = self.st.locked();
        st.playback + st.gpu_reads.values().sum::<usize>()
    }

    /// `-threads` for a run about to start: the cores split with the software runs already going
    /// (floor of 2), so concurrent transcodes share the CPU instead of each taking all of it.
    pub fn encode_threads(&self) -> u32 {
        let quota = cpu_quota();
        (quota / (self.st.locked().playback + 1)).max(2.min(quota)) as u32
    }

    /// Threads of a software optimization: its weight's share of the cores, `weight x quota / size`.
    pub fn optimize_threads(&self) -> u32 {
        let st = self.st.locked();
        let weight = st.cpu_weight.min(st.size.max(1));
        (weight * cpu_quota() / st.size.max(1)).max(1) as u32
    }

    /// Background runs queued for a slot.
    pub fn waiting(&self) -> usize {
        self.st.locked().queue.len()
    }

    /// Never waits: playback runs first, past the size if need be.
    /// A playback run on `device` (`None` = software, on the CPU budget).
    pub fn playback_on(self: &Arc<Self>, device: Option<&str>) -> PlaybackSlot {
        let device = device.map(String::from);
        self.playback_delta(true, device.as_deref());
        PlaybackSlot {
            slots: self.clone(),
            idle: Mutex::new(false),
            device,
        }
    }

    /// `devices` least loaded first: fewest reads, then no background job, then the order given.
    pub fn rank_gpus(&self, devices: &[String]) -> Vec<String> {
        let st = self.st.locked();
        let mut ranked = devices.to_vec();
        ranked.sort_by_key(|d| {
            (
                st.gpu_reads.get(d).copied().unwrap_or(0),
                on_device(&st, d) > 0,
            )
        });
        ranked
    }

    /// A hardware run on `device` failed.
    pub fn device_failed(&self, device: &str) {
        let mut st = self.st.locked();
        let f = st
            .failures
            .entry(device.into())
            .or_insert((0, std::time::Instant::now()));
        *f = (f.0 + 1, std::time::Instant::now());
    }

    /// A hardware run on `device` ran: its failures are forgiven.
    pub fn device_ok(&self, device: &str) {
        self.st.locked().failures.remove(device);
    }

    /// False while `device` has failed [`MAX_FAILURES`] times in a row and the cool-off lasts.
    pub fn device_usable(&self, device: &str) -> bool {
        self.st
            .locked()
            .failures
            .get(device)
            .is_none_or(|(n, at)| *n < MAX_FAILURES || at.elapsed() >= COOL_OFF)
    }

    /// Waits for a background slot of `weight` CPU shares: FIFO within a priority. Dropping the
    /// future leaves the queue.
    pub async fn background(
        self: &Arc<Self>,
        priority: JobPriority,
        weight: usize,
    ) -> BackgroundSlot {
        let rank = match priority {
            JobPriority::High => 0,
            JobPriority::Normal => 1,
            JobPriority::Low => 2,
        };
        self.enter(rank, Some(weight.max(1)), None).await
    }

    /// An optimization run on `lane`, below all other background work. A CPU lane weighs the
    /// setting's `cpuWeight`, a GPU lane no CPU at all; both count against the device's own limit.
    pub async fn optimize(self: &Arc<Self>, lane: Lane) -> BackgroundSlot {
        self.enter(OPTIMIZE_RANK, None, Some(lane)).await
    }

    /// Whether an optimization on `lane` could start now: its device has a free place and the
    /// GPU reserve or the CPU budget leaves it room.
    pub fn has_room(&self, lane: &Lane) -> bool {
        let st = self.st.locked();
        let e = Entry {
            id: 0,
            rank: OPTIMIZE_RANK,
            weight: if lane.gpu { 0 } else { st.cpu_weight },
            lane: Some(lane.clone()),
        };
        let cost = e.cost(st.size);
        let used: usize = st.running.iter().map(|h| h.e.cost(st.size)).sum();
        device_ready(&st, &e) && (cost == 0 || used + cost <= st.size.saturating_sub(st.playback))
    }

    async fn enter(
        self: &Arc<Self>,
        rank: u8,
        weight: Option<usize>,
        lane: Option<Lane>,
    ) -> BackgroundSlot {
        let (tx, rx) = oneshot::channel();
        let ticket = {
            let mut st = self.st.locked();
            let id = st.next_id;
            st.next_id += 1;
            let weight = weight.unwrap_or_else(|| {
                if lane.as_ref().is_some_and(|l| l.gpu) {
                    0
                } else {
                    st.cpu_weight
                }
            });
            let at = st
                .queue
                .iter()
                .position(|w| w.e.rank > rank)
                .unwrap_or(st.queue.len());
            st.queue.insert(
                at,
                Waiter {
                    e: Entry {
                        id,
                        rank,
                        weight,
                        lane,
                    },
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
        st.queue.retain(|w| w.e.id != id);
        st.running.retain(|h| h.e.id != id);
        rebalance(&mut st);
    }

    fn playback_delta(&self, up: bool, device: Option<&str>) {
        let mut st = self.st.locked();
        match device {
            None => {
                st.playback = if up {
                    st.playback + 1
                } else {
                    st.playback.saturating_sub(1)
                };
            }
            Some(d) => {
                let n = st.gpu_reads.entry(d.into()).or_default();
                *n = if up { *n + 1 } else { n.saturating_sub(1) };
            }
        }
        rebalance(&mut st);
    }
}

fn gpu_paused(st: &State, device: &str) -> bool {
    let reserve = st
        .device_reserve
        .get(device)
        .copied()
        .unwrap_or(st.gpu_reserve);
    reserve > 0 && st.gpu_reads.get(device).copied().unwrap_or(0) >= reserve
}

fn on_device(st: &State, device: &str) -> usize {
    st.running
        .iter()
        .filter(|h| h.e.lane.as_ref().is_some_and(|l| l.device == device))
        .count()
}

/// Whether the waiter may start now on its own device's terms; CPU weight is checked apart.
fn device_ready(st: &State, e: &Entry) -> bool {
    let Some(lane) = &e.lane else { return true };
    let limit = st.device_limit.get(&lane.device).copied().unwrap_or(1);
    on_device(st, &lane.device) < limit && !(lane.gpu && gpu_paused(st, &lane.device))
}

/// Grants waiters while the CPU budget `size - playback` and the devices allow, then flags the
/// holders that must yield: by (priority, age), the weights summed against the budget, and the GPU
/// runs of a device whose playback reserve is reached.
fn rebalance(st: &mut State) {
    let size = st.size;
    let allowed = size.saturating_sub(st.playback);
    let mut cpu_blocked = false;
    let mut kept = Vec::with_capacity(st.queue.len());
    for w in std::mem::take(&mut st.queue) {
        let cost = w.e.cost(size);
        let fits = cost == 0 || {
            // A non-optimization run only competes with its peers: it pauses an optimization.
            let used: usize = st
                .running
                .iter()
                .filter(|h| w.e.lane.is_some() || h.e.lane.is_none())
                .map(|h| h.e.cost(size))
                .sum();
            used + cost <= allowed
        };
        if (cost > 0 && cpu_blocked) || !device_ready(st, &w.e) || !fits {
            cpu_blocked |= cost > 0 && !fits;
            kept.push(w);
            continue;
        }
        let (tx, rx) = watch::channel(false);
        if w.grant.send(rx).is_ok() {
            st.running.push(Holder { e: w.e, tx });
        }
    }
    st.queue = kept;
    let mut order: Vec<usize> = (0..st.running.len()).collect();
    order.sort_by_key(|i| (st.running[*i].e.rank, st.running[*i].e.id));
    let mut used = 0;
    for i in order {
        let h = &st.running[i];
        let cost = h.e.cost(size);
        let cpu_ok = cost == 0 || used + cost <= allowed;
        if cpu_ok {
            used += cost;
        }
        let pause = !cpu_ok || h.e.gpu_device().is_some_and(|d| gpu_paused(st, d));
        h.tx.send_if_modified(|p| std::mem::replace(p, pause) != pause);
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
    /// The GPU this run holds; `None` for a software run.
    device: Option<String>,
}

impl PlaybackSlot {
    pub fn device(&self) -> Option<&str> {
        self.device.as_deref()
    }

    /// A paused (throttled) playback run lends its slot back to background work.
    pub fn set_idle(&self, idle: bool) {
        let mut i = self.idle.locked();
        if *i != idle {
            *i = idle;
            self.slots.playback_delta(!idle, self.device.as_deref());
        }
    }
}

impl Drop for PlaybackSlot {
    fn drop(&mut self) {
        if !*self.idle.locked() {
            self.slots.playback_delta(false, self.device.as_deref());
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

    #[test]
    fn encode_threads_split_the_cores_between_software_runs() {
        let s = Slots::new(1);
        let quota = cpu_quota();
        assert_eq!(s.encode_threads() as usize, quota);
        let held: Vec<_> = (0..quota * 2).map(|_| s.playback_on(None)).collect();
        assert_eq!(s.encode_threads() as usize, 2.min(quota));
        let _gpu = s.playback_on(Some("gpu0"));
        drop(held);
        assert_eq!(s.encode_threads() as usize, quota);
    }

    #[tokio::test]
    async fn background_queues_fifo_within_priority() {
        let s = Slots::new(1);
        let a = s.background(JobPriority::Normal, 1).await;
        let mut low = Box::pin(s.background(JobPriority::Low, 1));
        let mut n1 = Box::pin(s.background(JobPriority::Normal, 1));
        let n2 = Box::pin(s.background(JobPriority::Normal, 1));
        let mut high = Box::pin(s.background(JobPriority::High, 1));
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
        let old = s.background(JobPriority::Normal, 1).await;
        let new = s.background(JobPriority::Normal, 1).await;
        let mut queued = Box::pin(s.background(JobPriority::High, 1));
        let p = s.playback_on(None);
        assert_eq!(
            (*old.preempted.borrow(), *new.preempted.borrow()),
            (false, true)
        );
        let p2 = s.playback_on(None);
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
        let b = s.background(JobPriority::Normal, 1).await;
        let (p1, p2) = (s.playback_on(None), s.playback_on(None));
        assert_eq!(s.playback_in_use(), 2);
        assert!(*b.preempted.borrow());
        drop((p1, p2));
        assert!(!*b.preempted.borrow());
        s.set_size(Some(3));
        let c = s.background(JobPriority::Normal, 1).await;
        s.set_size(Some(1));
        assert!(!*b.preempted.borrow() && *c.preempted.borrow());
        s.set_size(None);
        assert_eq!(s.size(), auto_size());
    }

    #[tokio::test]
    async fn hardware_playback_leaves_the_cpu_budget() {
        let s = Slots::new(1);
        let job = s.background(JobPriority::Normal, 1).await;
        let hw1 = s.playback_on(Some("gpu0"));
        let hw2 = s.playback_on(Some("gpu0"));
        assert_eq!(s.playback_in_use(), 2);
        assert!(!*job.preempted.borrow(), "two hardware transcodes, one job");
        let cpu = s.playback_on(None);
        assert!(*job.preempted.borrow());
        cpu.set_idle(true);
        assert!(!*job.preempted.borrow());
        drop(cpu);
        drop((hw1, hw2));
        assert_eq!(s.playback_in_use(), 0);
    }

    fn cpu_lane() -> Lane {
        Lane {
            device: CPU_DEVICE.into(),
            gpu: false,
        }
    }

    fn gpu_lane() -> Lane {
        Lane {
            device: "gpu0".into(),
            gpu: true,
        }
    }

    #[tokio::test]
    async fn a_weighted_run_counts_its_weight_and_is_bounded_to_the_size() {
        let s = Slots::new(2);
        s.set_optimize(Some(5), None);
        // Weight 5 is bounded to the size: it still runs alone on 2 slots.
        let opt = s.optimize(cpu_lane()).await;
        let mut sprite = Box::pin(s.background(JobPriority::Normal, 1));
        assert!(!*opt.preempted.borrow());
        // The sprite starts at once and the optimization yields to it.
        let sprite = granted(&mut sprite).await.expect("sprite starts");
        assert!(*opt.preempted.borrow() && !*sprite.preempted.borrow());
        drop(sprite);
        assert!(!*opt.preempted.borrow());
        s.set_size(Some(1));
        assert!(!*opt.preempted.borrow(), "a smaller size does not block it");
        let p = s.playback_on(None);
        assert!(*opt.preempted.borrow());
        drop(p);
    }

    #[tokio::test]
    async fn the_lowest_priority_pauses_first_whatever_its_age() {
        let s = Slots::new(2);
        let low = s.background(JobPriority::Low, 1).await;
        let high = s.background(JobPriority::High, 1).await;
        let p = s.playback_on(None);
        assert_eq!(
            (*low.preempted.borrow(), *high.preempted.borrow()),
            (true, false),
            "the older low-priority run yields to the newer high one"
        );
        drop(p);
    }

    #[tokio::test]
    async fn optimization_has_its_own_device_limit_and_no_mark_on_the_job_limit() {
        let s = Slots::new(8);
        s.set_device_limit(CPU_DEVICE, 2);
        s.set_optimize(Some(1), None);
        let a = s.optimize(cpu_lane()).await;
        let b = s.optimize(cpu_lane()).await;
        let mut c = Box::pin(s.optimize(cpu_lane()));
        assert!(granted(&mut c).await.is_none(), "the device holds two");
        drop(a);
        assert!(granted(&mut c).await.is_some());
        drop(b);
    }

    #[tokio::test]
    async fn the_gpu_reserve_pauses_a_device_not_the_cpu() {
        let s = Slots::new(2);
        let gpu = s.optimize(gpu_lane()).await;
        let cpu = s.optimize(cpu_lane()).await;
        assert!(!*gpu.preempted.borrow() && !*cpu.preempted.borrow());
        let read = s.playback_on(Some("gpu0"));
        assert!(*gpu.preempted.borrow() && !*cpu.preempted.borrow());
        let mut again = Box::pin(s.optimize(gpu_lane()));
        assert!(granted(&mut again).await.is_none());
        s.set_optimize(None, Some(2));
        assert!(!*gpu.preempted.borrow(), "one read is below a reserve of 2");
        s.set_optimize(None, Some(0));
        drop(read);
        assert!(!*gpu.preempted.borrow());
    }

    #[tokio::test]
    async fn gpus_rank_by_reads_then_by_background_job() {
        let s = Slots::new(8);
        let devs: Vec<String> = vec!["gpu0".into(), "gpu1".into()];
        let _job = s.optimize(gpu_lane()).await;
        assert_eq!(
            s.rank_gpus(&devs),
            ["gpu1", "gpu0"],
            "gpu0 runs a background job"
        );
        let _a = s.playback_on(Some("gpu1"));
        assert_eq!(
            s.rank_gpus(&devs),
            ["gpu0", "gpu1"],
            "reads weigh more than the job"
        );
        assert!(s.rank_gpus(&[]).is_empty());
    }

    #[tokio::test]
    async fn the_reserve_of_a_device_overrides_the_global_one() {
        let s = Slots::new(8);
        s.set_optimize(None, Some(1));
        s.set_device_reserve("gpu0", Some(0));
        let _read = s.playback_on(Some("gpu0"));
        assert!(s.has_room(&gpu_lane()), "a reserve of 0 never pauses");
        s.set_device_reserve("gpu0", None);
        assert!(!s.has_room(&gpu_lane()), "the global reserve applies again");
    }

    #[test]
    fn a_device_failing_three_times_is_skipped_until_it_runs_again() {
        let s = Slots::new(8);
        for _ in 0..2 {
            s.device_failed("gpu1");
        }
        assert!(s.device_usable("gpu1"));
        s.device_failed("gpu1");
        assert!(!s.device_usable("gpu1"));
        assert!(s.device_usable("gpu0"));
        s.device_ok("gpu1");
        assert!(s.device_usable("gpu1"));
    }

    #[tokio::test]
    async fn a_lane_has_room_only_while_its_device_and_budget_allow() {
        let s = Slots::new(4);
        s.set_optimize(Some(2), Some(1));
        assert!(s.has_room(&cpu_lane()) && s.has_room(&gpu_lane()));
        let _p1 = s.playback_on(None);
        let _p2 = s.playback_on(None);
        let _p3 = s.playback_on(None);
        assert!(!s.has_room(&cpu_lane()), "playback leaves 1 share");
        assert!(s.has_room(&gpu_lane()), "a GPU run takes no CPU");
        let _read = s.playback_on(Some("gpu0"));
        assert!(!s.has_room(&gpu_lane()), "the GPU reserve is reached");
    }

    #[test]
    fn optimize_threads_follow_the_weight() {
        let s = Slots::new(cpu_quota().max(2));
        s.set_optimize(Some(1), None);
        let one = s.optimize_threads();
        s.set_optimize(Some(2), None);
        assert!(s.optimize_threads() >= one);
        assert!(s.optimize_threads() as usize <= cpu_quota());
    }
}
