//! Askama templates for the admin UI.

use crate::agent_updates::UpdateAvailable;
use crate::forge::JobKey;
use askama::Template;
use chrono::{DateTime, Utc};

/// Base data available to all templates
pub struct BaseContext {
    pub username: String,
}

/// Login page template
#[derive(Template)]
#[template(path = "admin/login.html")]
pub struct LoginTemplate {
    pub error: Option<String>,
}

/// Registration token summary
pub struct TokenSummary {
    pub token: String,
    pub expires_at: DateTime<Utc>,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
}

/// Dashboard page template
#[derive(Template)]
#[template(path = "admin/dashboard.html")]
pub struct DashboardTemplate {
    pub base: BaseContext,
    pub connected_agents: usize,
    pub active_runners: usize,
    pub pending_jobs: Vec<PendingJobSummary>,
    pub capacity: CapacitySummary,
    pub agents: Vec<AgentSummary>,
    pub tokens: Vec<TokenSummary>,
    pub new_token: Option<String>,
}

/// Webhook job still waiting for a runner
pub struct PendingJobSummary {
    pub job_id: JobKey,
    /// Agent running a runner for this job. None while it's still waiting for one
    pub assigned_agent: Option<String>,
    pub repository: Option<String>,
    pub workflow_name: Option<String>,
    pub job_name: Option<String>,
    pub labels: Vec<String>,
    /// How long ago the webhook arrived, e.g. `4m 12s`
    pub waiting: String,
    pub retry_count: i32,
    pub failed_agents: usize,
    /// Connected agents whose labels match, regardless of free capacity
    pub matching_agents: usize,
    /// Of `matching_agents`, the ones marked unavailable
    pub unavailable_agents: usize,
    pub free_capacity: usize,
}

/// Agent summary for list view
pub struct AgentSummary {
    pub agent_id: String,
    pub hostname: String,
    pub agent_type: String,
    /// Labels every runner of this agent has
    pub base_labels: Vec<String>,
    /// What each mapping adds on top of `base_labels`
    pub mapping_labels: Vec<Vec<String>>,
    pub max_vms: u32,
    /// Only known while the agent is connected
    pub limits: Vec<LimitView>,
    pub is_online: bool,
    pub active_vms: usize,
    pub created_at: DateTime<Utc>,
    pub revoked: bool,
    /// When an admin marked it unavailable (no new runners). None while it takes work
    pub unavailable_at: Option<DateTime<Utc>>,
    /// None for agents that predate version reporting
    pub version: Option<String>,
    /// Set when a newer release of this agent is out
    pub update: Option<UpdateAvailable>,
}

/// Limit a tart agent reports for VMs of any OS. It's the agent's real cap: its `max_vms` only covers its biggest
/// label set, e.g. just macOS on a macOS-only agent
const LIMIT_TOTAL: &str = "total";

impl AgentSummary {
    fn total_limit(&self) -> Option<&LimitView> {
        self.limits.iter().find(|l| l.name == LIMIT_TOTAL)
    }

    /// VMs in use, external ones included when the agent reports them
    pub fn capacity_used(&self) -> usize {
        self.total_limit().map_or(self.active_vms, |l| l.used)
    }

    pub fn capacity_max(&self) -> usize {
        self.total_limit().map_or(self.max_vms as usize, |l| l.max)
    }

    /// Running VMs the agent doesn't manage, counted in `capacity_used`
    pub fn capacity_external(&self) -> usize {
        self.total_limit().map_or(0, |l| l.external)
    }

    /// Limits shown under the total, like macOS's 2 VMs per host
    pub fn other_limits(&self) -> Vec<&LimitView> {
        self.limits
            .iter()
            .filter(|l| l.name != LIMIT_TOTAL)
            .collect()
    }
}

/// VM capacity across connected agents, for the dashboard
pub struct CapacitySummary {
    pub used: usize,
    pub total: usize,
    /// Of `used`, VMs the agents don't manage
    pub external: usize,
    /// Host-wide limits summed by name over the agents reporting them, e.g. macOS's 2 VMs per host
    pub limits: Vec<LimitView>,
}

