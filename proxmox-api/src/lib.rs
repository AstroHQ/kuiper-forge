use reqwest::{Client, IntoUrl, Method, RequestBuilder, StatusCode, Url};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt::Debug;
use std::process::Command;
use tracing::{error, info};

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

#[derive(thiserror::Error, Debug)]
pub enum Error {
    /// Error within reqwest library
    #[error("API error with reqwest: {0}")]
    ReqwestError(#[from] reqwest::Error),
    /// Error returned by server
    #[error("HTTP Error from API: {0}")]
    HttpError(StatusCode),
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
                let value = format!("PVEAPIToken={}!{id}={token}", self.username);
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
