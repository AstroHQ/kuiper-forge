use anyhow::Result;
use std::time::Duration;
use tart_control::run_vm;

fn main() -> Result<()> {
    {
        let _vm = run_vm("ventura-astro")?;
        std::thread::sleep(Duration::from_secs(10));
        println!("Timeout, killing...");
    }
    std::thread::sleep(Duration::from_secs(10));
    println!("Done");
    Ok(())
}
