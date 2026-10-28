//! libvirt and qemu-img through their CLIs: disk overlays, transient domains, IP lookup.
//!
//! Every domain the agent creates carries a `<kuiper:runner>` element in its `<metadata>`, naming the agent's
//! disk directory. The agent only ever destroys domains that carry its own, so a runner named like one of the
//! host's other VMs, or a stray DestroyRunner, can't touch them.

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use serde::Deserialize;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::IpSource;
use crate::error::{Error, Result};

/// Timeout for virsh and qemu-img commands, so a stuck libvirtd can't hang the agent.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// The agent's files in the disk directory end in these, and it only ever sweeps files that do: the directory
/// may hold other things, base images even.
const DISK_SUFFIX: &str = ".kuiper.qcow2";
const XML_SUFFIX: &str = ".kuiper.xml";
const NVRAM_SUFFIX: &str = ".kuiper.VARS.fd";

/// XML namespace of the ownership marker.
const MARKER_NS: &str = "https://github.com/AstroHQ/kuiper-forge/libvirt-agent";

/// The built-in domain: KVM, virtio disk and NIC, a guest agent channel, serial console.
const DOMAIN_TEMPLATE: &str = r#"<domain type='kvm'>
  <name>{name}</name>
  <metadata>
    {metadata}
  </metadata>
  <memory unit='MiB'>{memory_mib}</memory>
  <vcpu>{vcpus}</vcpu>
  <os>
    <type>hvm</type>
  </os>
  <features>
    <acpi/>
  </features>
  <cpu mode='host-passthrough'/>
  <clock offset='utc'/>
  <on_poweroff>destroy</on_poweroff>
  <on_reboot>restart</on_reboot>
  <on_crash>destroy</on_crash>
  <devices>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2' discard='unmap'/>
      <source file='{disk}'/>
      <target dev='vda' bus='virtio'/>
    </disk>
    {interface}
    <channel type='unix'>
      <target type='virtio' name='org.qemu.guest_agent.0'/>
    </channel>
    <serial type='pty'/>
    <console type='pty'/>
    <rng model='virtio'>
      <backend model='random'>/dev/urandom</backend>
    </rng>
    <memballoon model='virtio'/>
  </devices>
</domain>
"#;

/// Domain settings for one VM.
pub struct DomainSpec<'a> {
    pub name: &'a str,
    pub disk: &'a Path,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub network: &'a str,
    pub bridge: Option<&'a str>,
}

/// libvirt connection plus where the agent keeps its VM files.
pub struct Libvirt {
    uri: String,
    disk_dir: PathBuf,
    /// Domain XML template, `{...}` placeholders filled in per VM
    template: String,
}

impl Libvirt {
    pub fn new(uri: String, disk_dir: PathBuf, template: Option<String>) -> Self {
        Self {
            uri,
            disk_dir,
            template: template.unwrap_or_else(|| DOMAIN_TEMPLATE.to_string()),
        }
    }

    /// The overlay disk for VM `name`.
    pub fn disk_path(&self, name: &str) -> PathBuf {
        self.disk_dir.join(format!("{name}{DISK_SUFFIX}"))
    }

    fn xml_path(&self, name: &str) -> PathBuf {
        self.disk_dir.join(format!("{name}{XML_SUFFIX}"))
    }

    /// UEFI variable store for VM `name`, for templates that use `{nvram}`. libvirt creates it from the
    /// `<nvram template=...>` on start; transient domains don't take it with them, so the agent deletes it.
    fn nvram_path(&self, name: &str) -> PathBuf {
        self.disk_dir.join(format!("{name}{NVRAM_SUFFIX}"))
    }

