//! SSH to Proxmox VMs, over the shared transport in [`kuiper_agent_lib::ssh`].

use crate::config::SshConfig;
use crate::error::{Error, Result};
use kuiper_agent_lib::ssh::{SshAuth, SshClient, load_key};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

/// Build the SSH client from config. With both a key and a password, the key is tried first.
pub fn client(config: &SshConfig) -> Result<SshClient> {
    let auth = match (&config.private_key_path, &config.password) {
        (Some(key_path), Some(password)) => SshAuth::KeyThenPassword {
            key: Arc::new(load_key(key_path)?),
            password: password.clone(),
        },
        (Some(key_path), None) => SshAuth::key_file(key_path)?,
        (None, Some(password)) => SshAuth::Password(password.clone()),
        (None, None) => {
            return Err(Error::ssh(
                "SSH config must have either private_key_path or password",
            ));
        }
    };
    Ok(SshClient::new(&config.username, auth)
        .with_port(config.port)
        .with_timeout(Duration::from_secs(config.timeout_secs)))
}

/// Parse a VM IP address as Proxmox reports it.
pub fn parse_ip(ip: &str) -> Result<IpAddr> {
    ip.parse()
        .map_err(|e| Error::ssh(format!("Invalid VM IP address '{ip}': {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_needs_key_or_password() {
        let config: SshConfig = toml::from_str("").unwrap();
        assert!(client(&config).is_err());

        let config: SshConfig = toml::from_str("password = \"x\"\nport = 2222").unwrap();
        let client = client(&config).unwrap();
        assert_eq!(client.username(), "ci");
    }

    #[test]
    fn test_parse_ip() {
        assert!(parse_ip("10.0.0.5").is_ok());
        assert!(parse_ip("fe80::1").is_ok());
        assert!(parse_ip("not-an-ip").is_err());
    }
}