impl CapacitySummary {
    pub fn from_agents(agents: &[AgentSummary]) -> Self {
        let mut summary = Self {
            used: 0,
            total: 0,
            external: 0,
            limits: Vec::new(),
        };
        for agent in agents.iter().filter(|a| a.is_online && !a.revoked) {
            summary.used += agent.capacity_used();
            summary.total += agent.capacity_max();
            summary.external += agent.capacity_external();
            for limit in agent.other_limits() {
                match summary.limits.iter_mut().find(|l| l.name == limit.name) {
                    Some(total) => {
                        total.used += limit.used;
                        total.max += limit.max;
                        total.external += limit.external;
                    }
                    None => summary.limits.push(LimitView {
                        name: limit.name.clone(),
                        used: limit.used,
                        max: limit.max,
                        external: limit.external,
                    }),
                }
            }
        }
        summary
    }
}

/// Usage of one of an agent's host-wide limits
pub struct LimitView {
    pub name: String,
    /// The agent's own VMs plus external ones
    pub used: usize,
    pub max: usize,
    pub external: usize,
}

impl LimitView {
    /// Name for display, agents report it lowercase
    pub fn label(&self) -> &str {
        if self.name.eq_ignore_ascii_case("macos") {
            "macOS"
        } else {
            &self.name
        }
    }

    pub fn from_agent(agent: &crate::agent_registry::AgentInfo) -> Vec<Self> {
        agent
            .limits
            .iter()
            .map(|l| Self {
                name: l.name.clone(),
                used: l.active + l.external,
                max: l.max,
                external: l.external,
            })
            .collect()
    }
}

/// Split an agent's label sets into its base labels and what each mapping adds, so the base isn't repeated per set.
pub fn split_labels(agent: &crate::agent_registry::AgentInfo) -> (Vec<String>, Vec<Vec<String>>) {
    let is_base = |l: &String| agent.labels.iter().any(|b| b.eq_ignore_ascii_case(l));
    let mappings = agent
        .display_label_sets
        .iter()
        .map(|set| {
            set.iter()
                .filter(|l| !is_base(l))
                .cloned()
                .collect::<Vec<_>>()
        })
        .filter(|extra| !extra.is_empty())
        .collect();
    (agent.labels.clone(), mappings)
}

/// Agent detail page template
#[derive(Template)]
#[template(path = "admin/agent_detail.html")]
pub struct AgentDetailTemplate {
    pub base: BaseContext,
    pub agent: AgentSummary,
    pub runners: Vec<RunnerSummary>,
    pub failures: Vec<FailureSummary>,
    /// Last week's runner stats, None when it ran nothing
    pub performance: Option<AgentPerformance>,
    /// Active tab for the shared agent header
    pub tab: &'static str,
}

/// Just what the shared agent header needs
pub struct AgentHeader {
    pub agent_id: String,
    pub hostname: String,
    pub revoked: bool,
    pub unavailable_at: Option<DateTime<Utc>>,
}

/// Agent logs tab
#[derive(Template)]
#[template(path = "admin/agent_logs.html")]
pub struct AgentLogsTemplate {
    pub base: BaseContext,
    pub agent: AgentHeader,
    pub tab: &'static str,
    /// Oldest first, so the newest line sits at the bottom like a terminal
    pub lines: Vec<LogLineView>,
    /// Selected minimum level, lowercase
    pub level: &'static str,
    /// Cursor for the next "older" page, None when there's nothing older
    pub older: Option<String>,
    /// True when looking at an older page rather than the newest lines
    pub paged: bool,
    pub live: bool,
}

pub struct LogLineView {
    pub ts: DateTime<Utc>,
    pub level: &'static str,
    pub target: String,
    pub message: String,
}

