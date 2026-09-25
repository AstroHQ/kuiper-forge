//! Agent registry for tracking connected agents.
//!
//! Maintains a registry of all connected agents (both Tart and Proxmox),
//! provides label-based agent matching, and handles command routing.

// Allow dead code for fields/methods that may be useful for future features

use anyhow::{Result, anyhow};
use kuiper_agent_proto::{AgentMessage, CoordinatorMessage};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, mpsc, oneshot};
use tracing::{debug, info, warn};

/// Type of agent (Tart for macOS, Proxmox for Windows/Linux)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentType {
    Tart,
    Proxmox,
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentType::Tart => write!(f, "tart"),
            AgentType::Proxmox => write!(f, "proxmox"),
        }
    }
}

impl std::str::FromStr for AgentType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "tart" => Ok(AgentType::Tart),
            "proxmox" => Ok(AgentType::Proxmox),
            _ => Err(anyhow!("Unknown agent type: {s}")),
        }
    }
}

/// A host-wide limit an agent reports on top of `max_vms`, e.g. tart's macOS guest limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmLimit {
    pub name: String,
    pub max: usize,
    /// VMs using up this limit that the agent doesn't manage
    pub external: usize,
    /// The agent's own VMs using up this limit. Always 0 from older agents, use `ConnectedAgent::limit_active`
    pub active: usize,
}

impl From<&kuiper_agent_proto::CapacityLimit> for VmLimit {
    fn from(l: &kuiper_agent_proto::CapacityLimit) -> Self {
        Self {
            name: l.name.clone(),
            max: l.max as usize,
            external: l.external as usize,
            active: l.active as usize,
        }
    }
}

/// An agent's limits, which of them each label set's VMs count against, and its fixed-capacity pools, from its
/// latest status.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentCapacity {
    pub limits: Vec<VmLimit>,
    /// Limit names per label set, same order as `label_sets`. All empty from older agents, where every VM counts
    /// against every limit
    pub label_set_limits: Vec<Vec<String>>,
    /// Pool size per label set, same order as `label_sets`. All `None` means the legacy pool (base labels, `max_vms`)
    pub label_set_pools: Vec<Option<u32>>,
    /// Agent-chosen id per label set, same order as `label_sets`. Empty from older agents
    pub label_set_ids: Vec<String>,
    /// Index of the set for the agent's default resource. None from older agents
    pub default_set: Option<usize>,
}

impl From<&kuiper_agent_proto::AgentStatus> for AgentCapacity {
    fn from(status: &kuiper_agent_proto::AgentStatus) -> Self {
        Self {
            limits: status.limits.iter().map(VmLimit::from).collect(),
            label_set_limits: status
                .label_sets
                .iter()
                .map(|ls| ls.limits.clone())
                .collect(),
            label_set_pools: status.label_sets.iter().map(|ls| ls.pool_size).collect(),
            label_set_ids: status.label_sets.iter().map(|ls| ls.id.clone()).collect(),
            default_set: status.label_sets.iter().position(|ls| ls.is_default),
        }
    }
}

/// A reserved slot for one runner.
#[derive(Debug)]
struct Reservation {
    /// Names of the limits it counts against. Empty for agents without per-set limits
    limits: Vec<String>,
    at: std::time::Instant,
}

/// Returns true if every label in `required` is in `set` (case-insensitive).
fn set_covers(set: &[String], required: &[String]) -> bool {
    required
        .iter()
        .all(|r| set.iter().any(|label| label.eq_ignore_ascii_case(r)))
}

/// Information about a connected agent
#[derive(Debug)]
pub struct ConnectedAgent {
    /// Unique agent identifier (from certificate CN)
    pub agent_id: String,

    /// Type of agent (Tart or Proxmox)
    pub agent_type: AgentType,

    /// Hostname where the agent is running
    pub hostname: String,

    /// Maximum VMs this agent can manage
    pub max_vms: usize,

    /// Currently active VMs (as reported by agent)
    pub active_vms: usize,

    /// Extra limits from the agent's last status, empty for agents that only have `max_vms`
    pub limits: Vec<VmLimit>,

    /// Which `limits` each label set's VMs count against, same order as `label_sets`
    label_set_limits: Vec<Vec<String>>,

    /// Fixed-capacity pool size per label set, same order as `label_sets`
    label_set_pools: Vec<Option<u32>>,

    /// Agent-chosen id per label set, same order as `label_sets`
    label_set_ids: Vec<String>,

    /// Index of the set for the agent's default resource
    default_set: Option<usize>,

    /// Reserved slots by runner name: commands sent whose VM isn't in the agent's status yet. This prevents
    /// over-scheduling when sending multiple commands quickly. An entry goes when its runner is released or its VM
    /// shows up, so releasing a runner that already has a VM can't free someone else's slot
    reservations: HashMap<String, Reservation>,

    /// Labels this agent supports (e.g., ["macos", "arm64"])
    /// Deprecated: use label_sets for capability-based matching
    pub labels: Vec<String>,

    /// Label sets representing capabilities this agent can fulfill.
    /// Each set is one capability (e.g., base labels + one image_mapping).
    /// A job matches if its labels are a subset of ANY label set.
    pub label_sets: Vec<Vec<String>>,

    /// Channel to send commands to this agent
    pub command_tx: mpsc::Sender<CoordinatorMessage>,

    /// Pending commands waiting for responses
    pending_commands: RwLock<HashMap<String, oneshot::Sender<AgentMessage>>>,

    /// Last time we heard from this agent
    pub last_seen: std::time::Instant,
}

