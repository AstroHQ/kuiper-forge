use reqwest::{Client, IntoUrl, Method, RequestBuilder, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Debug;
use std::process::Command;
use std::time::Duration;
use tracing::{debug, error, info};

#[derive(Debug, Deserialize)]
struct VmResponse {
    data: Vec<VirtualMachine>,
}

#[derive(Debug, Deserialize)]
struct SnapshotListResponse {
    data: Vec<Snapshot>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct VirtualMachine {
    pub diskread: usize,
    pub node: String,
    pub id: String,
    pub template: usize,
    pub maxcpu: u32,
    pub netout: usize,
    pub maxdisk: usize,
    pub maxmem: usize,
    #[serde(rename = "type")]
    pub vm_type: String,
    pub mem: usize,
    pub uptime: usize,
    pub name: String,
    pub vmid: usize,
    pub netin: usize,
    pub disk: usize,
    pub cpu: f64,
    pub status: String,
    pub tags: Option<String>,
    pub diskwrite: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub running: Option<u32>,
    pub name: String,
    pub digest: Option<String>,
    pub description: String,
    pub parent: Option<String>,
    pub snaptime: Option<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Error within reqwest library
    #[error("Proxmox API request failed: {0}")]
    ReqwestError(#[from] reqwest::Error),

    /// Error returned by server
    #[error("HTTP error from API: {0}")]
    HttpError(StatusCode),

    /// Task failed
    #[error("Task failed with status: {0}")]
    TaskFailed(String),

    /// Unknown task status
    #[error("Unknown task status: {0}")]
    UnknownTaskStatus(String),

    /// Timeout waiting for condition
    #[error("Timeout: {0}")]
    Timeout(String),

    /// Guest agent not available
    #[error("Guest agent not available")]
    GuestAgentUnavailable,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub enum ProxmoxAuth {
    Password(String),
    ApiToken { id: String, token: String },
}
impl ProxmoxAuth {
    pub fn password<S: ToString>(password: S) -> Self {
        Self::Password(password.to_string())
    }
    pub fn api_token<I: ToString, S: ToString>(id: I, token: S) -> Self {
        Self::ApiToken {
            id: id.to_string(),
            token: token.to_string(),
        }
    }
}

pub struct ProxmoxVEAPI {
    client: Client,
    username: String,
    authentication: ProxmoxAuth,
    base_url: Url,
    node: String,
}
impl ProxmoxVEAPI {
    pub fn new<U: ToString, T: IntoUrl>(
        username: U,
        authentication: ProxmoxAuth,
        base_url: T,
        allow_invalid_tls: bool,
    ) -> Result<Self> {
        let client = Client::builder()
            .danger_accept_invalid_certs(allow_invalid_tls)
            .build()?;
        Ok(Self {
            client,
            username: username.to_string(),
            authentication,
            base_url: base_url.into_url()?.join("api2/json/").unwrap(),
            node: "pve".to_string(), // Default node name
        })
    }

    /// Create a new client with a specified node name.
    pub fn with_node<U: ToString, T: IntoUrl, N: ToString>(
        username: U,
        authentication: ProxmoxAuth,
        base_url: T,
        node: N,
        allow_invalid_tls: bool,
    ) -> Result<Self> {
        let client = Client::builder()
            .danger_accept_invalid_certs(allow_invalid_tls)
            .build()?;
        Ok(Self {
            client,
            username: username.to_string(),
            authentication,
            base_url: base_url.into_url()?.join("api2/json/").unwrap(),
            node: node.to_string(),
        })
    }
    fn request(&self, method: Method, url: Url) -> RequestBuilder {
        let request_builder = self.client.request(method, url);
        // let mut req = Request::new(Method::POST, url);
        match &self.authentication {
            ProxmoxAuth::Password(password) => {
                // TODO: switch to query if GET
                let params = [("username", self.username.as_str()), ("password", password)];
                request_builder.form(&params)
            }
            ProxmoxAuth::ApiToken { id, token } => {
                // Format: PVEAPIToken=USER@REALM!TOKENID=UUID
                // id already contains the full token ID (e.g., "user@pve!tokenname")
                let value = format!("PVEAPIToken={id}={token}");
                request_builder.header("Authorization", value)
            }
        }
    }

    #[allow(dead_code)]
    async fn get_token(&self) -> Result<()> {
        let url = self.base_url.join("access/ticket").unwrap();
        // let mut request_builder = self.client.post(url);
        // let mut req = Request::new(Method::POST, url);
        let req = self.request(Method::POST, url);
        let r = req.send().await?;
        let status = r.status();
        let resp = r.text().await?;
        info!("Status: {status} Response: {resp}");
        Ok(())
    }
    pub async fn list_vms(&self) -> Result<Vec<VirtualMachine>> {
        let mut url = self.base_url.join("cluster/resources/").unwrap();
        url.set_query(Some("type=vm"));
        let req = self.request(Method::GET, url);
        let r = req.send().await?;
        let status = r.status();
        if status.is_success() {
            let resp: VmResponse = r.json().await?;
            Ok(resp.data)
        } else {
            Err(Error::HttpError(status))
        }
    }
    pub async fn list_vm_snapshots(&self, vm: &VirtualMachine) -> Result<Vec<Snapshot>> {
        let path = format!("nodes/{}/qemu/{}/snapshot", vm.node, vm.vmid);
        let url = self.base_url.join(&path).unwrap();
        let resp = self.request(Method::GET, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: SnapshotListResponse = resp.json().await?;
            Ok(resp.data)
        } else {
            Err(Error::HttpError(status))
        }
    }
    pub async fn exec_on_vm(&self, vm: &VirtualMachine, _command: Command) -> Result<()> {
        let path = format!("nodes/{}/qemu/{}/agent/exec", vm.node, vm.vmid);
        let url = self.base_url.join(&path).unwrap();
        // let cmd = command.get_program();
        // let cmd_s = format!("command[]=hostname");
        // url.set_query(Some(&cmd_s));
        // let data = ExecData {
        //     command: command.to_vec(),
        // };
        let mut form = HashMap::new();
        form.insert("command", "hostname");
        let resp = self.request(Method::POST, url).form(&form).send().await?;
        let status = resp.status();
        // if status.is_success() {
        //     let text = resp.text().await?;
        //     println!("Text: {text}");
        // }
        // Ok(())
        if status.is_success() {
            let resp: ExecDataResponse = resp.json().await?;
            println!("PID: {}", resp.data.pid);
            self.exec_status(vm, resp.data.pid).await?;
            Ok(())
        } else {
            let body = resp
                .text()
                .await
                .unwrap_or_else(|e| format!("Error with text(): {e}"));
            error!("Failed to exec: {status} - {body}");
            Err(Error::HttpError(status))
        }
    }
    async fn exec_status(&self, vm: &VirtualMachine, pid: usize) -> Result<()> {
        let path = format!("nodes/{}/qemu/{}/agent/exec-status", vm.node, vm.vmid);
        let url = self.base_url.join(&path).unwrap();
        let data = ExecStatusData { pid };
        let resp = self.request(Method::POST, url).json(&data).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: ExecStatusDataResponse = resp.json().await?;
            println!("Response: {:?}", resp.data);

            Ok(())
        } else {
            error!("Failed to exec-status on pid {pid}: {status}");
            Err(Error::HttpError(status))
        }
    }
    pub async fn ping(&self, vm: &VirtualMachine) -> Result<()> {
        let path = format!("nodes/{}/qemu/{}/agent/ping", vm.node, vm.vmid);
        let url = self.base_url.join(&path).unwrap();
        let resp = self.request(Method::POST, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            info!("VM Ping successful");
            Ok(())
        } else {
            error!("Failed to ping: {status}");
            Err(Error::HttpError(status))
        }
    }

    // ========================================================================
    // VM Lifecycle Methods
    // ========================================================================

    /// Get the node name this client is configured for.
    pub fn node(&self) -> &str {
        &self.node
    }

    /// Get the next available VM ID from the cluster.
    pub async fn get_next_vmid(&self) -> Result<u32> {
        let url = self.base_url.join("cluster/nextid").unwrap();
        let resp = self.request(Method::GET, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: NextIdResponse = resp.json().await?;
            let vmid: u32 = resp.data.parse().map_err(|_| {
                Error::TaskFailed(format!("Invalid VMID response: {}", resp.data))
            })?;
            Ok(vmid)
        } else {
            error!("Failed to get next VMID: {status}");
            Err(Error::HttpError(status))
        }
    }

    /// Clone a VM from a template.
    ///
    /// # Arguments
    /// * `source_vmid` - The template VM ID to clone from
    /// * `target_vmid` - The new VM ID for the clone
    /// * `name` - Name for the new VM
    /// * `linked` - If true, create a linked clone (faster, requires LVM-thin/ZFS)
    /// * `storage` - Storage pool for the clone (optional, uses template's storage if None)
    ///
    /// Returns the UPID (task ID) for tracking the clone operation.
    pub async fn clone_vm(
        &self,
        source_vmid: u32,
        target_vmid: u32,
        name: &str,
        linked: bool,
        storage: Option<&str>,
    ) -> Result<String> {
        let path = format!("nodes/{}/qemu/{}/clone", self.node(), source_vmid);
        let url = self.base_url.join(&path).unwrap();

        let params = CloneParams {
            newid: target_vmid,
            name: name.to_string(),
            full: if linked { Some(0) } else { Some(1) },
            // storage parameter is not allowed for linked clones
            storage: if linked { None } else { storage.map(|s| s.to_string()) },
            target: Some(self.node().to_string()),
        };

        let resp = self.request(Method::POST, url).form(&params).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: TaskResponse = resp.json().await?;
            debug!("Clone task started: {}", resp.data);
            Ok(resp.data)
        } else {
            let body = resp.text().await.unwrap_or_default();
            error!("Failed to clone VM {source_vmid}: {status} - {body}");
            Err(Error::HttpError(status))
        }
    }

    /// Start a VM.
    ///
    /// Returns the UPID (task ID) for tracking the start operation.
    pub async fn start_vm(&self, vmid: u32) -> Result<String> {
        let path = format!("nodes/{}/qemu/{}/status/start", self.node(), vmid);
        let url = self.base_url.join(&path).unwrap();

        let resp = self.request(Method::POST, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: TaskResponse = resp.json().await?;
            debug!("Start task for VM {vmid}: {}", resp.data);
            Ok(resp.data)
        } else {
            let body = resp.text().await.unwrap_or_default();
            error!("Failed to start VM {vmid}: {status} - {body}");
            Err(Error::HttpError(status))
        }
    }

    /// Stop a VM (graceful shutdown via ACPI).
    ///
    /// Returns the UPID (task ID) for tracking the stop operation.
    pub async fn stop_vm(&self, vmid: u32) -> Result<String> {
        let path = format!("nodes/{}/qemu/{}/status/stop", self.node(), vmid);
        let url = self.base_url.join(&path).unwrap();

        let resp = self.request(Method::POST, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: TaskResponse = resp.json().await?;
            debug!("Stop task for VM {vmid}: {}", resp.data);
            Ok(resp.data)
        } else {
            let body = resp.text().await.unwrap_or_default();
            error!("Failed to stop VM {vmid}: {status} - {body}");
            Err(Error::HttpError(status))
        }
    }

    /// Shutdown a VM gracefully via QEMU guest agent (if available).
    pub async fn shutdown_vm(&self, vmid: u32) -> Result<String> {
        let path = format!("nodes/{}/qemu/{}/status/shutdown", self.node(), vmid);
        let url = self.base_url.join(&path).unwrap();

        let resp = self.request(Method::POST, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: TaskResponse = resp.json().await?;
            debug!("Shutdown task for VM {vmid}: {}", resp.data);
            Ok(resp.data)
        } else {
            let body = resp.text().await.unwrap_or_default();
            error!("Failed to shutdown VM {vmid}: {status} - {body}");
            Err(Error::HttpError(status))
        }
    }

    /// Delete a VM.
    ///
    /// Note: VM must be stopped before deletion.
    pub async fn delete_vm(&self, vmid: u32) -> Result<String> {
        let path = format!("nodes/{}/qemu/{}", self.node(), vmid);
        let url = self.base_url.join(&path).unwrap();

        let resp = self.request(Method::DELETE, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: TaskResponse = resp.json().await?;
            debug!("Delete task for VM {vmid}: {}", resp.data);
            Ok(resp.data)
        } else {
            let body = resp.text().await.unwrap_or_default();
            error!("Failed to delete VM {vmid}: {status} - {body}");
            Err(Error::HttpError(status))
        }
    }

    /// Get VM status.
    pub async fn get_vm_status(&self, vmid: u32) -> Result<VmStatus> {
        let path = format!("nodes/{}/qemu/{}/status/current", self.node(), vmid);
        let url = self.base_url.join(&path).unwrap();

        let resp = self.request(Method::GET, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: VmStatusResponse = resp.json().await?;
            Ok(resp.data)
        } else {
            error!("Failed to get VM {vmid} status: {status}");
            Err(Error::HttpError(status))
        }
    }

    /// Get network interfaces from QEMU guest agent.
    pub async fn get_network_interfaces(&self, vmid: u32) -> Result<Vec<NetworkInterface>> {
        let path = format!("nodes/{}/qemu/{}/agent/network-get-interfaces", self.node(), vmid);
        let url = self.base_url.join(&path).unwrap();

        let resp = self.request(Method::GET, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: NetworkInterfacesResponse = resp.json().await?;
            Ok(resp.data.result)
        } else if status == StatusCode::INTERNAL_SERVER_ERROR {
            // Guest agent not available
            Err(Error::GuestAgentUnavailable)
        } else {
            error!("Failed to get network interfaces for VM {vmid}: {status}");
            Err(Error::HttpError(status))
        }
    }

    /// Get task status.
    pub async fn get_task_status(&self, upid: &str) -> Result<TaskStatus> {
        // UPID format: UPID:node:pid:pstart:starttime:type:id:user@realm:
        // Extract node from UPID
        let parts: Vec<&str> = upid.split(':').collect();
        let node = if parts.len() > 1 { parts[1] } else { self.node() };

        let path = format!("nodes/{}/tasks/{}/status", node, urlencoding::encode(upid));
        let url = self.base_url.join(&path).unwrap();

        let resp = self.request(Method::GET, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            let resp: TaskStatusResponse = resp.json().await?;
            Ok(resp.data)
        } else {
            error!("Failed to get task status: {status}");
            Err(Error::HttpError(status))
        }
    }

    /// Wait for a Proxmox task to complete.
    ///
    /// Polls the task status until it completes or times out.
    pub async fn wait_for_task(&self, upid: &str) -> Result<()> {
        self.wait_for_task_with_timeout(upid, Duration::from_secs(300)).await
    }

    /// Wait for a Proxmox task to complete with a custom timeout.
    pub async fn wait_for_task_with_timeout(&self, upid: &str, timeout: Duration) -> Result<()> {
        let start = std::time::Instant::now();

        loop {
            if start.elapsed() > timeout {
                return Err(Error::Timeout(format!(
                    "Task {upid} did not complete within {timeout:?}"
                )));
            }

            let status = self.get_task_status(upid).await?;

            match status.status.as_str() {
                "stopped" => {
                    if let Some(exit) = &status.exitstatus {
                        if exit == "OK" {
                            return Ok(());
                        } else {
                            return Err(Error::TaskFailed(exit.clone()));
                        }
                    }
                    // No exitstatus but stopped - assume success
                    return Ok(());
                }
                "running" => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                other => {
                    return Err(Error::UnknownTaskStatus(other.to_string()));
                }
            }
        }
    }

    /// Poll for VM IP address via QEMU guest agent.
    ///
    /// Waits for the VM to boot and the guest agent to report network interfaces.
    pub async fn poll_for_ip(&self, vmid: u32, timeout: Duration) -> Result<String> {
        let start = std::time::Instant::now();

        while start.elapsed() < timeout {
            match self.get_network_interfaces(vmid).await {
                Ok(interfaces) => {
                    // Find first non-loopback IPv4 address
                    for iface in interfaces {
                        if iface.name == "lo" || iface.name == "Loopback Pseudo-Interface 1" {
                            continue;
                        }
                        for addr in iface.ip_addresses {
                            if addr.ip_address_type == "ipv4"
                                && !addr.ip_address.starts_with("169.254.") // Skip link-local
                                && !addr.ip_address.starts_with("127.")
                            {
                                debug!("Found IP {} on interface {}", addr.ip_address, iface.name);
                                return Ok(addr.ip_address);
                            }
                        }
                    }
                    debug!("Guest agent ready but no valid IP found yet");
                }
                Err(Error::GuestAgentUnavailable) => {
                    debug!("Guest agent not ready yet for VM {}", vmid);
                }
                Err(e) => {
                    debug!("Error getting network interfaces: {}", e);
                }
            }

            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        Err(Error::Timeout(format!(
            "VM {vmid} did not get an IP address within {timeout:?}"
        )))
    }

    /// Ping the QEMU guest agent to check if it's ready.
    pub async fn ping_guest_agent(&self, vmid: u32) -> Result<()> {
        let path = format!("nodes/{}/qemu/{}/agent/ping", self.node(), vmid);
        let url = self.base_url.join(&path).unwrap();
        let resp = self.request(Method::POST, url).send().await?;
        let status = resp.status();
        if status.is_success() {
            Ok(())
        } else if status == StatusCode::INTERNAL_SERVER_ERROR {
            Err(Error::GuestAgentUnavailable)
        } else {
            Err(Error::HttpError(status))
        }
    }
}
#[allow(dead_code)]
trait CommandExt {
    fn to_vec(self) -> Vec<OsString>;
}
impl CommandExt for Command {
    fn to_vec(self) -> Vec<OsString> {
        let mut args: Vec<OsString> = self.get_args().map(|s| s.to_os_string()).collect();
        args.insert(0, self.get_program().to_os_string());
        args
    }
}
#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct ExecData {
    command: Vec<OsString>,
}
#[derive(Debug, Serialize, Deserialize)]
struct ExecResponse {
    pid: usize,
}
#[derive(Debug, Deserialize)]
struct ExecDataResponse {
    data: ExecResponse,
}

#[derive(Debug, Serialize)]
struct ExecStatusData {
    pid: usize,
}
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
struct ExecStatusResponse {
    exited: bool,
    #[serde(rename = "err-data")]
    err_data: String,
    #[serde(rename = "err-truncated")]
    err_truncated: String,
    #[serde(rename = "out-data")]
    out_data: String,
    #[serde(rename = "out-truncated")]
    out_truncated: String,
    exitcode: usize,
}

#[derive(Debug, Deserialize)]
struct ExecStatusDataResponse {
    data: ExecStatusResponse,
}

// ============================================================================
// Additional response types for VM operations
// ============================================================================

/// Response for /cluster/nextid
#[derive(Debug, Deserialize)]
struct NextIdResponse {
    data: String,
}

/// Response for clone/start/stop operations (returns UPID task ID)
#[derive(Debug, Deserialize)]
struct TaskResponse {
    data: String,
}

/// Response for task status
#[derive(Debug, Deserialize)]
struct TaskStatusResponse {
    data: TaskStatus,
}

#[derive(Debug, Deserialize)]
pub struct TaskStatus {
    pub status: String,
    #[serde(default)]
    pub exitstatus: Option<String>,
    #[serde(default)]
    pub node: Option<String>,
    #[serde(rename = "type", default)]
    pub task_type: Option<String>,
}

/// Response for VM status
#[derive(Debug, Deserialize)]
struct VmStatusResponse {
    data: VmStatus,
}

#[derive(Debug, Deserialize)]
pub struct VmStatus {
    pub status: String,
    #[serde(default)]
    pub qmpstatus: Option<String>,
    #[serde(default)]
    pub pid: Option<u32>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub vmid: Option<u32>,
}

/// Response for network interfaces from QEMU guest agent
#[derive(Debug, Deserialize)]
struct NetworkInterfacesResponse {
    data: NetworkInterfacesData,
}

#[derive(Debug, Deserialize)]
struct NetworkInterfacesData {
    result: Vec<NetworkInterface>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NetworkInterface {
    pub name: String,
    #[serde(rename = "hardware-address", default)]
    pub hardware_address: Option<String>,
    #[serde(rename = "ip-addresses", default)]
    pub ip_addresses: Vec<IpAddress>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct IpAddress {
    #[serde(rename = "ip-address")]
    pub ip_address: String,
    #[serde(rename = "ip-address-type")]
    pub ip_address_type: String,
    #[serde(default)]
    pub prefix: Option<u32>,
}

/// Clone parameters
#[derive(Debug, Serialize)]
struct CloneParams {
    newid: u32,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    full: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
}
