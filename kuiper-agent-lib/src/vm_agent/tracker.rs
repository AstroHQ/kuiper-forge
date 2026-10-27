//! The VMs an agent is running, keyed by runner name, and how much of its capacity they use.
//!
//! Runner names are unique, so a late destroy for a finished runner can't touch a newer VM that
//! reused its backend ID (a Proxmox VMID, say).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use kuiper_agent_proto::{CapacityLimit, VmInfo};
use tokio::sync::{Notify, RwLock, watch};
use tokio::time::Instant;
use tracing::debug;

/// Where a VM is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    /// Being cloned from its image
    Creating,
    /// Booting, waiting for an IP and SSH
    Starting,
    /// Up, runner not set up yet
    Running,
    /// Installing and setting up the runner
    Configuring,
    /// Runner is processing a job
    RunnerActive,
    /// Being stopped and deleted
    Destroying,
}

impl VmState {
    /// Name for status reporting.
    pub fn as_str(&self) -> &'static str {
        match self {
            VmState::Creating => "creating",
            VmState::Starting => "starting",
            VmState::Running => "running",
            VmState::Configuring => "configuring",
            VmState::RunnerActive => "runner_active",
            VmState::Destroying => "destroying",
        }
    }
}

/// A host-wide limit on running VMs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limit {
    pub name: &'static str,
    pub max: u32,
}

/// Names of the [`Limit`]s a VM counts against, e.g. `["macos", "total"]`.
pub type LimitNames = &'static [&'static str];

/// VMs counted against each limit, by limit name.
pub type Usage = HashMap<&'static str, u32>;

/// Why a VM couldn't be reserved.
/// What a destroy should do with a VM, from [`Tracker::claim_for_destroy`].
pub enum Claim<V> {
    /// It isn't tracked: already destroyed, or never was
    Untracked,
    /// The caller has it and must destroy it, then [`Tracker::untrack`] it
    Destroy(V),
    /// Something else will destroy it: its lifecycle once the create finishes, or a destroy already under way.
    /// Wait for that
    Wait(Removal),
}

/// Resolves when a VM someone else is destroying is untracked.
pub struct Removal(watch::Receiver<Outcome>);