impl ConnectedAgent {
    /// Create a new connected agent
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent_id: String,
        agent_type: AgentType,
        hostname: String,
        max_vms: usize,
        active_vms: usize,
        labels: Vec<String>,
        label_sets: Vec<Vec<String>>,
        command_tx: mpsc::Sender<CoordinatorMessage>,
    ) -> Self {
        Self {
            agent_id,
            agent_type,
            hostname,
            max_vms,
            active_vms,
            limits: Vec::new(),
            label_set_limits: Vec::new(),
            label_set_pools: Vec::new(),
            label_set_ids: Vec::new(),
            default_set: None,
            reservations: HashMap::new(),
            labels,
            label_sets,
            command_tx,
            pending_commands: RwLock::new(HashMap::new()),
            last_seen: std::time::Instant::now(),
        }
    }

    /// Label sets to show people. The default set is just the base labels, and every mapping's set covers it, so
    /// it's only listed when there's nothing else
    fn display_label_sets(&self) -> Vec<Vec<String>> {
        if self.label_sets.len() <= 1 {
            return self.label_sets.clone();
        }
        self.label_sets
            .iter()
            .enumerate()
            .filter(|(i, _)| Some(*i) != self.default_set)
            .map(|(_, set)| set.clone())
            .collect()
    }

    /// Returns true if the agent says which limits each label set's VMs count against. Older agents don't, and every
    /// VM counts against every limit.
    fn limits_per_set(&self) -> bool {
        self.label_set_limits.iter().any(|l| !l.is_empty())
    }

    /// The agent's own VMs counted against `limit`.
    fn limit_active(&self, limit: &VmLimit) -> usize {
        if self.limits_per_set() {
            limit.active
        } else {
            self.active_vms
        }
    }

    /// Index of the label set a runner for this job is created from: the one named by `set_id`, else the one the
    /// agent's own matching picks (see `kuiper_agent_lib::labels::mapping_for`): the first set that adds a label the
    /// job asks for and covers it, else the default set. None for older agents that don't advertise their default set.
    fn selected_set(&self, labels: &[String], set_id: &str) -> Option<usize> {
        if !set_id.is_empty()
            && let Some(i) = self.label_set_ids.iter().position(|id| id == set_id)
        {
            return Some(i);
        }
        let default_set = self.default_set?;
        let is_base = |l: &String| self.labels.iter().any(|b| b.eq_ignore_ascii_case(l));
        let asks_for_extra = |set: &Vec<String>| {
            set.iter()
                .filter(|l| !is_base(l))
                .any(|l| labels.iter().any(|j| j.eq_ignore_ascii_case(l)))
        };
        self.label_sets
            .iter()
            .enumerate()
            .position(|(i, set)| i != default_set && asks_for_extra(set) && set_covers(set, labels))
            .or_else(|| {
                self.label_sets
                    .get(default_set)
                    .is_some_and(|set| set_covers(set, labels))
                    .then_some(default_set)
            })
    }

    /// Limits a job with these labels counts against: the ones of the set it's created from. When we can't tell which
    /// set that is, the union over every set it matches.
    fn limits_for(&self, labels: &[String], set_id: &str) -> impl Iterator<Item = &VmLimit> {
        let names: Option<Vec<&String>> =
            self.limits_per_set()
                .then(|| match self.selected_set(labels, set_id) {
                    Some(i) => self
                        .label_set_limits
                        .get(i)
                        .map(|names| names.iter().collect())
                        .unwrap_or_default(),
                    None => self
                        .label_sets
                        .iter()
                        .zip(&self.label_set_limits)
                        .filter(|(set, _)| set_covers(set, labels))
                        .flat_map(|(_, names)| names)
                        .collect(),
                });
        self.limits
            .iter()
            .filter(move |l| names.as_ref().is_none_or(|names| names.contains(&&l.name)))
    }

    /// Slots reserved for VMs that haven't shown up in the agent's status yet.
    pub fn reserved_slots(&self) -> usize {
        self.reservations.len()
    }

    /// Slots reserved against `limit`.
    fn limit_reserved(&self, limit: &VmLimit) -> usize {
        if !self.limits_per_set() {
            return self.reserved_slots();
        }
        self.reservations
            .values()
            .filter(|r| r.limits.contains(&limit.name))
            .count()
    }

    /// How many VMs of any kind this agent can run right now: `max_vms`, lowered by the limits every VM counts against
    /// that VMs outside the agent use up.
    pub fn effective_max(&self) -> usize {
        let per_set = self.limits_per_set();
        self.limits
            .iter()
            .filter(|l| {
                !per_set
                    || self
                        .label_set_limits
                        .iter()
                        .all(|names| names.contains(&l.name))
            })
            .map(|l| l.max.saturating_sub(l.external))
            .fold(self.max_vms, usize::min)
    }

    /// Most VMs for a job with these labels this agent can run at once right now, with nothing else of its own
    /// running.
    pub fn ceiling_for(&self, labels: &[String]) -> usize {
        self.limits_for(labels, "")
            .map(|l| l.max.saturating_sub(l.external))
            .fold(self.max_vms, usize::min)
    }

    /// Returns true if the agent sets its own fixed-capacity pools per label set.
    pub fn has_explicit_pools(&self) -> bool {
        self.label_set_pools.iter().any(Option::is_some)
    }

    /// Returns true if the agent is part of the legacy pool with these (normalized) labels.
    pub fn in_legacy_pool(&self, pool_labels: &[String]) -> bool {
        !self.has_explicit_pools() && normalize_labels(&self.labels) == pool_labels
    }

    /// Check if this agent has capacity for a job with these labels, created from the label set `set_id` if set
    /// Takes into account both active VMs and reserved slots
    pub fn has_capacity(&self, labels: &[String], set_id: &str) -> bool {
        self.available_capacity(labels, set_id) > 0
    }

    /// Get available capacity for a job with these labels, created from the label set `set_id` if set (number of VMs
    /// that can still be created)
    /// Takes into account both active VMs and reserved slots
    pub fn available_capacity(&self, labels: &[String], set_id: &str) -> usize {
        self.limits_for(labels, set_id)
            .map(|l| {
                l.max
                    .saturating_sub(l.external + self.limit_active(l) + self.limit_reserved(l))
            })
            .fold(
                self.max_vms
                    .saturating_sub(self.active_vms + self.reserved_slots()),
                usize::min,
            )
    }

    /// Reserve a slot for an upcoming VM creation for runner `runner_name`
    /// Returns true if reservation succeeded, false if no capacity
    pub fn reserve_slot(&mut self, labels: &[String], set_id: &str, runner_name: &str) -> bool {
        if !self.has_capacity(labels, set_id) {
            return false;
        }
        let limits = if self.limits_per_set() {
            self.limits_for(labels, set_id)
                .map(|l| l.name.clone())
                .collect()
        } else {
            Vec::new()
        };
        self.reservations.insert(
            runner_name.to_string(),
            Reservation {
                limits,
                at: std::time::Instant::now(),
            },
        );
        true
    }

    /// Release the reserved slot of runner `runner_name` (call when command completes or fails). Nothing to do
    /// when its VM already showed up.
    pub fn release_slot(&mut self, runner_name: &str) {
        self.reservations.remove(runner_name);
    }

    /// Check if this agent can handle a job with the given labels.
    ///
    /// If label_sets are configured, returns true if required_labels is a subset
    /// of ANY label set (each set represents one capability the agent can fulfill).
    ///
    /// Falls back to legacy flat labels matching if no label_sets are configured.
    pub fn matches_labels(&self, required_labels: &[String]) -> bool {
        if !self.label_sets.is_empty() {
            // New capability-based matching: job labels must be subset of ANY label set
            self.label_sets
                .iter()
                .any(|label_set| set_covers(label_set, required_labels))
        } else {
            // Legacy flat labels matching
            set_covers(&self.labels, required_labels)
        }
    }

    /// Register a pending command and return the response channel
    pub async fn register_pending_command(
        &self,
        command_id: &str,
    ) -> oneshot::Receiver<AgentMessage> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending_commands.write().await;
        pending.insert(command_id.to_string(), tx);
        rx
    }

    /// Complete a pending command with a response
    pub async fn complete_command(&self, command_id: &str, response: AgentMessage) -> bool {
        let mut pending = self.pending_commands.write().await;
        match pending.remove(command_id) {
            Some(tx) => tx.send(response).is_ok(),
            _ => false,
        }
    }

    /// Update the last seen timestamp
    pub fn touch(&mut self) {
        self.last_seen = std::time::Instant::now();
    }

    /// Check if agent is considered stale (no messages for a while)
    pub fn is_stale(&self, timeout: Duration) -> bool {
        self.last_seen.elapsed() > timeout
    }
}

/// Registry of all connected agents
#[derive(Debug, Default)]
pub struct AgentRegistry {
    agents: RwLock<HashMap<String, Arc<RwLock<ConnectedAgent>>>>,
    /// When each agent last dropped, cleared again on register. Also the lock that serializes
    /// register against `claim_failover`, so a reconnect can't slip in between "still offline" and
    /// "fail its runners". Always taken before `agents`.
    disconnects: RwLock<HashMap<String, std::time::Instant>>,
}

impl AgentRegistry {
    /// Create a new empty registry
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new agent connection
    #[allow(clippy::too_many_arguments)]
    pub async fn register(
        &self,
        agent_id: String,
        agent_type: AgentType,
        hostname: String,
        max_vms: usize,
        active_vms: usize,
        labels: Vec<String>,
        label_sets: Vec<Vec<String>>,
        command_tx: mpsc::Sender<CoordinatorMessage>,
    ) -> Arc<RwLock<ConnectedAgent>> {
        let agent = Arc::new(RwLock::new(ConnectedAgent::new(
            agent_id.clone(),
            agent_type,
            hostname.clone(),
            max_vms,
            active_vms,
            labels.clone(),
            label_sets.clone(),
            command_tx,
        )));

        {
            let mut disconnects = self.disconnects.write().await;
            let mut agents = self.agents.write().await;
            agents.insert(agent_id.clone(), Arc::clone(&agent));
            disconnects.remove(&agent_id);
        }

        info!(
            agent_id = %agent_id,
            agent_type = %agent_type,
            hostname = %hostname,
            labels = ?labels,
            label_sets = ?label_sets,
            max_vms = max_vms,
            active_vms = active_vms,
            "Agent registered"
        );

        agent
    }

