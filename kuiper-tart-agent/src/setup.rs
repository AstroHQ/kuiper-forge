//! Interactive first-run wizard (`setup`) and the registration step it shares with `register`.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Input, MultiSelect, Password, Select};
use kuiper_agent_lib::{AgentCertStore, RegistrationBundle};

use crate::config::{self, Config};
use crate::host_checks;

/// Images offered in the picker on top of whatever `tart list` already has.
const PRESET_IMAGES: &[(&str, &str)] = &[
    ("macOS Tahoe", "ghcr.io/cirruslabs/macos-tahoe-base:latest"),
    (
        "macOS Tahoe + Xcode",
        "ghcr.io/cirruslabs/macos-tahoe-xcode:latest",
    ),
    (
        "macOS Sequoia",
        "ghcr.io/cirruslabs/macos-sequoia-base:latest",
    ),
    (
        "macOS Sequoia + Xcode",
        "ghcr.io/cirruslabs/macos-sequoia-xcode:latest",
    ),
    ("Ubuntu", "ghcr.io/cirruslabs/ubuntu:latest"),
];

const DEFAULT_LABELS: &str = "self-hosted, macOS, ARM64";

/// Walk a fresh host through tart + DHCP checks, registration, labels, image mappings and limits, write the config,
/// pull the images and optionally install the LaunchAgent. Re-running it edits the existing config.
pub async fn run_setup(config_path: &Path) -> Result<()> {
    let theme = ColorfulTheme::default();

    println!("\n== Host ==");
    check_tart(&theme)?;
    check_dhcp(&theme)?;

    println!("\n== Registration ==");
    let existing = config_path
        .exists()
        .then(|| Config::read(config_path))
        .transpose()
        .context("existing config is unreadable, fix or remove it first")?;
    let Some(mut config) = registration_step(&theme, existing).await? else {
        println!(
            "Skipped registration, run `kuiper-tart-agent setup` again when you have a bundle."
        );
        return Ok(());
    };

    println!("\n== Labels & images ==");
    let local = host_checks::list_images().unwrap_or_default();
    labels_step(&theme, &mut config)?;
    config.tart.base_image = pick_image(
        &theme,
        "Default image (used when no mapping matches)",
        &local,
        Some(&config.tart.base_image).filter(|i| !i.is_empty()),
    )?;
    mappings_step(&theme, &mut config, &local)?;

    println!("\n== Limits & SSH ==");
    limits_step(&theme, &mut config)?;
    ssh_step(&theme, &mut config)?;

    config.save(config_path)?;
    println!("\n✓ Config saved to {}", config_path.display());

    pull_step(&theme, &config, &local)?;

    if Confirm::with_theme(&theme)
        .with_prompt("Install as a LaunchAgent so it runs at login?")
        .default(true)
        .interact()?
    {
        println!();
        crate::cmd_install(false, true, config_path).await?;
    } else {
        println!("\nStart the agent with: kuiper-tart-agent");
    }
    Ok(())
}

