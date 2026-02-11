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

    /// Reserved slots (commands sent but not yet reflected in active_vms)
    /// This prevents over-scheduling when sending multiple commands quickly
    reserved_slots: usize,

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
            reserved_slots: 0,
            labels,
            label_sets,
            command_tx,
            pending_commands: RwLock::new(HashMap::new()),
            last_seen: std::time::Instant::now(),
        }
    }

    /// Check if this agent has capacity for more VMs
    /// Takes into account both active VMs and reserved slots
    pub fn has_capacity(&self) -> bool {
        self.active_vms + self.reserved_slots < self.max_vms
    }

    /// Get available capacity (number of VMs that can still be created)
    /// Takes into account both active VMs and reserved slots
    pub fn available_capacity(&self) -> usize {
        self.max_vms
            .saturating_sub(self.active_vms + self.reserved_slots)
    }

    /// Reserve a slot for an upcoming VM creation
    /// Returns true if reservation succeeded, false if no capacity
    pub fn reserve_slot(&mut self) -> bool {
        if self.has_capacity() {
            self.reserved_slots += 1;
            true
        } else {
            false
        }
    }

    /// Release a reserved slot (call when command completes or fails)
    pub fn release_slot(&mut self) {
        self.reserved_slots = self.reserved_slots.saturating_sub(1);
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
            self.label_sets.iter().any(|label_set| {
                required_labels.iter().all(|required| {
                    label_set
                        .iter()
                        .any(|label| label.eq_ignore_ascii_case(required))
                })
            })
        } else {
            // Legacy flat labels matching
            required_labels.iter().all(|required| {
                self.labels
                    .iter()
                    .any(|label| label.eq_ignore_ascii_case(required))
            })
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

        let mut agents = self.agents.write().await;
        agents.insert(agent_id.clone(), Arc::clone(&agent));

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
    pub async fn unregister(&self, agent_id: &str) {
        let agent = {
            let mut agents = self.agents.write().await;
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
    }

    /// Get an agent by ID
    pub async fn get(&self, agent_id: &str) -> Option<Arc<RwLock<ConnectedAgent>>> {
        let agents = self.agents.read().await;
        agents.get(agent_id).cloned()
    }

    /// Find an agent with capacity matching the required labels
    pub async fn find_available_agent(&self, labels: &[String]) -> Option<String> {
        let agents = self.agents.read().await;
        for (id, agent) in agents.iter() {
            let agent = agent.read().await;
            let has_cap = agent.has_capacity();
            let matches = agent.matches_labels(labels);
            debug!(
                "Checking agent {}: has_capacity={} (active={}, reserved={}, max={}), matches_labels={:?}={}",
                id, has_cap, agent.active_vms, agent.reserved_slots, agent.max_vms, labels, matches
            );
            if has_cap && matches {
                return Some(id.clone());
            }
        }
        None
    }

    /// Find agents matching labels (may or may not have capacity)
    /// Useful for diagnostics and admin queries.
    #[allow(dead_code)] // Available for future admin API
    pub async fn find_agents_by_labels(&self, labels: &[String]) -> Vec<String> {
        let agents = self.agents.read().await;
        agents
            .iter()
            .filter_map(|(id, agent)| {
                // We need to check labels without await here
                // So we'll use try_read for a quick check
                if let Ok(agent) = agent.try_read()
                    && agent.matches_labels(labels)
                {
                    return Some(id.clone());
                }
                None
            })
            .collect()
    }

    /// Get total available capacity for agents matching labels
    pub async fn available_capacity(&self, labels: &[String]) -> usize {
        let agents = self.agents.read().await;
        let mut total = 0;
        for agent in agents.values() {
            let agent = agent.read().await;
            if agent.matches_labels(labels) {
                total += agent.available_capacity();
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

    /// Update agent status (active VMs, etc.)
    pub async fn update_status(&self, agent_id: &str, active_vms: usize) {
        if let Some(agent) = self.get(agent_id).await {
            let mut agent = agent.write().await;
            let old_active = agent.active_vms;
            agent.active_vms = active_vms;

            // Only reduce reserved_slots when active_vms increases (VM creation completed).
            // Don't clear all reserved_slots on every status update - the status might arrive
            // before the VM is created, which would cause us to over-schedule.
            if active_vms > old_active {
                let newly_active = active_vms - old_active;
                agent.reserved_slots = agent.reserved_slots.saturating_sub(newly_active);
            }

            agent.touch();
            debug!(
                agent_id = %agent_id,
                active_vms = active_vms,
                reserved_slots = agent.reserved_slots,
                "Agent status updated"
            );
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
    pub async fn reserve_slot(&self, agent_id: &str) -> bool {
        match self.get(agent_id).await {
            Some(agent) => {
                let mut agent = agent.write().await;
                let reserved = agent.reserve_slot();
                if reserved {
                    debug!(
                        agent_id = %agent_id,
                        reserved_slots = agent.reserved_slots,
                        "Slot reserved"
                    );
                }
                reserved
            }
            _ => false,
        }
    }

    /// Release a reserved slot on an agent
    pub async fn release_slot(&self, agent_id: &str) {
        if let Some(agent) = self.get(agent_id).await {
            let mut agent = agent.write().await;
            agent.release_slot();
            debug!(
                agent_id = %agent_id,
                reserved_slots = agent.reserved_slots,
                "Slot released"
            );
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
                max_vms: agent.max_vms,
                active_vms: agent.active_vms,
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
        let mut pools: HashMap<Vec<String>, u32> = HashMap::new();

        for agent in agents.values() {
            let agent = agent.read().await;

            // Normalize labels: sort and lowercase for consistent grouping
            let mut normalized_labels: Vec<String> =
                agent.labels.iter().map(|l| l.to_lowercase()).collect();
            normalized_labels.sort();

            // Add this agent's capacity to the pool for this label combination
            *pools.entry(normalized_labels).or_insert(0) += agent.max_vms as u32;
        }

        // Convert to PoolDefinition vec
        pools
            .into_iter()
            .map(|(labels, target_count)| PoolDefinition {
                labels,
                target_count,
            })
            .collect()
    }
}

/// Summary information about an agent (for listing)
#[derive(Debug, Clone)]
pub struct AgentInfo {
    pub agent_id: String,
    pub agent_type: AgentType,
    pub hostname: String,
    pub labels: Vec<String>,
    pub max_vms: usize,
    pub active_vms: usize,
    pub last_seen_secs: u64,
}

/// Pool definition derived from connected agents.
///
/// In the new agent-driven mode, each unique label combination
/// forms a separate pool with a target count equal to the sum
/// of max_vms across all agents with those labels.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PoolDefinition {
    /// Labels that define this pool (sorted for consistent comparison)
    pub labels: Vec<String>,

    /// Target count: sum of max_vms across all agents with these labels
    pub target_count: u32,
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
        registry.update_status("agent_1", 2).await;
        assert_eq!(registry.available_capacity(&["macos".to_string()]).await, 0);

        // Should not find available agent now
        let found = registry.find_available_agent(&["macos".to_string()]).await;
        assert!(found.is_none());
    }

    #[tokio::test]
    async fn test_reserved_slots_preserved_on_status_update() {
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

        // Reserve both slots (simulating coordinator sending 2 CreateRunner commands)
        assert!(registry.reserve_slot("agent_1").await);
        assert!(registry.reserve_slot("agent_1").await);

        // Now at capacity (0 active, 2 reserved)
        assert_eq!(registry.available_capacity(&["macos".to_string()]).await, 0);

        // Status update arrives with active_vms=0 (VMs not created yet)
        // This should NOT clear reserved_slots
        registry.update_status("agent_1", 0).await;

        // Should still be at capacity
        assert_eq!(registry.available_capacity(&["macos".to_string()]).await, 0);

        // Now VM is created - status update with active_vms=1
        // Should reduce reserved_slots by 1
        registry.update_status("agent_1", 1).await;
        assert_eq!(registry.available_capacity(&["macos".to_string()]).await, 0); // 1 active + 1 reserved = 2

        // Second VM created
        registry.update_status("agent_1", 2).await;
        assert_eq!(registry.available_capacity(&["macos".to_string()]).await, 0); // 2 active + 0 reserved

        // VM destroyed
        registry.update_status("agent_1", 1).await;
        assert_eq!(registry.available_capacity(&["macos".to_string()]).await, 1); // 1 active
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
    async fn test_get_pool_definitions_empty() {
        let registry = AgentRegistry::new();
        let pools = registry.get_pool_definitions().await;
        assert_eq!(pools.len(), 0);
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
}
