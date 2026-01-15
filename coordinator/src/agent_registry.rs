//! Agent registry for tracking connected agents.
//!
//! Maintains a registry of all connected agents (both Tart and Proxmox),
//! provides label-based agent matching, and handles command routing.

// Allow dead code for fields/methods that may be useful for future features

use agent_proto::{AgentMessage, CoordinatorMessage};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, RwLock};
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
            _ => Err(anyhow!("Unknown agent type: {}", s)),
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

    /// Currently active VMs
    pub active_vms: usize,

    /// Labels this agent supports (e.g., ["macos", "arm64"])
    pub labels: Vec<String>,

    /// Channel to send commands to this agent
    pub command_tx: mpsc::Sender<CoordinatorMessage>,

    /// Pending commands waiting for responses
    pending_commands: RwLock<HashMap<String, oneshot::Sender<AgentMessage>>>,

    /// Last time we heard from this agent
    pub last_seen: std::time::Instant,
}

impl ConnectedAgent {
    /// Create a new connected agent
    pub fn new(
        agent_id: String,
        agent_type: AgentType,
        hostname: String,
        max_vms: usize,
        labels: Vec<String>,
        command_tx: mpsc::Sender<CoordinatorMessage>,
    ) -> Self {
        Self {
            agent_id,
            agent_type,
            hostname,
            max_vms,
            active_vms: 0,
            labels,
            command_tx,
            pending_commands: RwLock::new(HashMap::new()),
            last_seen: std::time::Instant::now(),
        }
    }

    /// Check if this agent has capacity for more VMs
    pub fn has_capacity(&self) -> bool {
        self.active_vms < self.max_vms
    }

    /// Get available capacity (number of VMs that can still be created)
    pub fn available_capacity(&self) -> usize {
        self.max_vms.saturating_sub(self.active_vms)
    }

    /// Check if this agent matches all required labels
    pub fn matches_labels(&self, required_labels: &[String]) -> bool {
        required_labels.iter().all(|l| self.labels.contains(l))
    }

    /// Register a pending command and return the response channel
    pub async fn register_pending_command(&self, command_id: &str) -> oneshot::Receiver<AgentMessage> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending_commands.write().await;
        pending.insert(command_id.to_string(), tx);
        rx
    }

    /// Complete a pending command with a response
    pub async fn complete_command(&self, command_id: &str, response: AgentMessage) -> bool {
        let mut pending = self.pending_commands.write().await;
        if let Some(tx) = pending.remove(command_id) {
            tx.send(response).is_ok()
        } else {
            false
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
    pub async fn register(
        &self,
        agent_id: String,
        agent_type: AgentType,
        hostname: String,
        max_vms: usize,
        labels: Vec<String>,
        command_tx: mpsc::Sender<CoordinatorMessage>,
    ) -> Arc<RwLock<ConnectedAgent>> {
        let agent = Arc::new(RwLock::new(ConnectedAgent::new(
            agent_id.clone(),
            agent_type,
            hostname.clone(),
            max_vms,
            labels.clone(),
            command_tx,
        )));

        let mut agents = self.agents.write().await;
        agents.insert(agent_id.clone(), Arc::clone(&agent));

        info!(
            agent_id = %agent_id,
            agent_type = %agent_type,
            hostname = %hostname,
            labels = ?labels,
            max_vms = max_vms,
            "Agent registered"
        );

        agent
    }

    /// Unregister an agent (on disconnect)
    pub async fn unregister(&self, agent_id: &str) {
        let mut agents = self.agents.write().await;
        if agents.remove(agent_id).is_some() {
            info!(agent_id = %agent_id, "Agent unregistered");
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
            if agent.has_capacity() && agent.matches_labels(labels) {
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
                if let Ok(agent) = agent.try_read() {
                    if agent.matches_labels(labels) {
                        return Some(id.clone());
                    }
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
            .ok_or_else(|| anyhow!("Agent not found: {}", agent_id))?;

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
            agent.active_vms = active_vms;
            agent.touch();
            debug!(
                agent_id = %agent_id,
                active_vms = active_vms,
                "Agent status updated"
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
                vec!["macos".to_string(), "arm64".to_string()],
                tx,
            )
            .await;

        assert_eq!(registry.count().await, 1);

        // Find by labels
        let found = registry
            .find_available_agent(&["macos".to_string()])
            .await;
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
                vec!["macos".to_string()],
                tx,
            )
            .await;

        // Initially has capacity of 2
        assert_eq!(
            registry.available_capacity(&["macos".to_string()]).await,
            2
        );

        // Update to have 2 active VMs (no capacity)
        registry.update_status("agent_1", 2).await;
        assert_eq!(
            registry.available_capacity(&["macos".to_string()]).await,
            0
        );

        // Should not find available agent now
        let found = registry
            .find_available_agent(&["macos".to_string()])
            .await;
        assert!(found.is_none());
    }
}