/// Decode a bundle, save its server trust and register. Returns the coordinator + TLS sections for the config.
pub async fn register(
    bundle_token: &str,
) -> Result<(config::CoordinatorConfig, config::TlsConfig)> {
    let bundle = RegistrationBundle::decode(bundle_token)
        .map_err(|e| anyhow!("Invalid registration bundle: {e}"))?;
    println!("Coordinator: {}", bundle.coordinator_url);

    let certs_dir = Config::default_data_dir().join("certs");
    std::fs::create_dir_all(&certs_dir)?;

    let cert_store = AgentCertStore::new(certs_dir.clone());
    if let Some(ca_pem) = bundle.server_ca_pem.as_deref() {
        cert_store.save_ca(ca_pem)?;
        println!("✓ Saved server CA certificate");
    }
    cert_store.save_server_trust_mode(match bundle.server_trust_mode {
        kuiper_agent_lib::bundle::ServerTrustMode::Ca => "ca",
        kuiper_agent_lib::bundle::ServerTrustMode::Chain => "chain",
    })?;

    let hostname = url::Url::parse(&bundle.coordinator_url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "localhost".to_string());

    // labels/max_vms aren't here, they go in the first AgentStatus when the daemon connects
    let agent_config = kuiper_agent_lib::AgentConfig {
        coordinator_url: bundle.coordinator_url.clone(),
        coordinator_hostname: hostname.clone(),
        registration_token: Some(bundle.token),
        agent_type: "tart".to_string(),
    };

    // register() not connect(): always re-register with this token instead of silently reusing an existing (possibly
    // revoked) cert. the new identity is only written on success, so a bad token leaves the old cert alone
    println!("Connecting to coordinator...");
    let mut connector = kuiper_agent_lib::AgentConnector::new(agent_config, cert_store.clone());
    connector
        .register()
        .await
        .map_err(|e| anyhow!("Registration failed: {e}"))?;

    let agent_id = cert_store
        .get_agent_id()
        .ok_or_else(|| anyhow!("Failed to get agent ID after registration"))?;
    println!("✓ Registered as {agent_id}");

    Ok((
        config::CoordinatorConfig {
            url: bundle.coordinator_url,
            hostname,
        },
        config::TlsConfig {
            ca_cert: Some(certs_dir.join("ca.crt")),
            certs_dir,
        },
    ))
}

/// A config with the coordinator/TLS sections filled in and everything else blank or default.
pub fn blank_config(coordinator: config::CoordinatorConfig, tls: config::TlsConfig) -> Config {
    Config {
        coordinator,
        tls,
        agent: config::AgentConfig { labels: vec![] },
        tart: config::TartConfig {
            base_image: String::new(),
            max_macos_vms: config::MACOS_GUEST_LIMIT,
            max_total_vms: 5,
            shared_cache_dir: None,
            ssh: config::SshAuthConfig::default(),
            runner_version: "latest".to_string(),
            image_mappings: vec![],
        },
        cleanup: config::CleanupConfig::default(),
        reconnect: config::ReconnectConfig::default(),
        host: config::HostConfig::default(),
        logging: config::LoggingConfig::default(),
    }
}

fn check_tart(theme: &ColorfulTheme) -> Result<()> {
    match host_checks::check_tart_version() {
        Ok(v) => {
            println!("✓ tart {v}");
            return Ok(());
        }
        Err(msg) => println!("✗ {msg}"),
    }
    if !Confirm::with_theme(theme)
        .with_prompt("Install tart via Homebrew?")
        .default(true)
        .interact()?
    {
        bail!("the agent needs tart 2.x, install it and re-run setup");
    }

    // inherits the terminal, so brew can ask to trust the cirruslabs tap itself
    run_inherited("brew", &["install", "cirruslabs/cli/tart"])?;
    let v = host_checks::check_tart_version().map_err(|e| anyhow!(e))?;
    println!("✓ tart {v}");
    Ok(())
}

fn check_dhcp(theme: &ColorfulTheme) -> Result<()> {
    let Err(msg) = host_checks::check_dhcp_lease_time() else {
        println!("✓ DHCP lease time");
        return Ok(());
    };
    println!("✗ {msg}");
    if Confirm::with_theme(theme)
        .with_prompt("Fix it now? (runs `defaults write` with sudo)")
        .default(true)
        .interact()?
    {
        let args = host_checks::dhcp_lease_fix_args();
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        match run_inherited("sudo", &args) {
            Ok(()) => println!("✓ DHCP lease time set"),
            Err(e) => println!(
                "⚠ {e}\n  fix it by hand: {}",
                host_checks::dhcp_lease_fix_command()
            ),
        }
    } else {
        println!(
            "  the agent refuses to start until this is fixed, unless [host] dhcp_lease_check is relaxed"
        );
    }
    Ok(())
}