    /// Create a copy-on-write overlay of `image` for VM `name`. Fails if the overlay already exists, qemu-img
    /// would otherwise overwrite it.
    pub async fn create_overlay(&self, name: &str, image: &Path) -> Result<PathBuf> {
        let disk = self.disk_path(name);
        if tokio::fs::try_exists(&disk).await? {
            return Err(Error::Libvirt(format!(
                "disk {} already exists",
                disk.display()
            )));
        }
        let format = image_format(image).await?;
        let output = run(Command::new("qemu-img")
            .args(["create", "-f", "qcow2", "-F", &format, "-b"])
            .arg(image)
            .arg(&disk))
        .await?;
        check("qemu-img create", output)?;
        info!("Created overlay {} on {}", disk.display(), image.display());
        Ok(disk)
    }

    /// Fill in the domain template for `spec`.
    pub fn domain_xml(&self, spec: &DomainSpec<'_>) -> String {
        let interface = match spec.bridge {
            Some(bridge) => format!(
                "<interface type='bridge'><source bridge='{}'/><model type='virtio'/></interface>",
                xml_escape(bridge)
            ),
            None => format!(
                "<interface type='network'><source network='{}'/><model type='virtio'/></interface>",
                xml_escape(spec.network)
            ),
        };
        let metadata = format!(
            "<kuiper:runner xmlns:kuiper='{MARKER_NS}' disk-dir='{}'/>",
            xml_escape(&self.disk_dir.display().to_string())
        );
        self.template
            .replace("{name}", &xml_escape(spec.name))
            .replace("{disk}", &xml_escape(&spec.disk.display().to_string()))
            .replace(
                "{nvram}",
                &xml_escape(&self.nvram_path(spec.name).display().to_string()),
            )
            .replace("{vcpus}", &spec.vcpus.to_string())
            .replace("{memory_mib}", &spec.memory_mib.to_string())
            .replace("{interface}", &interface)
            .replace("{metadata}", &metadata)
    }

    /// Start a transient domain for `spec`. It's gone from libvirt once it's destroyed or shuts down.
    pub async fn start_domain(&self, spec: &DomainSpec<'_>) -> Result<()> {
        let xml_path = self.xml_path(spec.name);
        tokio::fs::write(&xml_path, self.domain_xml(spec)).await?;
        let output = self.virsh(&["create"], Some(&xml_path)).await?;
        check("virsh create", output)?;
        info!("Started domain {}", spec.name);
        Ok(())
    }

    /// The first non-loopback IPv4 address of domain `name`, if it has one yet.
    pub async fn domain_ip(&self, name: &str, source: IpSource) -> Result<Option<IpAddr>> {
        let sources: &[&str] = match source {
            IpSource::Lease => &["lease"],
            IpSource::Agent => &["agent"],
            IpSource::Any => &["lease", "agent"],
        };
        for source in sources {
            let output = self
                .virsh(&["domifaddr", name, "--source", source], None)
                .await?;
            if !output.status.success() {
                // the guest agent isn't up yet, say
                debug!(
                    "domifaddr --source {} for {} failed: {}",
                    source,
                    name,
                    String::from_utf8_lossy(&output.stderr).trim()
                );
                continue;
            }
            if let Some(ip) = parse_domifaddr(&String::from_utf8_lossy(&output.stdout)) {
                return Ok(Some(ip));
            }
        }
        Ok(None)
    }

