use anyhow::Result;
use tart_control::VirtualMachine;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> Result<()> {
    let fmt_layer = fmt::layer();
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("trace"))
        .unwrap();
    tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer)
        .init();
    let vm = VirtualMachine::find("ventura-astro").await?;
    // let keys = vm.ssh_keyscan().await?;
    // println!("Keys: {keys:?}");
    vm.install_host_ssh_key().await?;
    // vm.install_client_ssh_key().await?;
    let r = vm.exec("hostname").await?;
    println!("Result: {r}");
    Ok(())
}