/// Recorded agent failure for the agent detail page
pub struct FailureSummary {
    pub occurred_at: DateTime<Utc>,
    /// e.g. `4m 12s`
    pub ago: String,
    pub kind: &'static str,
    pub runner_name: Option<String>,
    pub job_id: Option<JobKey>,
    pub message: String,
}

/// Admin user row for the users page
pub struct UserSummary {
    pub username: String,
    pub created_at: DateTime<Utc>,
    pub last_login: Option<DateTime<Utc>>,
    pub is_self: bool,
}

/// Admin users page template
#[derive(Template)]
#[template(path = "admin/users.html")]
pub struct UsersTemplate {
    pub base: BaseContext,
    pub users: Vec<UserSummary>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

/// API token row for the API tokens page
pub struct ApiTokenSummary {
    pub id: String,
    pub name: String,
    pub token_prefix: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// API tokens page template
#[derive(Template)]
#[template(path = "admin/api_tokens.html")]
pub struct ApiTokensTemplate {
    pub base: BaseContext,
    pub tokens: Vec<ApiTokenSummary>,
    /// Plaintext of a just-created token, shown once
    pub new_token: Option<String>,
    pub error: Option<String>,
}

/// Runner summary
pub struct RunnerSummary {
    pub runner_name: String,
    pub vm_name: String,
    pub created_at: DateTime<Utc>,
    pub job_id: Option<JobKey>,
    pub job_name: Option<String>,
    pub repository: Option<String>,
    pub workflow_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_registry::{AgentInfo, AgentType};

    fn strings(labels: &[&str]) -> Vec<String> {
        labels.iter().map(|l| l.to_string()).collect()
    }

    fn agent(base: &[&str], sets: &[&[&str]]) -> AgentInfo {
        AgentInfo {
            agent_id: "agent_1".to_string(),
            agent_type: AgentType::PROXMOX,
            hostname: "pve".to_string(),
            labels: strings(base),
            label_sets: vec![],
            display_label_sets: sets.iter().map(|s| strings(s)).collect(),
            max_vms: 3,
            active_vms: 0,
            limits: vec![],
            explicit_pools: false,
            last_seen_secs: 0,
            unavailable: false,
        }
    }

    #[test]
    fn split_labels_keeps_only_what_mappings_add() {
        let agent = agent(
            &["Self-Hosted"],
            &[
                &["self-hosted", "windows"],
                &["self-hosted", "linux", "ubuntu-24.04"],
            ],
        );
        assert_eq!(
            split_labels(&agent),
            (
                strings(&["Self-Hosted"]),
                vec![strings(&["windows"]), strings(&["linux", "ubuntu-24.04"])]
            )
        );
    }

    #[test]
    fn split_labels_without_mappings_is_just_the_base() {
        let agent = agent(&["self-hosted", "macos"], &[&["self-hosted", "macos"]]);
        assert_eq!(
            split_labels(&agent),
            (strings(&["self-hosted", "macos"]), vec![])
        );
    }

    fn summary(
        online: bool,
        active_vms: usize,
        max_vms: u32,
        limits: Vec<LimitView>,
    ) -> AgentSummary {
        AgentSummary {
            agent_id: "agent".to_string(),
            hostname: "host".to_string(),
            agent_type: "tart".to_string(),
            base_labels: vec![],
            mapping_labels: vec![],
            max_vms,
            limits,
            is_online: online,
            active_vms,
            created_at: Utc::now(),
            revoked: false,
            unavailable_at: None,
            version: None,
            update: None,
        }
    }

    fn limit(name: &str, used: usize, max: usize, external: usize) -> LimitView {
        LimitView {
            name: name.to_string(),
            used,
            max,
            external,
        }
    }