    /// Unregister an agent (on disconnect)
    ///
    /// This cancels all pending commands for the agent, which will cause
    /// the fleet manager to clean up any runners that were being created.
    ///
    /// Returns the disconnect stamp; compare with `last_disconnect` before acting on a grace timer.
    pub async fn unregister(&self, agent_id: &str) -> std::time::Instant {
        let stamp = std::time::Instant::now();
        let agent = {
            let mut disconnects = self.disconnects.write().await;
            let mut agents = self.agents.write().await;
            disconnects.insert(agent_id.to_string(), stamp);
            agents.remove(agent_id)
        };

        if let Some(agent) = agent {
            // Cancel all pending commands to unblock waiters
            // This causes send_command to return an error, triggering runner cleanup
            let pending_count = {
                let agent = agent.read().await;
                let mut pending = agent.pending_commands.write().await;
                let count = pending.len();
                pending.clear(); // Dropping senders causes receivers to error
                count
            };

            if pending_count > 0 {
                info!(
                    agent_id = %agent_id,
                    pending_commands = pending_count,
                    "Agent unregistered, cancelled pending commands"
                );
            } else {
                info!(agent_id = %agent_id, "Agent unregistered");
            }
        }

        stamp
    }

    /// Returns true if the agent is still offline from the disconnect identified by `stamp`, and
    /// consumes the stamp so nobody else claims it. Atomic with register: after this returns true,
    /// any reconnect happens strictly later, so runners snapshotted *before* the call can't belong
    /// to the new connection.
    pub async fn claim_failover(&self, agent_id: &str, stamp: std::time::Instant) -> bool {
        let mut disconnects = self.disconnects.write().await;
        let agents = self.agents.read().await;
        if agents.contains_key(agent_id) || disconnects.get(agent_id) != Some(&stamp) {
            return false;
        }
        disconnects.remove(agent_id);
        true
    }

    /// Get an agent by ID
    pub async fn get(&self, agent_id: &str) -> Option<Arc<RwLock<ConnectedAgent>>> {
        let agents = self.agents.read().await;
        agents.get(agent_id).cloned()
    }

    /// Find an agent with capacity matching the required labels
    pub async fn find_available_agent(&self, labels: &[String]) -> Option<String> {
        self.select_agent(labels, &[]).await
    }

    /// Pick the least loaded agent that matches the labels and has a free slot.
    ///
    /// Agents in `avoid` (ones that already failed this job) only get picked when no other agent
    /// fits, so a single-agent setup still retries instead of stranding the job.
    ///
    /// Least loaded = most free slots, then fewest in use, then id. Iterating the HashMap and taking
    /// the first match biased everything onto whichever agent happened to hash first.
    pub async fn select_agent(&self, labels: &[String], avoid: &[String]) -> Option<String> {
        self.select_agent_where(labels, avoid, |_| true).await
    }

    /// Pick the least loaded member of the legacy pool with these (normalized) labels that has a free slot.
    pub async fn select_legacy_pool_agent(&self, pool_labels: &[String]) -> Option<String> {
        self.select_agent_where(pool_labels, &[], |a| a.in_legacy_pool(pool_labels))
            .await
    }