    /// Whether domain `name` exists and is one of this agent's. `None` if libvirt has no such domain.
    pub async fn owns_domain(&self, name: &str) -> Result<Option<bool>> {
        let output = self.virsh(&["dumpxml", name], None).await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if is_not_found(&stderr) {
                return Ok(None);
            }
            return Err(Error::Libvirt(format!(
                "virsh dumpxml {name} failed: {}",
                stderr.trim()
            )));
        }
        Ok(Some(self.is_ours(&String::from_utf8_lossy(&output.stdout))))
    }

    /// Whether domain XML has this agent's marker. libvirt may hand it back with either quote style.
    fn is_ours(&self, xml: &str) -> bool {
        let dir = xml_escape(&self.disk_dir.display().to_string());
        xml.contains(MARKER_NS)
            && (xml.contains(&format!("disk-dir='{dir}'"))
                || xml.contains(&format!("disk-dir=\"{dir}\"")))
    }

    /// Stop VM `name` if it's one of this agent's, and delete its files. A domain with that name that isn't the
    /// agent's is left alone and reported as an error, the files (in the agent's disk directory) still go.
    pub async fn destroy(&self, name: &str) -> Result<()> {
        let result = match self.owns_domain(name).await {
            Ok(Some(true)) => {
                let output = self.virsh(&["destroy", name], None).await?;
                if output.status.success() {
                    info!("Destroyed domain {}", name);
                    Ok(())
                } else {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    if is_not_found(&stderr) {
                        Ok(())
                    } else {
                        Err(Error::Libvirt(format!(
                            "virsh destroy {name} failed: {}",
                            stderr.trim()
                        )))
                    }
                }
            }
            Ok(Some(false)) => Err(Error::Libvirt(format!(
                "domain {name} isn't one of this agent's VMs, not destroying it"
            ))),
            Ok(None) => {
                debug!("Domain {} is already gone", name);
                Ok(())
            }
            Err(e) => Err(e),
        };

        for path in [
            self.disk_path(name),
            self.xml_path(name),
            self.nvram_path(name),
        ] {
            match tokio::fs::remove_file(&path).await {
                Ok(()) => debug!("Removed {}", path.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warn!("Failed to remove {}: {}", path.display(), e),
            }
        }
        result
    }

    /// Destroy this agent's domains and files left over from an earlier run (it crashed, or the host rebooted).
    pub async fn sweep_leftovers(&self) -> Result<()> {
        let output = self.virsh(&["list", "--name"], None).await?;
        let names = check("virsh list", output)?;
        for name in names.lines().map(str::trim).filter(|n| !n.is_empty()) {
            if self.owns_domain(name).await? == Some(true) {
                warn!("Destroying leftover VM {} from an earlier run", name);
                self.destroy(name).await?;
            }
        }

        // files with no domain: the domain died with the host, or creating it failed
        let mut entries = tokio::fs::read_dir(&self.disk_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if [DISK_SUFFIX, XML_SUFFIX, NVRAM_SUFFIX]
                .iter()
                .any(|suffix| file_name.ends_with(suffix))
            {
                warn!("Removing leftover VM file {}", path.display());
                if let Err(e) = tokio::fs::remove_file(&path).await {
                    warn!("Failed to remove {}: {}", path.display(), e);
                }
            }
        }
        Ok(())
    }

    async fn virsh(&self, args: &[&str], file: Option<&Path>) -> Result<Output> {
        let mut cmd = Command::new("virsh");
        cmd.args(["-c", &self.uri]).args(args);
        if let Some(file) = file {
            cmd.arg(file);
        }
        run(&mut cmd).await
    }
}

#[derive(Deserialize)]
struct ImageInfo {
    format: String,
}

/// Disk format of `image` ("qcow2", "raw", ...), for the overlay's backing format.
async fn image_format(image: &Path) -> Result<String> {
    let output = run(Command::new("qemu-img")
        .args(["info", "--output=json"])
        .arg(image))
    .await?;
    let json = check("qemu-img info", output)?;
    let info: ImageInfo = serde_json::from_str(&json)
        .map_err(|e| Error::Libvirt(format!("bad qemu-img info output: {e}")))?;
    Ok(info.format)
}

async fn run(cmd: &mut Command) -> Result<Output> {
    let program = cmd.as_std().get_program().to_string_lossy().to_string();
    timeout(COMMAND_TIMEOUT, cmd.output())
        .await
        .map_err(|_| Error::Libvirt(format!("{program} timed out")))?
        .map_err(|e| Error::Libvirt(format!("failed to run {program}: {e}")))
}

/// stdout of a successful command, else an error with its stderr.
fn check(what: &str, output: Output) -> Result<String> {
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(Error::Libvirt(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )))
    }
}

fn is_not_found(stderr: &str) -> bool {
    stderr.contains("failed to get domain") || stderr.contains("Domain not found")
}