/// Returns the config to fill in, or `None` if there's no registration yet and the operator skipped it.
async fn registration_step(
    theme: &ColorfulTheme,
    existing: Option<Config>,
) -> Result<Option<Config>> {
    let registered = existing.as_ref().and_then(|c| {
        let store = AgentCertStore::new(c.tls.certs_dir.clone());
        store
            .has_certificates()
            .then(|| store.get_agent_id())
            .flatten()
    });
    if let (Some(config), Some(id)) = (&existing, &registered) {
        println!("Registered as {id} with {}", config.coordinator.url);
        if !Confirm::with_theme(theme)
            .with_prompt("Re-register with a new bundle?")
            .default(false)
            .interact()?
        {
            return Ok(existing);
        }
    }

    let bundle: String = Input::with_theme(theme)
        .with_prompt("Registration bundle (kfr1_…), blank to skip")
        .allow_empty(true)
        .interact_text()?;
    let bundle = bundle.trim();
    if bundle.is_empty() {
        // a config without certs still can't run, but keep editing it if there is one
        return Ok(existing);
    }

    let (coordinator, tls) = register(bundle).await?;
    Ok(Some(match existing {
        Some(mut c) => {
            c.coordinator = coordinator;
            c.tls = tls;
            c
        }
        None => blank_config(coordinator, tls),
    }))
}

fn labels_step(theme: &ColorfulTheme, config: &mut Config) -> Result<()> {
    println!(
        "Agent labels go on every runner this host offers. Keep OS labels like macOS out if you add linux images."
    );
    let current = if config.agent.labels.is_empty() {
        DEFAULT_LABELS.to_string()
    } else {
        config.agent.labels.join(", ")
    };
    let input: String = Input::with_theme(theme)
        .with_prompt("Agent labels (comma separated)")
        .default(current)
        .validate_with(|s: &String| {
            if parse_labels(s).is_empty() {
                Err("need at least one label")
            } else {
                Ok(())
            }
        })
        .interact_text()?;
    config.agent.labels = parse_labels(&input);
    Ok(())
}

fn mappings_step(theme: &ColorfulTheme, config: &mut Config, local: &[String]) -> Result<()> {
    println!(
        "\nMappings pick an image by job labels. The first mapping whose labels (plus agent labels) \
         are all in `runs-on` wins."
    );
    let mappings = &mut config.tart.image_mappings;
    if !mappings.is_empty() {
        let items: Vec<String> = mappings.iter().map(describe_mapping).collect();
        let keep = MultiSelect::with_theme(theme)
            .with_prompt("Keep which existing mappings? (space toggles)")
            .items(&items)
            .defaults(&vec![true; items.len()])
            .interact()?;
        let mut i = 0;
        mappings.retain(|_| {
            i += 1;
            keep.contains(&(i - 1))
        });
    }

    while Confirm::with_theme(theme)
        .with_prompt("Add a label → image mapping?")
        .default(mappings.is_empty())
        .interact()?
    {
        let input: String = Input::with_theme(theme)
            .with_prompt("Job labels for this mapping (comma separated, e.g. xcode-16)")
            .validate_with(|s: &String| {
                if parse_labels(s).is_empty() {
                    Err("need at least one label")
                } else {
                    Ok(())
                }
            })
            .interact_text()?;
        let image = pick_image(theme, "Image for these labels", local, None)?;
        let pool = pool_prompt(theme)?;
        let mapping = config::ImageMapping {
            labels: parse_labels(&input),
            image,
            pool,
        };
        println!("  + {}", describe_mapping(&mapping));
        mappings.push(mapping);
    }
    Ok(())
}

fn pool_prompt(theme: &ColorfulTheme) -> Result<Option<u32>> {
    let pool: String = Input::with_theme(theme)
        .with_prompt(
            "Fixed pool size (blank = on demand; once any mapping has one, the rest get none)",
        )
        .allow_empty(true)
        .validate_with(|s: &String| {
            if s.trim().is_empty() || s.trim().parse::<u32>().is_ok() {
                Ok(())
            } else {
                Err("a whole number, or blank")
            }
        })
        .interact_text()?;
    Ok(pool.trim().parse().ok())
}