    async fn select_agent_where(
        &self,
        labels: &[String],
        avoid: &[String],
        include: impl Fn(&ConnectedAgent) -> bool,
    ) -> Option<String> {
        let agents = self.agents.read().await;
        let mut reasons: Vec<String> = Vec::new();
        // (avoided, free, in_use, id) - sorted so a non-avoided agent with the most free slots wins
        let mut candidates: Vec<(bool, usize, usize, String)> = Vec::new();
        for (id, agent) in agents.iter() {
            let agent = agent.read().await;
            if !include(&agent) {
                continue;
            }
            let has_cap = agent.has_capacity(labels, "");
            let matches = agent.matches_labels(labels);
            if has_cap && matches {
                let in_use = agent.active_vms + agent.reserved_slots();
                candidates.push((
                    avoid.contains(id),
                    agent.available_capacity(labels, ""),
                    in_use,
                    id.clone(),
                ));
                continue;
            }
            reasons.push(format!(
                "{}: matches={}, capacity={} (active={}, reserved={}, max={}, usable={})",
                id,
                matches,
                has_cap,
                agent.active_vms,
                agent.reserved_slots(),
                agent.max_vms,
                agent.effective_max()
            ));
        }
        drop(agents);

        candidates.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then(b.1.cmp(&a.1))
                .then(a.2.cmp(&b.2))
                .then(a.3.cmp(&b.3))
        });
        if let Some((avoided, free, in_use, id)) = candidates.first() {
            if *avoided {
                warn!(
                    agent_id = %id,
                    "Only agents that already failed this job have capacity - retrying on one anyway"
                );
            } else {
                debug!(
                    agent_id = %id,
                    free_slots = free,
                    in_use = in_use,
                    candidates = candidates.len(),
                    "Selected least loaded agent"
                );
            }
            return Some(id.clone());
        }
        if reasons.is_empty() {
            warn!("No agents registered to handle labels {:?}", labels);
        } else {
            warn!(
                "No agent available for labels {:?} — checked {} agent(s): [{}]",
                labels,
                reasons.len(),
                reasons.join("; ")
            );
        }
        None
    }

    /// Find agents matching labels (may or may not have capacity)
    /// Useful for diagnostics and admin queries.
    pub async fn find_agents_by_labels(&self, labels: &[String]) -> Vec<String> {
        let agents = self.agents.read().await;
        let mut result = Vec::new();
        for (id, agent) in agents.iter() {
            if agent.read().await.matches_labels(labels) {
                result.push(id.clone());
            }
        }
        result
    }

    /// Get total available capacity for agents matching labels
    pub async fn available_capacity(&self, labels: &[String]) -> usize {
        let agents = self.agents.read().await;
        let mut total = 0;
        for agent in agents.values() {
            let agent = agent.read().await;
            if agent.matches_labels(labels) {
                total += agent.available_capacity(labels, "");
            }
        }
        total
    }

    /// Free slots across the members of the legacy pool with these (normalized) labels.
    pub async fn legacy_pool_capacity(&self, pool_labels: &[String]) -> usize {
        let agents = self.agents.read().await;
        let mut total = 0;
        for agent in agents.values() {
            let agent = agent.read().await;
            if agent.in_legacy_pool(pool_labels) {
                total += agent.available_capacity(pool_labels, "");
            }
        }
        total
    }

    /// Send a command to an agent and wait for response
    pub async fn send_command(
        &self,
        agent_id: &str,
        command: CoordinatorMessage,
        command_id: &str,
        timeout: Duration,
    ) -> Result<AgentMessage> {
        let agent = self
            .get(agent_id)
            .await
            .ok_or_else(|| anyhow!("Agent not found: {agent_id}"))?;

        // Register pending command
        let rx = {
            let agent = agent.read().await;
            let rx = agent.register_pending_command(command_id).await;

            // Send the command
            agent
                .command_tx
                .send(command)
                .await
                .map_err(|_| anyhow!("Failed to send command to agent"))?;

            rx
        };

        // Wait for response with timeout
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(_)) => Err(anyhow!("Command response channel closed")),
            Err(_) => {
                // Clean up pending command on timeout
                let agent = agent.read().await;
                let mut pending = agent.pending_commands.write().await;
                pending.remove(command_id);
                Err(anyhow!("Command timed out"))
            }
        }
    }

    /// Handle a response from an agent
    pub async fn handle_response(&self, agent_id: &str, command_id: &str, response: AgentMessage) {
        if let Some(agent) = self.get(agent_id).await {
            let agent = agent.read().await;
            if !agent.complete_command(command_id, response).await {
                warn!(
                    agent_id = %agent_id,
                    command_id = %command_id,
                    "Received response for unknown command"
                );
            }
        }
    }

    /// Update agent status from a periodic AgentStatus message.
    ///
    /// Every field the agent reports (max_vms, labels, label_sets, active_vms)
    /// is treated as the live truth — the agent's running config wins. This is
    /// what lets a user edit the agent's config (e.g. `concurrent_vms`) and
    /// have the coordinator pick up the new value without a full reconnect.
    pub async fn update_status(
        &self,
        agent_id: &str,
        active_vms: usize,
        max_vms: usize,
        labels: Vec<String>,
        label_sets: Vec<Vec<String>>,
    ) {
        if let Some(agent) = self.get(agent_id).await {
            let mut agent = agent.write().await;
            let old_active = agent.active_vms;
            let old_max = agent.max_vms;
            agent.active_vms = active_vms;

            // Refresh fields that come from the agent's config so live config
            // changes propagate. Logged at info! when max_vms changes since
            // that affects scheduling capacity.
            if old_max != max_vms {
                info!(
                    agent_id = %agent_id,
                    old_max = old_max,
                    new_max = max_vms,
                    "Agent max_vms changed via status update"
                );
                agent.max_vms = max_vms;
            }
            if agent.labels != labels {
                debug!(
                    agent_id = %agent_id,
                    old = ?agent.labels,
                    new = ?labels,
                    "Agent labels changed via status update"
                );
                agent.labels = labels;
            }
            if agent.label_sets != label_sets {
                debug!(agent_id = %agent_id, "Agent label_sets changed via status update");
                agent.label_sets = label_sets;
            }

            // reservations turn into VMs by name in settle_reservations, so the counts here don't touch them

            agent.touch();

            debug!(
                agent_id = %agent_id,
                active_vms = active_vms,
                old_active = old_active,
                reserved_slots = agent.reserved_slots(),
                "Agent status updated"
            );
        }
    }

    /// Drop the reservations of runners whose VMs are in the agent's latest status: they count as active now. Call
    /// after `update_status`, so there's no moment where a VM counts as neither.
    pub async fn settle_reservations(&self, agent_id: &str, vm_names: &[String]) {
        if let Some(agent) = self.get(agent_id).await {
            let mut agent = agent.write().await;
            let before = agent.reserved_slots();
            agent
                .reservations
                .retain(|runner_name, _| !vm_names.contains(runner_name));
            if agent.reserved_slots() != before {
                debug!(
                    agent_id = %agent_id,
                    reserved_slots = agent.reserved_slots(),
                    old_reserved = before,
                    "Reservations turned into VMs"
                );
            }
        }
    }

    /// Replace the agent's limits and pools with the ones from its latest status.
    pub async fn set_capacity(&self, agent_id: &str, capacity: AgentCapacity) {
        if let Some(agent) = self.get(agent_id).await {
            let mut agent = agent.write().await;
            if agent.limits != capacity.limits
                || agent.label_set_limits != capacity.label_set_limits
                || agent.label_set_pools != capacity.label_set_pools
                || agent.label_set_ids != capacity.label_set_ids
                || agent.default_set != capacity.default_set
            {
                debug!(agent_id = %agent_id, capacity = ?capacity, "Agent capacity changed");

                agent.limits = capacity.limits;
                agent.label_set_limits = capacity.label_set_limits;
                agent.label_set_pools = capacity.label_set_pools;
                agent.label_set_ids = capacity.label_set_ids;
                agent.default_set = capacity.default_set;
            }
        }
    }

    /// Free slots on one agent for a job with these labels, created from the label set `set_id` if set. 0 if it's
    /// not connected.
    pub async fn agent_capacity(&self, agent_id: &str, labels: &[String], set_id: &str) -> usize {
        match self.get(agent_id).await {
            Some(agent) => agent.read().await.available_capacity(labels, set_id),
            None => 0,
        }
    }

    /// Update agent's last_seen timestamp without changing VM counts.
    /// Used for heartbeat/pong responses.
    pub async fn touch(&self, agent_id: &str) {
        if let Some(agent) = self.get(agent_id).await {
            let mut agent = agent.write().await;
            agent.touch();
        }
    }

    /// Reserve a slot on an agent for an upcoming VM creation
    /// Returns true if reservation succeeded
    pub async fn reserve_slot(
        &self,
        agent_id: &str,
        labels: &[String],
        set_id: &str,
        runner_name: &str,
    ) -> bool {
        match self.get(agent_id).await {
            Some(agent) => {
                let mut agent = agent.write().await;
                let reserved = agent.reserve_slot(labels, set_id, runner_name);
                if reserved {
                    debug!(
                        agent_id = %agent_id,
                        reserved_slots = agent.reserved_slots(),
                        "Slot reserved"
                    );
                }
                reserved
            }
            _ => false,
        }
    }

    /// Release runner `runner_name`'s reserved slot on an agent
    pub async fn release_slot(&self, agent_id: &str, runner_name: &str) {
        if let Some(agent) = self.get(agent_id).await {
            let mut agent = agent.write().await;
            agent.release_slot(runner_name);
            debug!(
                agent_id = %agent_id,
                reserved_slots = agent.reserved_slots(),
                "Slot released"
            );
        }
    }

    /// Drop reservations whose runner is gone from the DB, e.g. a CreateRunner that was acked but whose runner record
    /// was already cleaned up. Only ones older than `min_age`: a reservation is made before its runner is saved.
    pub async fn reconcile_reservations(
        &self,
        agent_id: &str,
        db_runners: &[String],
        min_age: Duration,
    ) {
        if let Some(agent) = self.get(agent_id).await {
            let mut agent = agent.write().await;
            let stale: Vec<String> = agent
                .reservations
                .iter()
                .filter(|(name, r)| r.at.elapsed() >= min_age && !db_runners.contains(name))
                .map(|(name, _)| name.clone())
                .collect();
            for name in &stale {
                agent.reservations.remove(name);
            }
            if !stale.is_empty() {
                info!(
                    agent_id = %agent_id,
                    runners = ?stale,
                    reserved_slots = agent.reserved_slots(),
                    "Dropped reservations for runners no longer in the DB"
                );
            }
        }
    }

    /// List all connected agents
    pub async fn list_all(&self) -> Vec<AgentInfo> {
        let agents = self.agents.read().await;
        let mut result = Vec::new();
        for agent in agents.values() {
            let agent = agent.read().await;
            result.push(AgentInfo {
                agent_id: agent.agent_id.clone(),
                agent_type: agent.agent_type,
                hostname: agent.hostname.clone(),
                labels: agent.labels.clone(),
                label_sets: agent.label_sets.clone(),
                display_label_sets: agent.display_label_sets(),
                max_vms: agent.max_vms,
                active_vms: agent.active_vms,
                limits: agent
                    .limits
                    .iter()
                    .map(|l| VmLimit {
                        active: agent.limit_active(l),
                        ..l.clone()
                    })
                    .collect(),
                explicit_pools: agent.has_explicit_pools(),
                last_seen_secs: agent.last_seen.elapsed().as_secs(),
            });
        }
        result
    }

    /// Get count of connected agents
    pub async fn count(&self) -> usize {
        let agents = self.agents.read().await;
        agents.len()
    }

    /// Remove stale agents that haven't been seen for a while
    pub async fn remove_stale(&self, timeout: Duration) -> Vec<String> {
        let mut to_remove = Vec::new();

        {
            let agents = self.agents.read().await;
            for (id, agent) in agents.iter() {
                let agent = agent.read().await;
                if agent.is_stale(timeout) {
                    to_remove.push(id.clone());
                }
            }
        }

        for id in &to_remove {
            self.unregister(id).await;
            warn!(agent_id = %id, "Removed stale agent");
        }

        to_remove
    }

    /// Get pool definitions derived from connected agents.
    ///
    /// Groups agents by their unique label combinations and calculates
    /// target counts (sum of max_vms) for each pool.
    ///
    /// This is used in the new agent-driven mode where pools are not
    /// statically configured but derived from connected agents.
    pub async fn get_pool_definitions(&self) -> Vec<PoolDefinition> {
        use std::collections::HashMap;

        let agents = self.agents.read().await;
        let mut legacy: HashMap<Vec<String>, u32> = HashMap::new();
        let mut explicit = Vec::new();

        for agent in agents.values() {
            let agent = agent.read().await;

            if agent.has_explicit_pools() {
                for (i, set) in agent.label_sets.iter().enumerate() {
                    let target_count = agent.label_set_pools.get(i).copied().flatten().unwrap_or(0);
                    if target_count > 0 {
                        explicit.push(PoolDefinition {
                            labels: normalize_labels(set),
                            target_count,
                            agent_id: Some(agent.agent_id.clone()),
                            label_set_id: agent.label_set_ids.get(i).cloned().unwrap_or_default(),
                        });
                    }
                }
                continue;
            }

            // base-label runners only fit what the agent's limits allow for them, e.g. a tart agent whose default
            // image is macOS can't run max_vms of them when a linux mapping raised max_vms
            *legacy.entry(normalize_labels(&agent.labels)).or_insert(0) +=
                agent.ceiling_for(&agent.labels) as u32;
        }

        legacy
            .into_iter()
            .map(|(labels, target_count)| PoolDefinition {
                labels,
                target_count,
                agent_id: None,
                label_set_id: String::new(),
            })
            .chain(explicit)
            .collect()
    }
}