impl Removal {
    /// Wait for the VM to be untracked. Returns how its destroy went.
    pub async fn wait(mut self) -> Result<(), String> {
        // the sender is dropped with the tracked VM, which ends this
        while self.0.changed().await.is_ok() {}
        self.0.borrow().clone().unwrap_or(Ok(()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReserveError {
    #[error("Capacity exceeded: {0}")]
    Capacity(String),

    #[error("VM already running: {0}")]
    AlreadyRunning(String),
}

/// How a VM's destroy went, once it's untracked: `Err` holds the backend's error.
type Outcome = Option<Result<(), String>>;

struct Tracked<V> {
    /// The backend's VM, once created
    vm: Option<V>,
    /// Dropped (closing the channel) when the VM is untracked, after the destroy's outcome is set
    removed: watch::Sender<Outcome>,
    /// The backend's ID for it, the runner name until it's created
    vm_id: String,
    state: VmState,
    ip: Option<IpAddr>,
    limits: LimitNames,
    created_at: Instant,
}

/// The agent's VMs and capacity.
pub struct Tracker<V> {
    vms: RwLock<HashMap<String, Tracked<V>>>,
    limits: Vec<Limit>,
    /// Fired on every change worth re-sending status for
    changed: Arc<Notify>,
}

impl<V: Clone> Tracker<V> {
    pub fn new(limits: Vec<Limit>) -> Self {
        Self {
            vms: RwLock::new(HashMap::new()),
            limits,
            changed: Arc::new(Notify::new()),
        }
    }

    /// Fired whenever the VM set or a VM's state changes.
    pub fn changes(&self) -> Arc<Notify> {
        self.changed.clone()
    }

    /// Take a slot for a new VM `name` counting against `limits`. Checked and taken under one lock, so two
    /// creates can't both get the last slot.
    pub async fn reserve(
        &self,
        name: &str,
        limits: LimitNames,
        external: &Usage,
    ) -> Result<(), ReserveError> {
        let mut vms = self.vms.write().await;
        if vms.contains_key(name) {
            return Err(ReserveError::AlreadyRunning(name.to_string()));
        }
        let own = usage(&vms);
        if self.free(limits, &own, external) == 0 {
            return Err(ReserveError::Capacity(self.summary(&own, external)));
        }
        vms.insert(
            name.to_string(),
            Tracked {
                vm: None,
                removed: watch::Sender::new(None),
                vm_id: name.to_string(),
                state: VmState::Creating,
                ip: None,
                limits,
                created_at: Instant::now(),
            },
        );
        drop(vms);
        self.changed.notify_one();
        Ok(())
    }

    /// Record the backend's VM for `name`. Returns false if `name` was destroyed while it was being
    /// created, in which case the caller still owns `vm` and must destroy it.
    pub async fn set_created(&self, name: &str, vm: V, vm_id: String, limits: LimitNames) -> bool {
        let mut vms = self.vms.write().await;
        let Some(tracked) = vms.get_mut(name) else {
            return false;
        };
        if tracked.state == VmState::Destroying {
            return false;
        }
        tracked.vm = Some(vm);
        tracked.vm_id = vm_id;
        tracked.limits = limits;
        drop(vms);
        self.changed.notify_one();
        true
    }

    /// Move `name` to `state`, unless it's being destroyed.
    pub async fn set_state(&self, name: &str, state: VmState) {
        let mut vms = self.vms.write().await;
        if let Some(tracked) = vms.get_mut(name)
            && tracked.state != VmState::Destroying
        {
            debug!("VM {} state: {:?} -> {:?}", name, tracked.state, state);
            tracked.state = state;
            drop(vms);
            self.changed.notify_one();
        }
    }

    /// Record `name`'s IP and mark it running.
    pub async fn set_running(&self, name: &str, ip: IpAddr) {
        if let Some(tracked) = self.vms.write().await.get_mut(name) {
            tracked.ip = Some(ip);
        }
        self.set_state(name, VmState::Running).await;
    }

    /// Mark `name` as being destroyed, and say who destroys it. A VM still being created, or already being
    /// destroyed, is destroyed by whoever has it; the caller waits for that.
    pub async fn claim_for_destroy(&self, name: &str) -> Claim<V> {
        let mut vms = self.vms.write().await;
        let Some(tracked) = vms.get_mut(name) else {
            debug!("{} isn't tracked (already destroyed)", name);
            return Claim::Untracked;
        };
        if tracked.state == VmState::Destroying {
            debug!("{} is already being destroyed, waiting for that", name);
            return Claim::Wait(Removal(tracked.removed.subscribe()));
        }
        tracked.state = VmState::Destroying;
        let claim = match &tracked.vm {
            Some(vm) => Claim::Destroy(vm.clone()),
            // the lifecycle sees it's being destroyed once the create finishes, and destroys it
            None => Claim::Wait(Removal(tracked.removed.subscribe())),
        };
        drop(vms);
        self.changed.notify_one();
        claim
    }

    /// Stop tracking `name`, telling anyone waiting on its destroy how it went.
    pub async fn untrack(&self, name: &str, outcome: Result<(), String>) {
        let removed = self.vms.write().await.remove(name);
        if let Some(tracked) = removed {
            tracked.removed.send_replace(Some(outcome));
            self.changed.notify_one();
        }
    }

    /// Resolves once `name` is destroyed or being destroyed. Checked every 5s.
    pub async fn torn_down(&self, name: &str) {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let alive = self
                .vms
                .read()
                .await
                .get(name)
                .is_some_and(|t| t.state != VmState::Destroying);
            if !alive {
                return;
            }
        }
    }

    /// The runner name for `id`, which is a runner name or a backend VM ID.
    pub async fn find(&self, id: &str) -> Option<String> {
        let vms = self.vms.read().await;
        if vms.contains_key(id) {
            return Some(id.to_string());
        }
        vms.iter()
            .find(|(_, t)| t.vm_id == id)
            .map(|(name, _)| name.clone())
    }

    /// Names of every tracked VM.
    pub async fn names(&self) -> Vec<String> {
        self.vms.read().await.keys().cloned().collect()
    }

    /// Names of VMs created more than `max_age` ago.
    pub async fn older_than(&self, max_age: Duration) -> Vec<String> {
        self.vms
            .read()
            .await
            .iter()
            .filter(|(_, t)| t.created_at.elapsed() > max_age)
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub async fn count(&self) -> usize {
        self.vms.read().await.len()
    }

    /// The tracked VMs, for `AgentStatus`.
    pub async fn vm_infos(&self) -> Vec<VmInfo> {
        self.vms
            .read()
            .await
            .iter()
            .map(|(name, t)| VmInfo {
                vm_id: t.vm_id.clone(),
                name: name.clone(),
                state: t.state.as_str().to_string(),
                ip_address: t.ip.map(|ip| ip.to_string()).unwrap_or_default(),
            })
            .collect()
    }

    /// Slots free for a VM counting against `limits`, after `external` VMs.
    pub async fn free_slots(&self, limits: LimitNames, external: &Usage) -> u32 {
        let own = usage(&*self.vms.read().await);
        self.free(limits, &own, external)
    }

    /// Most VMs counting against `limits` that fit when nothing else runs.
    pub fn max_vms(&self, limits: LimitNames) -> u32 {
        self.limits_in(limits).map(|l| l.max).min().unwrap_or(0)
    }

    /// Every limit with its usage, for `AgentStatus`.
    pub async fn capacity_limits(&self, external: &Usage) -> Vec<CapacityLimit> {
        let own = usage(&*self.vms.read().await);
        self.limits
            .iter()
            .map(|l| CapacityLimit {
                name: l.name.to_string(),
                max: l.max,
                external: count(external, l.name),
                active: count(&own, l.name),
            })
            .collect()
    }

    fn limits_in(&self, names: LimitNames) -> impl Iterator<Item = &Limit> {
        self.limits.iter().filter(move |l| names.contains(&l.name))
    }

    fn free(&self, limits: LimitNames, own: &Usage, external: &Usage) -> u32 {
        self.limits_in(limits)
            .map(|l| {
                l.max
                    .saturating_sub(count(own, l.name) + count(external, l.name))
            })
            .min()
            .unwrap_or(0)
    }

    fn summary(&self, own: &Usage, external: &Usage) -> String {
        self.limits
            .iter()
            .map(|l| {
                let ext = count(external, l.name);
                format!(
                    "{} {}/{} ({} external)",
                    l.name,
                    count(own, l.name) + ext,
                    l.max,
                    ext
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn usage<V>(vms: &HashMap<String, Tracked<V>>) -> Usage {
    let mut usage = Usage::new();
    for name in vms.values().flat_map(|t| t.limits) {
        *usage.entry(name).or_default() += 1;
    }
    usage
}

fn count(usage: &Usage, name: &str) -> u32 {
    usage.get(name).copied().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MACOS: LimitNames = &["macos", "total"];
    const LINUX: LimitNames = &["total"];

    fn tracker(macos: u32, total: u32) -> Tracker<u32> {
        Tracker::new(vec![
            Limit {
                name: "macos",
                max: macos,
            },
            Limit {
                name: "total",
                max: total,
            },
        ])
    }

    fn usage_of(macos: u32, total: u32) -> Usage {
        Usage::from([("macos", macos), ("total", total)])
    }

    #[test]
    fn test_vm_state_as_str() {
        assert_eq!(VmState::Creating.as_str(), "creating");
        assert_eq!(VmState::Running.as_str(), "running");
        assert_eq!(VmState::RunnerActive.as_str(), "runner_active");
        assert_eq!(VmState::Destroying.as_str(), "destroying");
    }

    #[test]
    fn test_free_counts_external_vms() {
        let t = tracker(2, 3);
        let none = Usage::new();

        // one external mac leaves one mac slot
        assert_eq!(t.free(MACOS, &none, &usage_of(1, 1)), 1);
        assert_eq!(t.free(MACOS, &usage_of(1, 1), &usage_of(1, 1)), 0);

        // external linux VMs only use up the total limit
        assert_eq!(t.free(MACOS, &none, &usage_of(0, 2)), 1);
        assert_eq!(t.free(MACOS, &none, &usage_of(0, 3)), 0);

        // over the limit doesn't underflow
        assert_eq!(t.free(MACOS, &usage_of(2, 2), &usage_of(2, 4)), 0);
    }

    #[test]
    fn test_free_linux_skips_macos_limit() {
        let t = tracker(2, 4);
        let none = Usage::new();

        // both mac slots taken: no more mac, linux still fits
        assert_eq!(t.free(MACOS, &usage_of(2, 2), &none), 0);
        assert_eq!(t.free(LINUX, &usage_of(2, 2), &none), 2);

        // linux VMs use up mac slots only through the total limit
        assert_eq!(t.free(MACOS, &usage_of(0, 3), &none), 1);
        assert_eq!(t.free(LINUX, &usage_of(1, 3), &usage_of(0, 1)), 0);
    }

    #[test]
    fn test_max_vms() {
        let t = tracker(2, 3);
        assert_eq!(t.max_vms(MACOS), 2);
        assert_eq!(t.max_vms(LINUX), 3);
    }

    #[tokio::test]
    async fn test_reserve_checks_capacity_and_names() {
        let t = tracker(1, 2);
        let none = Usage::new();
        t.reserve("a", MACOS, &none).await.unwrap();
        assert!(matches!(
            t.reserve("a", LINUX, &none).await,
            Err(ReserveError::AlreadyRunning(_))
        ));
        assert!(matches!(
            t.reserve("b", MACOS, &none).await,
            Err(ReserveError::Capacity(_))
        ));
        t.reserve("b", LINUX, &none).await.unwrap();
        assert_eq!(t.count().await, 2);

        let limits = t.capacity_limits(&none).await;
        assert_eq!((limits[0].name.as_str(), limits[0].active), ("macos", 1));
        assert_eq!((limits[1].name.as_str(), limits[1].active), ("total", 2));
    }

    #[tokio::test]
    async fn test_destroy_after_destroy_is_noop() {
        let t = tracker(2, 2);
        t.reserve("runner-a", LINUX, &Usage::new()).await.unwrap();
        t.set_created("runner-a", 120, "120".into(), LINUX).await;

        assert!(matches!(
            t.claim_for_destroy("runner-a").await,
            Claim::Destroy(120)
        ));
        // a second destroy while the first is in flight waits for it
        let Claim::Wait(second) = t.claim_for_destroy("runner-a").await else {
            panic!("second destroy should wait");
        };
        t.untrack("runner-a", Err("delete failed".into())).await;
        assert_eq!(second.wait().await, Err("delete failed".to_string()));

        // the backend hands 120 to another runner's VM
        t.reserve("runner-b", LINUX, &Usage::new()).await.unwrap();
        t.set_created("runner-b", 120, "120".into(), LINUX).await;

        // runner-a's late cleanup leaves runner-b alone
        assert!(matches!(
            t.claim_for_destroy("runner-a").await,
            Claim::Untracked
        ));
        assert_eq!(t.find("120").await.as_deref(), Some("runner-b"));
    }

    #[tokio::test]
    async fn test_destroy_while_creating() {
        let t = tracker(2, 2);
        t.reserve("runner-c", LINUX, &Usage::new()).await.unwrap();
        let Claim::Wait(removal) = t.claim_for_destroy("runner-c").await else {
            panic!("a VM being created is destroyed by its lifecycle");
        };
        let waiter = tokio::spawn(removal.wait());
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "the destroy waits for the create");

        // the create finishing finds it destroyed, and keeps the VM to destroy itself
        assert!(!t.set_created("runner-c", 7, "7".into(), LINUX).await);
        t.untrack("runner-c", Ok(())).await;
        assert_eq!(waiter.await.unwrap(), Ok(()));
    }

    #[tokio::test(start_paused = true)]
    async fn test_torn_down() {
        let t = tracker(2, 2);
        t.reserve("runner-d", LINUX, &Usage::new()).await.unwrap();
        let still_running =
            tokio::time::timeout(Duration::from_secs(60), t.torn_down("runner-d")).await;
        assert!(still_running.is_err(), "a running VM isn't torn down");

        t.claim_for_destroy("runner-d").await;
        tokio::time::timeout(Duration::from_secs(10), t.torn_down("runner-d"))
            .await
            .expect("a VM being destroyed counts as torn down");

        t.untrack("runner-d", Ok(())).await;
        tokio::time::timeout(Duration::from_secs(10), t.torn_down("runner-d"))
            .await
            .expect("an untracked VM counts as torn down");
    }

    #[tokio::test]
    async fn test_find_by_name_or_vm_id() {
        let t = tracker(2, 2);
        t.reserve("runner-e", LINUX, &Usage::new()).await.unwrap();
        // until created, the VM ID is the name
        assert_eq!(t.find("runner-e").await.as_deref(), Some("runner-e"));
        t.set_created("runner-e", 9, "9".into(), LINUX).await;
        assert_eq!(t.find("9").await.as_deref(), Some("runner-e"));
        assert_eq!(t.find("10").await, None);
    }
}
