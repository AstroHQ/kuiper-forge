use anyhow::Result;
use proxmox_api::{ProxmoxAuth, ProxmoxVEAPI};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    let fmt_layer = fmt::layer();
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .init();
    let proxmox = ProxmoxVEAPI::new(
        "root@pam",
        // ProxmoxAuth::api_token("vm-control", "ad97f909-34ec-4f4f-959f-ce3b959a2f3f"),
        ProxmoxAuth::api_token("vm-control", "42741fe0-54c1-43a4-83c0-7156d090adcf"),
        // "https://192.168.1.254:8006/",
        "https://192.168.1.253:8006/",
        true,
    )?;
    let vms = proxmox.list_vms().await?;
    for vm in vms {
        let snapshots = proxmox.list_vm_snapshots(&vm).await?;
        println!(
            "VM: {} ({}) - {} - Snapshots: {snapshots:?}",
            vm.name, vm.vmid, vm.status
        );
    }
    Ok(())
}
