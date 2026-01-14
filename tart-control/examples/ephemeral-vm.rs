use anyhow::Result;
use std::time::Duration;
use tart_control::EphemeralVm;
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
    let mut vm = EphemeralVm::new("ventura-astro", 1);
    vm.start().await?;
    tokio::time::sleep(Duration::from_secs(30)).await;
    println!("30s over, stopping VM");
    vm.stop().await?;
    println!("VM should be gone");
    Ok(())
}
