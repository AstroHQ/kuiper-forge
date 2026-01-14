use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    let vms = tart_control::list_vms().await?;
    println!("VMs: {vms:#?}");
    Ok(())
}
