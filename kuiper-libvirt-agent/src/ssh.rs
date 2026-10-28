//! SSH to libvirt VMs, over the shared transport in [`kuiper_agent_lib::ssh`].

use crate::config::SshConfig;
use crate::error::{Error, Result};
use kuiper_agent_lib::ssh::{SshAuth, SshClient, load_key};
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
            return Err(Error::Ssh(
                "SSH config must have either private_key_path or password".to_string(),
            ));
        }
    };
    Ok(SshClient::new(&config.username, auth)
        .with_port(config.port)
        .with_timeout(Duration::from_secs(config.timeout_secs)))
}