/// Lowercased and sorted, so the same labels in any order make the same pool.
pub fn normalize_labels(labels: &[String]) -> Vec<String> {
    let mut normalized: Vec<String> = labels.iter().map(|l| l.to_lowercase()).collect();
    normalized.sort();
    normalized
}

/// Summary information about an agent (for listing)
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_id: String,
    pub agent_type: AgentType,
    pub hostname: String,
    pub labels: Vec<String>,
    pub label_sets: Vec<Vec<String>>,
    /// `label_sets` without the default set when there are others, see [`ConnectedAgent::display_label_sets`]
    pub display_label_sets: Vec<Vec<String>>,
    pub max_vms: usize,
    pub active_vms: usize,
    /// `active` is filled in for older agents too
    pub limits: Vec<VmLimit>,
    /// Returns true if the agent sets its own fixed-capacity pools per label set
    pub explicit_pools: bool,
    pub last_seen_secs: u64,
}

/// Fixed-capacity pool derived from connected agents.
///
/// An agent that sets `pool` on its mappings gets one pool per label set, owned by that agent. Other agents share a
/// legacy pool per base label combination, with a target count of the sum of their max_vms.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolDefinition {
    /// Labels that define this pool (sorted for consistent comparison)
    pub labels: Vec<String>,

    /// Runners to keep
    pub target_count: u32,

    /// The agent an explicit pool belongs to. None for legacy pools
    pub agent_id: Option<String>,

    /// The agent's id for the label set, sent back so it creates from exactly that mapping. Empty for legacy pools
    /// and older agents
    pub label_set_id: String,
}

impl PoolDefinition {
    /// Key stored on the runners of an explicit pool, to count them. Two sets can have the same labels, so it has the
    /// id too.
    pub fn key(&self) -> String {
        let labels = self.labels.join(",");
        if self.label_set_id.is_empty() {
            labels
        } else {
            format!("{labels}@{}", self.label_set_id)
        }
    }
}

impl AgentInfo {
    /// Check if agent is online (seen recently)
    pub fn is_online(&self) -> bool {
        self.last_seen_secs < 60 // Consider online if seen in last minute
    }

