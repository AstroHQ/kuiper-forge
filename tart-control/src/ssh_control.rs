use std::net::Ipv4Addr;
use tokio::process::Command;

pub async fn start_runner(host: Ipv4Addr, _key: &str) -> std::io::Result<()> {
    let output = Command::new("ssh")
        .arg(format!("admin@{host}"))
        .arg("ls")
        .output()
        .await?;
    println!("Result: {output:?}");
    Ok(())
}
