use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
// use std::process::Stdio;
// use std::time::Duration;
use std::{net::Ipv4Addr, str::FromStr};
// use tokio::io::{stderr, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tracing::{error, trace};

mod ssh_control;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO Error: {0}")]
    IoError(#[from] std::io::Error),
    #[error("Executing tart CLI failed")]
    TartCLIFailure,
    #[error("JSON Error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("VM requested was not found")]
    VirtualMachineNotFound,
    #[error("Invalid UTF8 string encountered")]
    Utf8Error,
    #[error("VM is already running")]
    VirtualMachineAlreadyStarted,
}
pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct EphemeralVm {
    pub base_name: String,
    pub number: u8,
    our_name: String,
    running_vm: Option<RunningVm>,
}
impl EphemeralVm {
    pub fn new(base_name: &str, number: u8) -> Self {
        Self {
            base_name: base_name.to_string(),
            number,
            our_name: format!("ci-{}", number),
            running_vm: None,
        }
    }
    pub fn name(&self) -> &str {
        self.our_name.as_str()
    }
    pub async fn start(&mut self) -> Result<()> {
        if self.running_vm.is_some() {
            return Err(Error::VirtualMachineAlreadyStarted);
        }
        let vm = clone_vm(&self.base_name, self.name()).await?;
        trace!("VM cloned from {} to {}", self.base_name, self.name());
        self.running_vm = Some(vm.run()?);
        Ok(())
    }
    pub async fn stop(&mut self) -> Result<()> {
        self.destroy().await?;
        Ok(())
    }
    async fn destroy(&mut self) -> Result<()> {
        if let Some(mut vm) = self.running_vm.take() {
            vm.stop().await?;
        }
        match Command::new("tart")
            .arg("delete")
            .arg(self.name())
            .output()
            .await
        {
            Ok(output) => {
                trace!("tart delete output: {output:?}");
            }
            Err(e) => {
                error!("Failed to remove ephemeral VM: {e}");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct VirtualMachine {
    pub source: String,
    pub name: String,
    pub size: usize,
}
impl VirtualMachine {
    pub async fn find(name: &str) -> Result<Self> {
        let vms = list_vms().await?;
        let vm = vms
            .into_iter()
            .find(|vm| vm.name == name)
            .ok_or(Error::VirtualMachineNotFound)?;
        Ok(vm)
    }
    pub fn run(&self) -> Result<RunningVm> {
        run_vm(&self.name)
    }
    pub async fn clone(&self, name: &str) -> Result<Self> {
        clone_vm(&self.name, name).await
    }
    pub async fn ip(&self) -> Result<Ipv4Addr> {
        let ip_s = run_cmd("ip", &self.name).await?;
        let ip_s = ip_s.trim_end();
        Ok(Ipv4Addr::from_str(ip_s).unwrap())
    }
    pub async fn ssh_keyscan(&self) -> Result<Vec<String>> {
        let ip = self.ip().await?;
        let output = Command::new("ssh-keyscan")
            .arg(ip.to_string())
            .output()
            .await?;
        if !output.status.success() {
            return Err(Error::TartCLIFailure);
        }
        let output = String::from_utf8(output.stdout).map_err(|_| Error::Utf8Error)?;
        let mut keys = Vec::new();
        for line in output.lines().filter(|l| !l.starts_with('#')) {
            keys.push(line.to_string());
        }
        Ok(keys)
    }
    pub async fn is_key_installed(&self) -> Result<bool> {
        let output = Command::new("ssh-keygen")
            .arg("-F")
            .arg(self.ip().await?.to_string())
            .output()
            .await?;
        Ok(!output.stdout.is_empty())
    }
    /// Installs VM's host SSH key into local machine's known_hosts
    pub async fn install_host_ssh_key(&self) -> Result<()> {
        use std::io::Write; // TODO: make async
        if self.is_key_installed().await? {
            println!("Already installed ssh key locally, skipping");
            return Ok(());
        }
        let keys = self.ssh_keyscan().await?;
        let key_file_path = dirs::home_dir().unwrap().join(".ssh").join("known_hosts");
        println!("Opening {}", key_file_path.display());
        let mut key_file = OpenOptions::new().append(true).open(key_file_path)?;
        let header = format!("# Tart VM {} - {}\n", self.name, self.ip().await?);
        key_file.write_all(header.as_bytes())?;
        for key in keys {
            key_file.write_all(format!("{key}\n").as_bytes())?;
        }
        Ok(())
    }
    // agent: https://github.com/actions/runner/releases/download/v2.315.0/actions-runner-osx-arm64-2.315.0.tar.gz"
    // pub async fn install_client_ssh_key(&self) -> Result<()> {
    //     let mut child = Command::new("ssh-copy-id")
    //         .arg(format!("admin@{}", self.ip().await?))
    //         .stdout(Stdio::piped())
    //         .stdin(Stdio::piped())
    //         .stderr(Stdio::piped())
    //         .spawn()?;
    //     let mut stdout = child.stdout.take().unwrap();
    //     let mut stdin = child.stdin.take().unwrap();
    //     let mut stderr = child.stderr.take().unwrap();
    //     tokio::spawn(async move {
    //         loop {
    //             let mut out = [0u8; 32];
    //             let bytes = stdout.read(&mut out).await.unwrap();
    //             let output = std::str::from_utf8(&out[0..bytes])
    //                 .map_err(|_| Error::Utf8Error)
    //                 .unwrap();
    //             println!("Output: {output}");
    //         }
    //     });
    //     tokio::spawn(async move {
    //         loop {
    //             let mut out = [0u8; 32];
    //             let bytes = stderr.read(&mut out).await.unwrap();
    //             let output = std::str::from_utf8(&out[0..bytes])
    //                 .map_err(|_| Error::Utf8Error)
    //                 .unwrap();
    //             println!("Stderr: {output}");
    //         }
    //     });
    //     tokio::time::sleep(Duration::from_secs(1)).await;
    //     stdin.write_all(b"admin\n").await?;
    //     tokio::time::sleep(Duration::from_secs(5)).await;
    //     // let mut out = [0u8; 32];
    //     // let bytes = stdout.read(&mut out).await?;
    //     // let output = std::str::from_utf8(&out[0..bytes]).map_err(|_| Error::Utf8Error)?;
    //     // println!("Output: {output}");
    //     child.kill().await?;
    //     todo!()
    // }
    pub async fn exec(&self, _command: &str) -> Result<String> {
        // let mut child = Command::new("ssh")
        //     .arg(format!("admin@{}", self.ip()?))
        //     .stdout(Stdio::piped())
        //     .stdin(Stdio::piped())
        //     .spawn()?;
        // let stdout = child.stdout.as_mut().unwrap();
        // let mut out = [0u8; 32];
        // let bytes = stdout.read(&mut out)?;
        // let output = std::str::from_utf8(&out[0..bytes]).map_err(|_| Error::Utf8Error)?;
        // println!("Output: {output}");
        //
        // let stdin = child.stdin.as_mut().unwrap();
        // stdin.write_all(b"admin\n")?;
        // std::thread::sleep(Duration::from_secs(1));
        // stdin.write_all(b"hostname\n")?;
        //
        // let bytes = stdout.read(&mut out)?;
        // let output = std::str::from_utf8(&out[0..bytes]).map_err(|_| Error::Utf8Error)?;
        // println!("Output2: {output}");
        // child.kill().unwrap();
        // Ok(output.to_string())
        ssh_control::start_runner(self.ip().await?, "").await?;
        Ok(String::new())
    }
}

async fn run_cmd(cmd: &str, name: &str) -> Result<String> {
    let output = Command::new("tart").arg(cmd).arg(name).output().await?;
    String::from_utf8(output.stdout).map_err(|_| Error::Utf8Error)
}

pub async fn list_vms() -> Result<Vec<VirtualMachine>> {
    let output = Command::new("tart")
        .arg("list")
        .arg("--source")
        .arg("local")
        .arg("--format")
        .arg("json")
        .output()
        .await?;
    if output.status.success() {
        let vms: Vec<VirtualMachine> = serde_json::from_slice(output.stdout.as_slice())?;
        Ok(vms)
    } else {
        Err(Error::TartCLIFailure)
    }
}
pub struct RunningVm {
    child: Option<Child>,
}
impl RunningVm {
    pub async fn stop(&mut self) -> Result<()> {
        if let Some(mut child) = self.child.take() {
            child.kill().await?;
            child.wait().await?;
        }
        Ok(())
    }
}
impl Drop for RunningVm {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            tokio::spawn(async move { child.kill().await });
        }
    }
}
pub fn run_vm(vm_name: &str) -> Result<RunningVm> {
    let child = Command::new("tart").arg("run").arg(vm_name).spawn()?;
    Ok(RunningVm { child: Some(child) })
}

pub async fn clone_vm(name: &str, destination: &str) -> Result<VirtualMachine> {
    let output = Command::new("tart")
        .arg("clone")
        .arg(name)
        .arg(destination)
        .output()
        .await?;
    if output.status.success() {
        Ok(VirtualMachine::find(destination).await?)
    } else {
        let msg = std::str::from_utf8(&output.stdout).unwrap();
        error!("Failed to clone: {msg}");
        Err(Error::TartCLIFailure)
    }
}