    /// Format status for display
    pub fn status_display(&self) -> String {
        if self.is_online() {
            "online".to_string()
        } else if self.last_seen_secs < 3600 {
            format!("offline ({}m)", self.last_seen_secs / 60)
        } else {
            format!("offline ({}h)", self.last_seen_secs / 3600)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_type_parsing() {
        assert_eq!("tart".parse::<AgentType>().unwrap(), AgentType::Tart);
        assert_eq!("Tart".parse::<AgentType>().unwrap(), AgentType::Tart);
        assert_eq!("PROXMOX".parse::<AgentType>().unwrap(), AgentType::Proxmox);
        assert!("unknown".parse::<AgentType>().is_err());
    }

    #[tokio::test]
    async fn test_agent_registry_basic() {
        let registry = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel(32);

        // Register an agent
        let _agent = registry
            .register(
                "agent_1".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                2,
                0, // active_vms
                vec!["macos".to_string(), "arm64".to_string()],
                vec![], // no label_sets - use legacy flat labels
                tx,
            )
            .await;

        assert_eq!(registry.count().await, 1);

        // Find by labels
        let found = registry.find_available_agent(&["macos".to_string()]).await;
        assert_eq!(found, Some("agent_1".to_string()));

        // Not found with wrong labels
        let not_found = registry
            .find_available_agent(&["windows".to_string()])
            .await;
        assert!(not_found.is_none());

        // Unregister
        registry.unregister("agent_1").await;
        assert_eq!(registry.count().await, 0);
    }

    #[tokio::test]
    async fn test_limits_with_external_vms_reduce_capacity() {
        let registry = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel(32);
        registry
            .register(
                "agent_1".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                2,
                0, // active_vms
                vec!["macos".to_string()],
                vec![],
                tx,
            )
            .await;
        let limit = |name: &str, max, external| VmLimit {
            name: name.to_string(),
            max,
            external,
            active: 0,
        };
        let limits = |limits| AgentCapacity {
            limits,
            ..Default::default()
        };
        let labels = ["macos".to_string()];

        // someone runs a mac VM by hand: one macOS slot left
        registry
            .set_capacity(
                "agent_1",
                limits(vec![limit("macos", 2, 1), limit("total", 5, 1)]),
            )
            .await;
        assert_eq!(registry.available_capacity(&labels).await, 1);

        // plus four linux VMs: total limit is full
        registry
            .set_capacity(
                "agent_1",
                limits(vec![limit("macos", 2, 1), limit("total", 5, 5)]),
            )
            .await;
        assert_eq!(registry.available_capacity(&labels).await, 0);
        assert!(registry.find_available_agent(&labels).await.is_none());

        // external VMs gone, back to max_vms
        registry
            .set_capacity(
                "agent_1",
                limits(vec![limit("macos", 2, 0), limit("total", 5, 0)]),
            )
            .await;
        assert_eq!(registry.available_capacity(&labels).await, 2);
    }

    #[tokio::test]
    async fn test_linux_jobs_skip_macos_limit() {
        let registry = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel(32);
        let strings = |labels: &[&str]| labels.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        registry
            .register(
                "agent_1".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                4,
                2, // active_vms: both macOS
                strings(&["self-hosted"]),
                vec![
                    strings(&["self-hosted", "macos"]),
                    strings(&["self-hosted", "linux"]),
                ],
                tx,
            )
            .await;
        let limit = |name: &str, max, active| VmLimit {
            name: name.to_string(),
            max,
            external: 0,
            active,
        };
        let set_limits = |macos_active, total_active| AgentCapacity {
            limits: vec![
                limit("macos", 2, macos_active),
                limit("total", 4, total_active),
            ],
            label_set_limits: vec![strings(&["macos", "total"]), strings(&["total"])],
            ..Default::default()
        };
        registry.set_capacity("agent_1", set_limits(2, 2)).await;
        let macos = strings(&["macos"]);
        let linux = strings(&["linux"]);

        // macOS limit is full, linux still has the rest of the total
        assert_eq!(registry.available_capacity(&macos).await, 0);
        assert_eq!(registry.available_capacity(&linux).await, 2);
        assert!(registry.find_available_agent(&macos).await.is_none());

        // labels matching both sets count against both
        assert_eq!(
            registry
                .available_capacity(&strings(&["self-hosted"]))
                .await,
            0
        );

        // a pending linux reservation only holds a slot on the limits linux VMs use
        registry
            .update_status(
                "agent_1",
                1,
                4,
                strings(&["self-hosted"]),
                vec![
                    strings(&["self-hosted", "macos"]),
                    strings(&["self-hosted", "linux"]),
                ],
            )
            .await;
        registry.set_capacity("agent_1", set_limits(1, 1)).await;
        assert!(registry.reserve_slot("agent_1", &linux, "", "r1").await);
        assert_eq!(registry.available_capacity(&macos).await, 1);
        assert_eq!(registry.available_capacity(&linux).await, 2);

        // it becomes a VM: the status lists it and counts it in total active
        registry.set_capacity("agent_1", set_limits(1, 2)).await;
        registry
            .settle_reservations("agent_1", &strings(&["r1"]))
            .await;
        registry
            .update_status(
                "agent_1",
                2,
                4,
                strings(&["self-hosted"]),
                vec![
                    strings(&["self-hosted", "macos"]),
                    strings(&["self-hosted", "linux"]),
                ],
            )
            .await;
        assert_eq!(registry.available_capacity(&macos).await, 1);
        assert_eq!(registry.available_capacity(&linux).await, 2);

        // a mac and a linux reservation, then the mac command is rejected: its macOS slot is free again right away
        assert!(registry.reserve_slot("agent_1", &macos, "", "r2").await);
        assert!(registry.reserve_slot("agent_1", &linux, "", "r3").await);
        assert_eq!(registry.available_capacity(&macos).await, 0);
        registry.release_slot("agent_1", "r2").await;
        assert_eq!(registry.available_capacity(&macos).await, 1);
        assert_eq!(registry.available_capacity(&linux).await, 1);
    }

    /// A tart agent with a linux base image and a macOS mapping, both pooled, with ids like new agents send.
    async fn linux_base_macos_mapping(external_macos: usize) -> AgentRegistry {
        let registry = AgentRegistry::new();
        let strings = |labels: &[&str]| labels.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let (tx, _rx) = mpsc::channel(32);
        registry
            .register(
                "agent_1".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                5,
                0,
                strings(&["self-hosted"]),
                vec![
                    strings(&["self-hosted", "macos"]),
                    strings(&["self-hosted"]),
                ],
                tx,
            )
            .await;
        let limit = |name: &str, max| VmLimit {
            name: name.to_string(),
            max,
            external: external_macos,
            active: 0,
        };
        registry
            .set_capacity(
                "agent_1",
                AgentCapacity {
                    limits: vec![limit("macos", 2), limit("total", 5)],
                    label_set_limits: vec![strings(&["macos", "total"]), strings(&["total"])],
                    label_set_ids: strings(&["sequoia", "noble"]),
                    default_set: Some(1),
                    ..Default::default()
                },
            )
            .await;
        registry
    }

    #[tokio::test]
    async fn test_base_label_jobs_use_the_base_image_limits() {
        let registry = linux_base_macos_mapping(0).await;
        let base = ["self-hosted".to_string()];

        // base-only runners use the linux base image, not the macOS mapping
        assert_eq!(registry.get_pool_definitions().await[0].target_count, 5);
        assert_eq!(registry.available_capacity(&base).await, 5);
        let mac = ["self-hosted".to_string(), "macos".to_string()];
        assert_eq!(registry.available_capacity(&mac).await, 2);
    }

    #[tokio::test]
    async fn test_default_set_is_the_flagged_one() {
        let registry = AgentRegistry::new();
        let strings = |labels: &[&str]| labels.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let (tx, _rx) = mpsc::channel(32);
        let base = strings(&["self-hosted"]);

        // a linux mapping with only base labels has the same labels as the macOS default
        registry
            .register(
                "agent_1".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                5,
                0,
                base.clone(),
                vec![base.clone(), base.clone()],
                tx,
            )
            .await;
        let limit = |name: &str, max| VmLimit {
            name: name.to_string(),
            max,
            external: 0,
            active: 0,
        };
        registry
            .set_capacity(
                "agent_1",
                AgentCapacity {
                    limits: vec![limit("macos", 2), limit("total", 5)],
                    label_set_limits: vec![strings(&["total"]), strings(&["macos", "total"])],
                    label_set_ids: strings(&["noble", "sequoia"]),
                    default_set: Some(1),
                    ..Default::default()
                },
            )
            .await;

        assert_eq!(registry.get_pool_definitions().await[0].target_count, 2);
        assert_eq!(registry.available_capacity(&base).await, 2);

        // the mapping is still reachable by its id
        assert_eq!(registry.agent_capacity("agent_1", &base, "noble").await, 5);
    }

    #[tokio::test]
    async fn test_display_label_sets_hides_default_only_with_mappings() {
        let registry = linux_base_macos_mapping(0).await;
        let agents = registry.list_all().await;
        assert_eq!(agents[0].label_sets.len(), 2);
        assert_eq!(
            agents[0].display_label_sets,
            vec![vec!["self-hosted".to_string(), "macos".to_string()]]
        );

        let registry = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel(32);
        let base = vec!["self-hosted".to_string()];
        registry
            .register(
                "agent_1".to_string(),
                AgentType::Proxmox,
                "pve".to_string(),
                3,
                0,
                base.clone(),
                vec![base.clone()],
                tx,
            )
            .await;
        registry
            .set_capacity(
                "agent_1",
                AgentCapacity {
                    default_set: Some(0),
                    ..Default::default()
                },
            )
            .await;
        assert_eq!(registry.list_all().await[0].display_label_sets, vec![base]);
    }

    #[tokio::test]
    async fn test_pool_set_id_picks_that_sets_limits() {
        // two external macOS VMs use up the macOS limit
        let registry = linux_base_macos_mapping(2).await;
        let base = ["self-hosted".to_string()];

        assert_eq!(registry.agent_capacity("agent_1", &base, "noble").await, 3);
        assert_eq!(
            registry.agent_capacity("agent_1", &base, "sequoia").await,
            0
        );
        assert!(
            !registry
                .reserve_slot("agent_1", &base, "sequoia", "r3")
                .await
        );
        assert!(registry.reserve_slot("agent_1", &base, "noble", "r4").await);
    }

    #[tokio::test]
    async fn test_agent_capacity() {
        let registry = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel(32);

        registry
            .register(
                "agent_1".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                2,
                0, // active_vms
                vec!["macos".to_string()],
                vec![], // no label_sets
                tx,
            )
            .await;

        // Initially has capacity of 2
        assert_eq!(registry.available_capacity(&["macos".to_string()]).await, 2);

        // Update to have 2 active VMs (no capacity)
        registry
            .update_status("agent_1", 2, 2, vec!["macos".to_string()], vec![])
            .await;
        assert_eq!(registry.available_capacity(&["macos".to_string()]).await, 0);

        // Should not find available agent now
        let found = registry.find_available_agent(&["macos".to_string()]).await;
        assert!(found.is_none());
    }

    /// Register a legacy macOS agent with `max_vms` slots and `active` VMs running.
    async fn mac_agent(max_vms: usize, active: usize) -> AgentRegistry {
        let registry = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel(32);
        registry
            .register(
                "agent_1".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                max_vms,
                active,
                vec!["macos".to_string()],
                vec![],
                tx,
            )
            .await;
        registry
    }

    /// A status from agent_1 with these VMs running.
    async fn mac_status(registry: &AgentRegistry, max_vms: usize, vms: &[&str]) {
        registry
            .update_status(
                "agent_1",
                vms.len(),
                max_vms,
                vec!["macos".to_string()],
                vec![],
            )
            .await;
        let names: Vec<String> = vms.iter().map(|v| v.to_string()).collect();
        registry.settle_reservations("agent_1", &names).await;
    }

    #[tokio::test]
    async fn test_reserved_slots_preserved_on_status_update() {
        let registry = mac_agent(2, 0).await;
        let macos = ["macos".to_string()];

        // Reserve both slots (simulating coordinator sending 2 CreateRunner commands)
        assert!(registry.reserve_slot("agent_1", &[], "", "r1").await);
        assert!(registry.reserve_slot("agent_1", &[], "", "r2").await);
        assert_eq!(registry.available_capacity(&macos).await, 0);

        // Status update arrives with no VMs yet: reservations stay
        mac_status(&registry, 2, &[]).await;
        assert_eq!(registry.available_capacity(&macos).await, 0);

        // r1's VM shows up: 1 active + 1 reserved
        mac_status(&registry, 2, &["r1"]).await;
        assert_eq!(registry.available_capacity(&macos).await, 0);

        // both VMs up, then both done
        mac_status(&registry, 2, &["r1", "r2"]).await;
        assert_eq!(registry.available_capacity(&macos).await, 0);
        mac_status(&registry, 2, &[]).await;
        assert_eq!(registry.available_capacity(&macos).await, 2);
    }

    #[tokio::test]
    async fn test_release_after_vm_started_keeps_other_reservations() {
        let registry = mac_agent(3, 0).await;
        let macos = ["macos".to_string()];

        assert!(registry.reserve_slot("agent_1", &[], "", "r1").await);
        mac_status(&registry, 3, &["r1"]).await;
        assert!(registry.reserve_slot("agent_1", &[], "", "r2").await);
        assert!(registry.reserve_slot("agent_1", &[], "", "r3").await);
        assert_eq!(registry.available_capacity(&macos).await, 0);

        // r1's job finishes: its VM is gone and the fleet releases it, which must not free r2's or r3's slot
        mac_status(&registry, 3, &[]).await;
        registry.release_slot("agent_1", "r1").await;
        assert_eq!(registry.available_capacity(&macos).await, 1);
    }

    #[tokio::test]
    async fn test_get_pool_definitions() {
        let registry = AgentRegistry::new();
        let (tx1, _rx1) = mpsc::channel(32);
        let (tx2, _rx2) = mpsc::channel(32);
        let (tx3, _rx3) = mpsc::channel(32);

        // Register 2 agents with same labels
        registry
            .register(
                "agent_1".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                2,
                0, // active_vms
                vec![
                    "self-hosted".to_string(),
                    "macos".to_string(),
                    "arm64".to_string(),
                ],
                vec![], // no label_sets
                tx1,
            )
            .await;

        registry
            .register(
                "agent_2".to_string(),
                AgentType::Tart,
                "mac-mini-2".to_string(),
                3,
                0, // active_vms
                vec![
                    "self-hosted".to_string(),
                    "macos".to_string(),
                    "arm64".to_string(),
                ],
                vec![], // no label_sets
                tx2,
            )
            .await;

        // Register 1 agent with different labels
        registry
            .register(
                "agent_3".to_string(),
                AgentType::Proxmox,
                "proxmox-1".to_string(),
                5,
                0, // active_vms
                vec![
                    "self-hosted".to_string(),
                    "linux".to_string(),
                    "x64".to_string(),
                ],
                vec![], // no label_sets
                tx3,
            )
            .await;

        let pools = registry.get_pool_definitions().await;

        // Should have 2 pools
        assert_eq!(pools.len(), 2);

        // Find macOS pool
        let macos_pool = pools
            .iter()
            .find(|p| p.labels.contains(&"macos".to_string()))
            .unwrap();
        assert_eq!(macos_pool.target_count, 5); // 2 + 3

        // Find linux pool
        let linux_pool = pools
            .iter()
            .find(|p| p.labels.contains(&"linux".to_string()))
            .unwrap();
        assert_eq!(linux_pool.target_count, 5);
    }

    #[tokio::test]
    async fn test_explicit_pools_per_label_set() {
        let registry = AgentRegistry::new();
        let strings = |labels: &[&str]| labels.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let (tx1, _rx1) = mpsc::channel(32);
        let (tx2, _rx2) = mpsc::channel(32);
        registry
            .register(
                "explicit".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                4,
                0,
                strings(&["self-hosted"]),
                vec![
                    strings(&["self-hosted", "macOS"]),
                    strings(&["self-hosted", "linux"]),
                    strings(&["self-hosted", "windows"]),
                ],
                tx1,
            )
            .await;
        registry
            .set_capacity(
                "explicit",
                AgentCapacity {
                    label_set_pools: vec![Some(1), Some(2), Some(0)],
                    label_set_ids: strings(&["sequoia", "noble", "win"]),
                    ..Default::default()
                },
            )
            .await;
        registry
            .register(
                "legacy".to_string(),
                AgentType::Tart,
                "mac-mini-2".to_string(),
                2,
                0,
                strings(&["self-hosted", "macos"]),
                vec![strings(&["self-hosted", "macos"])],
                tx2,
            )
            .await;

        let mut pools = registry.get_pool_definitions().await;
        pools.sort_by(|a, b| (&a.agent_id, &a.labels).cmp(&(&b.agent_id, &b.labels)));
        let explicit = |labels: &[&str], target_count, id: &str| PoolDefinition {
            labels: strings(labels),
            target_count,
            agent_id: Some("explicit".to_string()),
            label_set_id: id.to_string(),
        };

        // the size-0 set gets no pool, and the explicit agent isn't in the legacy pool
        assert_eq!(
            pools,
            vec![
                PoolDefinition {
                    labels: strings(&["macos", "self-hosted"]),
                    target_count: 2,
                    agent_id: None,
                    label_set_id: String::new(),
                },
                explicit(&["linux", "self-hosted"], 2, "noble"),
                explicit(&["macos", "self-hosted"], 1, "sequoia"),
            ]
        );
        assert_eq!(pools[0].key(), "macos,self-hosted");
        assert_eq!(pools[1].key(), "linux,self-hosted@noble");
    }

    #[tokio::test]
    async fn test_legacy_pool_skips_explicit_pool_agents() {
        let registry = AgentRegistry::new();
        let strings = |labels: &[&str]| labels.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let (tx1, _rx1) = mpsc::channel(32);
        let (tx2, _rx2) = mpsc::channel(32);
        let base = strings(&["self-hosted"]);

        // a roomy explicit-pool agent with the same base labels as a one-slot legacy agent
        registry
            .register(
                "explicit".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                4,
                0,
                base.clone(),
                vec![strings(&["self-hosted", "linux"]), base.clone()],
                tx1,
            )
            .await;
        registry
            .set_capacity(
                "explicit",
                AgentCapacity {
                    label_set_pools: vec![Some(1), Some(0)],
                    ..Default::default()
                },
            )
            .await;
        registry
            .register(
                "legacy".to_string(),
                AgentType::Tart,
                "mac-mini-2".to_string(),
                1,
                0,
                base.clone(),
                vec![base.clone()],
                tx2,
            )
            .await;

        assert_eq!(registry.legacy_pool_capacity(&base).await, 1);
        assert_eq!(
            registry.select_legacy_pool_agent(&base).await.as_deref(),
            Some("legacy")
        );
        registry.reserve_slot("legacy", &base, "", "r7").await;
        assert_eq!(registry.select_legacy_pool_agent(&base).await, None);
    }

    #[tokio::test]
    async fn test_legacy_pool_target_follows_base_image_limits() {
        let registry = AgentRegistry::new();
        let strings = |labels: &[&str]| labels.iter().map(|l| l.to_string()).collect::<Vec<_>>();
        let (tx, _rx) = mpsc::channel(32);
        let base = strings(&["self-hosted"]);

        // macOS base image plus a linux mapping: max_vms is the total, base-label runners are macOS
        registry
            .register(
                "agent_1".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                5,
                0,
                base.clone(),
                vec![strings(&["self-hosted", "linux"]), base.clone()],
                tx,
            )
            .await;
        let limit = |name: &str, max| VmLimit {
            name: name.to_string(),
            max,
            external: 0,
            active: 0,
        };
        registry
            .set_capacity(
                "agent_1",
                AgentCapacity {
                    limits: vec![limit("macos", 2), limit("total", 5)],
                    label_set_limits: vec![strings(&["total"]), strings(&["macos", "total"])],
                    ..Default::default()
                },
            )
            .await;

        let pools = registry.get_pool_definitions().await;
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].target_count, 2);
        assert_eq!(registry.legacy_pool_capacity(&base).await, 2);
    }