/// The first non-loopback IPv4 address in `virsh domifaddr` output:
///
/// ```text
///  Name       MAC address          Protocol     Address
/// -------------------------------------------------------------------------------
///  vnet0      52:54:00:4b:73:6b    ipv4         192.168.122.94/24
/// ```
fn parse_domifaddr(output: &str) -> Option<IpAddr> {
    output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace().rev();
            let address = fields.next()?;
            let protocol = fields.next()?;
            (protocol == "ipv4").then_some(address)
        })
        .filter_map(|address| {
            let ip = address.split('/').next()?;
            ip.parse::<Ipv4Addr>().ok()
        })
        .find(|ip| !ip.is_loopback() && !ip.is_link_local())
        .map(IpAddr::V4)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('\'', "&apos;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn libvirt() -> Libvirt {
        Libvirt::new(
            "qemu:///system".to_string(),
            PathBuf::from("/var/lib/kuiper-libvirt-agent/vms"),
            None,
        )
    }

    fn spec<'a>(disk: &'a Path, bridge: Option<&'a str>) -> DomainSpec<'a> {
        DomainSpec {
            name: "kuiper-runner-1",
            disk,
            vcpus: 4,
            memory_mib: 8192,
            network: "default",
            bridge,
        }
    }

    #[test]
    fn test_parse_domifaddr_lease() {
        let output = " Name       MAC address          Protocol     Address
-------------------------------------------------------------------------------
 vnet0      52:54:00:4b:73:6b    ipv4         192.168.122.94/24
";
        assert_eq!(
            parse_domifaddr(output),
            Some(IpAddr::from([192, 168, 122, 94]))
        );
    }

    #[test]
    fn test_parse_domifaddr_agent_skips_loopback_and_ipv6() {
        let output = " Name       MAC address          Protocol     Address
-------------------------------------------------------------------------------
 lo         00:00:00:00:00:00    ipv4         127.0.0.1/8
 -          -                    ipv6         ::1/128
 enp1s0     52:54:00:4b:73:6b    ipv6         fe80::5054:ff:fe4b:736b/64
 -          -                    ipv4         10.0.0.17/24
";
        assert_eq!(parse_domifaddr(output), Some(IpAddr::from([10, 0, 0, 17])));
    }

    #[test]
    fn test_parse_domifaddr_empty() {
        let output = " Name       MAC address          Protocol     Address
-------------------------------------------------------------------------------
";
        assert_eq!(parse_domifaddr(output), None);
    }

    #[test]
    fn test_domain_xml_network() {
        let libvirt = libvirt();
        let disk = libvirt.disk_path("kuiper-runner-1");
        let xml = libvirt.domain_xml(&spec(&disk, None));
        assert!(xml.contains("<name>kuiper-runner-1</name>"));
        assert!(xml.contains(
            "<source file='/var/lib/kuiper-libvirt-agent/vms/kuiper-runner-1.kuiper.qcow2'/>"
        ));
        assert!(xml.contains("<source network='default'/>"));
        assert!(xml.contains("<memory unit='MiB'>8192</memory>"));
        assert!(!xml.contains('{'), "unfilled placeholder:\n{xml}");
        // the agent recognizes its own domain
        assert!(libvirt.is_ours(&xml));
    }

    #[test]
    fn test_domain_xml_win11_example() {
        let template = include_str!("../../examples/kuiper-libvirt-domain-win11.xml");
        let libvirt = Libvirt::new(
            "qemu:///system".to_string(),
            PathBuf::from("/var/lib/kuiper-libvirt-agent/vms"),
            Some(template.to_string()),
        );
        let disk = libvirt.disk_path("kuiper-runner-1");
        let xml = libvirt.domain_xml(&spec(&disk, None));
        assert!(
            xml.contains(
                ">/var/lib/kuiper-libvirt-agent/vms/kuiper-runner-1.kuiper.VARS.fd</nvram>"
            )
        );
        assert!(
            !xml.contains("{name}") && !xml.contains("{nvram}") && !xml.contains("{interface}")
        );
        assert!(libvirt.is_ours(&xml));
    }

    #[test]
    fn test_domain_xml_bridge() {
        let libvirt = libvirt();
        let disk = libvirt.disk_path("kuiper-runner-1");
        let xml = libvirt.domain_xml(&spec(&disk, Some("br0")));
        assert!(xml.contains("<interface type='bridge'><source bridge='br0'/>"));
        assert!(!xml.contains("type='network'"));
    }

    #[test]
    fn test_is_ours() {
        let libvirt = libvirt();
        // libvirt hands metadata back with double quotes
        let ours = format!(
            "<domain><metadata><kuiper:runner xmlns:kuiper=\"{MARKER_NS}\" disk-dir=\"/var/lib/kuiper-libvirt-agent/vms\"/></metadata></domain>"
        );
        assert!(libvirt.is_ours(&ours));

        // another agent's disk dir
        let other = ours.replace("/var/lib/kuiper-libvirt-agent/vms", "/srv/other");
        assert!(!libvirt.is_ours(&other));

        // someone's own VM
        assert!(!libvirt.is_ours("<domain><name>minikube</name></domain>"));
    }

    #[test]
    fn test_xml_escape() {
        assert_eq!(xml_escape("a<b>&'\""), "a&lt;b&gt;&amp;&apos;&quot;");
    }

    /// Runs the real virsh/qemu-img sequence against `qemu:///session` (your user's libvirt, not the system's): a
    /// blank base image, an overlay, a transient domain with user-mode networking and TCG, then destroy and sweep.
    /// `cargo test -p kuiper-libvirt-agent -- --ignored`
    #[tokio::test]
    #[ignore = "needs libvirt and qemu-img on the host"]
    async fn test_session_lifecycle() {
        let dir = std::env::temp_dir().join(format!("kuiper-libvirt-it-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let base = dir.join("base.qcow2");
        let status = std::process::Command::new("qemu-img")
            .args(["create", "-q", "-f", "qcow2"])
            .arg(&base)
            .arg("64M")
            .status()
            .unwrap();
        assert!(status.success());

        // no libvirt network in a session, and maybe no /dev/kvm
        let template = DOMAIN_TEMPLATE
            .replace("<domain type='kvm'>", "<domain type='qemu'>")
            .replace("<cpu mode='host-passthrough'/>", "")
            .replace(
                "{interface}",
                "<interface type='user'><model type='virtio'/></interface>",
            );
        let libvirt = Libvirt::new("qemu:///session".to_string(), dir.clone(), Some(template));
        let name = format!("kuiper-it-{}", std::process::id());
        let disk = libvirt.create_overlay(&name, &base).await.unwrap();
        assert!(disk.exists());
        // never overwrites
        assert!(libvirt.create_overlay(&name, &base).await.is_err());

        let spec = DomainSpec {
            name: &name,
            disk: &disk,
            vcpus: 1,
            memory_mib: 128,
            network: "unused",
            bridge: None,
        };
        libvirt.start_domain(&spec).await.unwrap();
        assert_eq!(libvirt.owns_domain(&name).await.unwrap(), Some(true));
        // no OS, so no address
        assert_eq!(
            libvirt.domain_ip(&name, IpSource::Lease).await.unwrap(),
            None
        );

        // another agent (different disk dir) won't touch it
        let other = Libvirt::new("qemu:///session".to_string(), dir.join("other"), None);
        assert_eq!(other.owns_domain(&name).await.unwrap(), Some(false));
        assert!(other.destroy(&name).await.is_err());
        assert_eq!(libvirt.owns_domain(&name).await.unwrap(), Some(true));

        libvirt.destroy(&name).await.unwrap();
        assert_eq!(libvirt.owns_domain(&name).await.unwrap(), None);
        assert!(!disk.exists());
        // destroying again is fine
        libvirt.destroy(&name).await.unwrap();

        // a crashed run's leftovers go, other files stay
        libvirt.create_overlay(&name, &base).await.unwrap();
        libvirt.start_domain(&spec).await.unwrap();
        libvirt.sweep_leftovers().await.unwrap();
        assert_eq!(libvirt.owns_domain(&name).await.unwrap(), None);
        assert!(!disk.exists());
        assert!(base.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