fn limits_step(theme: &ColorfulTheme, config: &mut Config) -> Result<()> {
    let tart = &mut config.tart;
    let options: Vec<u32> = (1..=config::MACOS_GUEST_LIMIT).collect();
    let idx = Select::with_theme(theme)
        .with_prompt("Max macOS VMs at once (macOS allows 2 per host)")
        .items(&options)
        .default(
            options
                .iter()
                .position(|&n| n == tart.max_macos_vms)
                .unwrap_or(options.len() - 1),
        )
        .interact()?;
    tart.max_macos_vms = options[idx];

    tart.max_total_vms = Input::with_theme(theme)
        .with_prompt("Max VMs of any OS at once")
        .default(tart.max_total_vms.max(tart.max_macos_vms))
        .validate_with(|n: &u32| if *n >= 1 { Ok(()) } else { Err("at least 1") })
        .interact_text()?;

    let pooled: u32 = tart.image_mappings.iter().filter_map(|m| m.pool).sum();
    if pooled > tart.max_total_vms {
        println!(
            "⚠ pools add up to {pooled} runners but only {} VMs can run, some won't fill",
            tart.max_total_vms
        );
    }
    Ok(())
}

fn ssh_step(theme: &ColorfulTheme, config: &mut Config) -> Result<()> {
    let ssh = &mut config.tart.ssh;
    ssh.username = Input::with_theme(theme)
        .with_prompt("SSH user inside the VMs")
        .default(ssh.username.clone())
        .interact_text()?;

    let methods = [
        ("default", "default keys (~/.ssh/id_ed25519 or id_rsa)"),
        ("password", "password"),
        ("key", "a specific private key"),
    ];
    let labels: Vec<&str> = methods.iter().map(|(_, l)| *l).collect();
    let idx = Select::with_theme(theme)
        .with_prompt("SSH auth")
        .items(&labels)
        .default(
            methods
                .iter()
                .position(|(m, _)| *m == ssh.auth_method)
                .unwrap_or(0),
        )
        .interact()?;
    ssh.auth_method = methods[idx].0.to_string();

    match ssh.auth_method.as_str() {
        "password" => {
            let keep = ssh.password.is_some()
                && Confirm::with_theme(theme)
                    .with_prompt("Keep the saved password?")
                    .default(true)
                    .interact()?;
            if !keep {
                ssh.password = Some(
                    Password::with_theme(theme)
                        .with_prompt("SSH password")
                        .interact()?,
                );
            }
        }
        "key" => {
            let current = ssh
                .private_key
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "~/.ssh/id_ed25519".to_string());
            let path: String = Input::with_theme(theme)
                .with_prompt("Private key path")
                .default(current)
                .interact_text()?;
            ssh.private_key = Some(path.into());
        }
        _ => {}
    }
    Ok(())
}

/// Offer to pre-pull OCI images so the first job doesn't wait on a multi-GB download, and so the agent knows each
/// image's OS up front.
fn pull_step(theme: &ColorfulTheme, config: &Config, local: &[String]) -> Result<()> {
    let mut images: Vec<&str> = Vec::new();
    for image in std::iter::once(&config.tart.base_image)
        .chain(config.tart.image_mappings.iter().map(|m| &m.image))
    {
        if !images.contains(&image.as_str()) && !local.contains(image) {
            images.push(image);
        }
    }
    let (refs, missing_local): (Vec<&str>, Vec<&str>) = images
        .into_iter()
        .partition(|i| host_checks::is_oci_image(i));
    for name in missing_local {
        println!("⚠ local image {name} doesn't exist yet, the agent won't start until it does");
    }
    if refs.is_empty() {
        return Ok(());
    }

    let picked = MultiSelect::with_theme(theme)
        .with_prompt("Pull these images now? (space toggles, macOS images are tens of GB)")
        .items(&refs)
        .defaults(&vec![true; refs.len()])
        .interact()?;
    for &i in &picked {
        let oci_ref = refs[i];
        println!("\n=== {oci_ref} ===");
        if run_inherited("tart", &["pull", oci_ref]).is_ok() {
            continue;
        }

        // most likely a private registry, log in once and retry
        let host = oci_ref.split('/').next().unwrap_or(oci_ref);
        if !Confirm::with_theme(theme)
            .with_prompt(format!("Pull failed. Log in to {host} and retry?"))
            .default(true)
            .interact()?
        {
            continue;
        }
        let username: String = Input::with_theme(theme)
            .with_prompt(format!("{host} username"))
            .interact_text()?;
        let token = Password::with_theme(theme)
            .with_prompt(format!("{host} token/password"))
            .interact()?;
        if let Err(e) = tart_login(host, &username, &token) {
            println!("✗ {e}");
            continue;
        }
        if let Err(e) = run_inherited("tart", &["pull", oci_ref]) {
            println!("✗ {e}");
        }
    }
    Ok(())
}