    #[tokio::test]
    async fn test_get_pool_definitions_empty() {
        let registry = AgentRegistry::new();
        let pools = registry.get_pool_definitions().await;
        assert_eq!(pools.len(), 0);
    }

    #[tokio::test]
    async fn test_reconcile_reservations() {
        let registry = mac_agent(3, 1).await;
        let macos = ["macos".to_string()];
        let names = |n: &[&str]| n.iter().map(|v| v.to_string()).collect::<Vec<_>>();

        assert!(registry.reserve_slot("agent_1", &[], "", "r1").await);
        assert!(registry.reserve_slot("agent_1", &[], "", "r2").await);
        assert_eq!(registry.available_capacity(&macos).await, 0);

        // too new to judge by the DB
        registry
            .reconcile_reservations("agent_1", &names(&["r1"]), Duration::from_secs(60))
            .await;
        assert_eq!(registry.available_capacity(&macos).await, 0);

        // r2's runner record is gone
        registry
            .reconcile_reservations("agent_1", &names(&["r1"]), Duration::ZERO)
            .await;
        assert_eq!(registry.available_capacity(&macos).await, 1);
    }

    #[tokio::test]
    async fn test_label_sets_matching() {
        let registry = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel(32);

        // Register agent with two label sets (capabilities):
        // - Can handle jobs requiring [self-hosted, macos, sequoia]
        // - Can handle jobs requiring [self-hosted, macos, ventura]
        registry
            .register(
                "agent_1".to_string(),
                AgentType::Tart,
                "mac-mini-1".to_string(),
                2,
                0,                                                    // active_vms
                vec!["self-hosted".to_string(), "macos".to_string()], // base labels
                vec![
                    vec![
                        "self-hosted".to_string(),
                        "macos".to_string(),
                        "sequoia".to_string(),
                    ],
                    vec![
                        "self-hosted".to_string(),
                        "macos".to_string(),
                        "ventura".to_string(),
                    ],
                ],
                tx,
            )
            .await;

        // Should match job requiring sequoia
        let found = registry
            .find_available_agent(&[
                "self-hosted".to_string(),
                "macOS".to_string(), // case-insensitive
                "sequoia".to_string(),
            ])
            .await;
        assert_eq!(found, Some("agent_1".to_string()));

        // Should match job requiring ventura
        let found = registry
            .find_available_agent(&[
                "self-hosted".to_string(),
                "macos".to_string(),
                "ventura".to_string(),
            ])
            .await;
        assert_eq!(found, Some("agent_1".to_string()));

        // Should NOT match job requiring both sequoia AND ventura (no single capability has both)
        let found = registry
            .find_available_agent(&[
                "self-hosted".to_string(),
                "macos".to_string(),
                "sequoia".to_string(),
                "ventura".to_string(),
            ])
            .await;
        assert!(found.is_none());

        // Should match job requiring only base labels
        let found = registry
            .find_available_agent(&["self-hosted".to_string(), "macos".to_string()])
            .await;
        assert_eq!(found, Some("agent_1".to_string()));
    }