    #[test]
    fn capacity_sums_connected_agents_and_their_limits() {
        let agents = [
            // tart: max_vms is only the macOS cap, the total limit is what the host runs
            summary(
                true,
                1,
                2,
                vec![limit("macos", 1, 2, 0), limit("total", 2, 5, 0)],
            ),
            summary(
                true,
                0,
                2,
                vec![limit("macos", 1, 2, 1), limit("total", 1, 5, 1)],
            ),
            // proxmox reports no limits, max_vms is its total
            summary(true, 2, 4, vec![]),
            // offline agents have no capacity to give
            summary(
                false,
                0,
                2,
                vec![limit("macos", 0, 2, 0), limit("total", 0, 5, 0)],
            ),
        ];
        assert_eq!(
            (agents[0].capacity_used(), agents[0].capacity_max()),
            (2, 5)
        );
        assert_eq!(
            (agents[2].capacity_used(), agents[2].capacity_max()),
            (2, 4)
        );

        let capacity = CapacitySummary::from_agents(&agents);
        assert_eq!(
            (capacity.used, capacity.total, capacity.external),
            (5, 14, 1)
        );
        // total is the headline, not repeated as a limit
        assert_eq!(capacity.limits.len(), 1);
        let limit = &capacity.limits[0];
        assert_eq!(
            (limit.label(), limit.used, limit.max, limit.external),
            ("macOS", 2, 4, 1)
        );
    }
}

/// Metrics page
#[derive(Template)]
#[template(path = "admin/metrics.html")]
pub struct MetricsTemplate {
    pub base: BaseContext,
    /// Selected range, e.g. `7d`
    pub range: &'static str,
    pub ranges: Vec<&'static str>,
    pub runs: usize,
    pub running: usize,
    pub failures: usize,
    pub success: String,
    pub provision_p95: String,
    pub wait_p50: String,
    /// Step columns, in the order every group's `steps` follows
    pub steps: Vec<StepColumn>,
    pub agents: Vec<RunGroupView>,
    pub images: Vec<RunGroupView>,
    pub installs: Vec<InstallView>,
    pub jobs: JobSummaryView,
    /// Chart series, JSON with `<` escaped so it can sit in a script tag
    pub chart_json: String,
}

/// One agent step's column header
pub struct StepColumn {
    pub name: String,
    /// What the step covers, for the header's tooltip
    pub title: &'static str,
}

/// One agent step for a group, formatted
pub struct StepView {
    pub stat: StatView,
    /// e.g. `75% skipped`, empty when nothing skipped it
    pub skipped: String,
}

/// p50 and p95 of one duration, formatted
pub struct StatView {
    pub p50: String,
    pub p95: String,
    pub count: usize,
}

/// Runner stats for one agent or image
pub struct RunGroupView {
    /// Agent page to link to, None for images
    pub link: Option<String>,
    pub label: String,
    pub detail: String,
    pub runs: usize,
    pub running: usize,
    pub completed: usize,
    pub failures: usize,
    pub rejected: usize,
    pub stopped: usize,
    /// e.g. `97.5%`, `-` before anything finished
    pub success: String,
    /// `ok`/`warn`/`bad`/`idle` for the status dot
    pub success_status: &'static str,
    /// e.g. `boot 3, setup 1`
    pub failed_phases: String,
    pub ack: StatView,
    pub provision: StatView,
    /// Same order as the page's step columns
    pub steps: Vec<StepView>,
}

/// One image's runner installs from one source
pub struct InstallView {
    pub image: String,
    pub source: String,
    /// What the source means, for its tooltip
    pub source_title: &'static str,
    pub runs: usize,
    pub install: StatView,
}

/// Where queued jobs spent their time
pub struct JobSummaryView {
    pub jobs: usize,
    pub pending: usize,
    pub ran: usize,
    pub abandoned: usize,
    pub gone: usize,
    pub cancelled: usize,
    pub mean_attempts: String,
    pub wait: StatView,
    pub to_coordinator: StatView,
    pub dispatch: StatView,
    pub provision: StatView,
    pub pickup: StatView,
}

/// The agent page's last-week numbers
pub struct AgentPerformance {
    pub runs: usize,
    pub failures: usize,
    pub success: String,
    pub success_status: &'static str,
    pub failed_phases: String,
    pub provision: StatView,
    pub boot: StepView,
    pub install: StepView,
    pub job: StepView,
}