/// Pick a preset, an image `tart list` knows about, or type one in.
fn pick_image(
    theme: &ColorfulTheme,
    prompt: &str,
    local: &[String],
    current: Option<&String>,
) -> Result<String> {
    let mut choices: Vec<(String, String)> = Vec::new();
    if let Some(c) = current {
        choices.push((format!("{c}  (current)"), c.clone()));
    }
    for (name, oci_ref) in PRESET_IMAGES {
        if current.is_some_and(|c| c == oci_ref) {
            continue;
        }
        let pulled = if local.iter().any(|l| l == oci_ref) {
            ", pulled"
        } else {
            ""
        };
        choices.push((format!("{name}  [{oci_ref}{pulled}]"), oci_ref.to_string()));
    }
    for name in local {
        if current == Some(name) || PRESET_IMAGES.iter().any(|(_, r)| r == name) {
            continue;
        }
        choices.push((format!("{name}  (local)"), name.clone()));
    }

    let mut items: Vec<&str> = choices.iter().map(|(l, _)| l.as_str()).collect();
    items.push("Other…");
    let idx = Select::with_theme(theme)
        .with_prompt(prompt)
        .items(&items)
        .default(0)
        .interact()?;
    if let Some((_, image)) = choices.get(idx) {
        return Ok(image.clone());
    }
    Ok(Input::<String>::with_theme(theme)
        .with_prompt("Image (OCI ref or local VM name)")
        .interact_text()?
        .trim()
        .to_string())
}

fn describe_mapping(m: &config::ImageMapping) -> String {
    let pool = m.pool.map(|p| format!(" (pool {p})")).unwrap_or_default();
    format!("[{}] → {}{pool}", m.labels.join(", "), m.image)
}

/// Split on commas and/or whitespace, dropping empties and repeats.
fn parse_labels(s: &str) -> Vec<String> {
    let mut labels: Vec<String> = Vec::new();
    for l in s
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|l| !l.is_empty())
    {
        if !labels.iter().any(|x| x.eq_ignore_ascii_case(l)) {
            labels.push(l.to_string());
        }
    }
    labels
}

fn run_inherited(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("could not run `{program}`"))?;
    if !status.success() {
        bail!("`{program} {}` failed ({status})", args.join(" "));
    }
    Ok(())
}

/// Token goes in on stdin so it isn't visible in the process list. tart keeps it in the keychain for later pulls
fn tart_login(host: &str, username: &str, token: &str) -> Result<()> {
    use std::io::Write;

    let mut child = Command::new("tart")
        .args(["login", host, "--username", username, "--password-stdin"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .context("could not run `tart login`")?;

    // dropping stdin closes it, so tart sees EOF
    child
        .stdin
        .take()
        .context("no stdin for `tart login`")?
        .write_all(token.as_bytes())?;
    if !child.wait()?.success() {
        bail!("`tart login {host}` failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_labels() {
        assert_eq!(
            parse_labels("self-hosted, macOS,ARM64"),
            ["self-hosted", "macOS", "ARM64"]
        );
        assert_eq!(parse_labels("  a  b,,a "), ["a", "b"]);
        assert_eq!(parse_labels("macOS, macos"), ["macOS"]);
        assert!(parse_labels(" , ").is_empty());
    }
}