    #[tokio::test]
    async fn test_select_agent_prefers_least_loaded() {
        let registry = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel(32);

        for (id, active) in [("busy", 1usize), ("idle", 0usize)] {
            registry
                .register(
                    id.to_string(),
                    AgentType::Tart,
                    format!("{id}.local"),
                    2,
                    active,
                    vec!["macos".to_string()],
                    vec![],
                    tx.clone(),
                )
                .await;
        }

        let labels = ["macos".to_string()];
        assert_eq!(
            registry.select_agent(&labels, &[]).await,
            Some("idle".to_string())
        );

        // a reservation counts as load too
        assert!(registry.reserve_slot("idle", &[], "", "r13").await);
        assert!(registry.reserve_slot("idle", &[], "", "r14").await);
        assert_eq!(
            registry.select_agent(&labels, &[]).await,
            Some("busy".to_string())
        );
    }

    #[tokio::test]
    async fn test_select_agent_avoids_failed_agent_when_possible() {
        let registry = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel(32);

        for id in ["a", "b"] {
            registry
                .register(
                    id.to_string(),
                    AgentType::Tart,
                    format!("{id}.local"),
                    2,
                    0,
                    vec!["macos".to_string()],
                    vec![],
                    tx.clone(),
                )
                .await;
        }

        let labels = ["macos".to_string()];
        // tie on load, ids break it: "a" wins unless avoided
        assert_eq!(
            registry.select_agent(&labels, &[]).await,
            Some("a".to_string())
        );
        assert_eq!(
            registry.select_agent(&labels, &["a".to_string()]).await,
            Some("b".to_string())
        );
        // everyone failed: still hand out someone rather than strand the job
        assert_eq!(
            registry
                .select_agent(&labels, &["a".to_string(), "b".to_string()])
                .await,
            Some("a".to_string())
        );
        // a full non-failed agent doesn't count as an alternative
        assert!(registry.reserve_slot("b", &[], "", "r15").await);
        assert!(registry.reserve_slot("b", &[], "", "r16").await);
        assert_eq!(
            registry.select_agent(&labels, &["a".to_string()]).await,
            Some("a".to_string())
        );
    }

    #[tokio::test]
    async fn test_disconnect_stamp_is_replaced_by_newer_drop() {
        let registry = AgentRegistry::new();
        let (tx, _rx) = mpsc::channel(32);
        let reg = || {
            registry.register(
                "a".to_string(),
                AgentType::Tart,
                "a.local".to_string(),
                1,
                0,
                vec![],
                vec![],
                tx.clone(),
            )
        };

        reg().await;
        let first = registry.unregister("a").await;

        // came back and dropped again: the first timer's claim is stale, the second's wins once
        reg().await;
        assert!(!registry.claim_failover("a", first).await);
        let second = registry.unregister("a").await;
        assert!(second > first);
        assert!(!registry.claim_failover("a", first).await);
        assert!(registry.claim_failover("a", second).await);
        assert!(!registry.claim_failover("a", second).await);

        // reconnect clears the stamp, so a timer from before can't claim after registration
        reg().await;
        let third = registry.unregister("a").await;
        reg().await;
        assert!(!registry.claim_failover("a", third).await);
    }
}
