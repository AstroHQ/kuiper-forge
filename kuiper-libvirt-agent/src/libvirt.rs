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
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

use crate::config::{DiskBus, Firmware, GuestOs, Hardware, IpSource, Machine};
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

/// Domain settings for one VM.
pub struct DomainSpec<'a> {
    pub name: &'a str,
    pub disk: &'a Path,
    pub vcpus: u32,
    pub memory_mib: u32,
    pub network: &'a str,
    pub bridge: Option<&'a str>,
    pub hardware: &'a Hardware,
    /// The image's `domain_template`, read in. The domain is generated from `hardware` without one
    pub template: Option<&'a str>,
}

/// Sizes of OVMF's variable store in its 2 MB and 4 MB builds.
const VARS_SIZES: [usize; 2] = [128 * 1024, 528 * 1024];

/// The domain template for `hardware`, placeholders and all: KVM, a virtio NIC, a guest agent channel, serial
/// console, plus what `hardware` asks for.
pub fn domain_template(hardware: &Hardware) -> String {
    let windows = hardware.os == GuestOs::Windows;
    let uefi = hardware.firmware == Firmware::Uefi;

    let machine = match hardware.machine {
        Some(Machine::Pc) => " machine='pc'",
        Some(Machine::Q35) => " machine='q35'",
        None => "",
    };
    let mut os = format!("    <type{machine}>hvm</type>\n");
    if uefi {
        let secure_boot = if hardware.secure_boot { "yes" } else { "no" };
        os.push_str(&format!(
            "    <firmware>\n      <feature enabled='{secure_boot}' name='secure-boot'/>\n"
        ));
        // with uefi_vars the agent writes the VM's variable store itself, and its keys are what they are
        if hardware.secure_boot && hardware.uefi_vars.is_none() {
            os.push_str("      <feature enabled='yes' name='enrolled-keys'/>\n");
        }
        os.push_str("    </firmware>\n    <nvram>{nvram}</nvram>\n");
    }

    let mut features = String::from("    <acpi/>\n    <apic/>\n");
    if windows {
        features.push_str(
            "    <hyperv mode='custom'>
      <relaxed state='on'/>
      <vapic state='on'/>
      <spinlocks state='on' retries='8191'/>
      <vpindex state='on'/>
      <synic state='on'/>
      <stimer state='on'/>
      <frequencies state='on'/>
    </hyperv>
",
        );
    }

    // Windows keeps the RTC in local time
    let clock = if windows {
        "<clock offset='localtime'>\n    <timer name='hypervclock' present='yes'/>\n  </clock>"
    } else {
        "<clock offset='utc'/>"
    };

    let disk = match hardware.disk_bus {
        DiskBus::Virtio => {
            "    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2' discard='unmap'/>
      <source file='{disk}'/>
      <target dev='vda' bus='virtio'/>
    </disk>
"
        }
        DiskBus::Scsi => {
            "    <controller type='scsi' model='virtio-scsi'/>
    <disk type='file' device='disk'>
      <driver name='qemu' type='qcow2' discard='unmap'/>
      <source file='{disk}'/>
      <target dev='sda' bus='scsi'/>
    </disk>
"
        }
    };
    let tpm = if hardware.tpm {
        "    <tpm model='tpm-crb'>\n      <backend type='emulator' version='2.0'/>\n    </tpm>\n"
    } else {
        ""
    };

    let firmware = if uefi { " firmware='efi'" } else { "" };
    format!(
        "<domain type='kvm'>
  <name>{{name}}</name>
  <metadata>
    {{metadata}}
  </metadata>
  <memory unit='MiB'>{{memory_mib}}</memory>
  <vcpu>{{vcpus}}</vcpu>
  <os{firmware}>
{os}  </os>
  <features>
{features}  </features>
  <cpu mode='host-passthrough'/>
  {clock}
  <on_poweroff>destroy</on_poweroff>
  <on_reboot>restart</on_reboot>
  <on_crash>destroy</on_crash>
  <devices>
{disk}    {{interface}}
    <channel type='unix'>
      <target type='virtio' name='org.qemu.guest_agent.0'/>
    </channel>
    <serial type='pty'/>
    <console type='pty'/>
{tpm}    <rng model='virtio'>
      <backend model='random'>/dev/urandom</backend>
    </rng>
    <memballoon model='virtio'/>
  </devices>
</domain>
"
    )
}

/// Whether a custom domain template puts `{metadata}` inside `<metadata>`. Without it a VM runs fine, but the
/// agent can't tell it's its own and won't destroy it.
pub fn check_template(template: &str) -> std::result::Result<(), String> {
    let inside = template.find("{metadata}").is_some_and(|at| {
        let (before, after) = template.split_at(at);
        before
            .rfind("<metadata>")
            .is_some_and(|open| !before[open..].contains("</metadata>"))
            && after.contains("</metadata>")
    });
    if inside {
        Ok(())
    } else {
        Err("it needs {metadata} inside <metadata>...</metadata>, that's how the agent knows its VMs".to_string())
    }
}

/// `vars` without the zeros it was padded with. A Proxmox EFI disk is a whole LVM extent or more, of which OVMF
/// uses the first 128 or 528 KiB (Proxmox tells QEMU the size), and QEMU sizes the flash from the file.
fn trim_vars(mut vars: Vec<u8>) -> Vec<u8> {
    if let Some(&size) = VARS_SIZES
        .iter()
        .find(|&&size| vars.len() > size && vars[size..].iter().all(|&b| b == 0))
    {
        vars.truncate(size);
    }
    vars
}

/// libvirt connection plus where the agent keeps its VM files.
pub struct Libvirt {
    uri: String,
    disk_dir: PathBuf,
}

impl Libvirt {
    pub fn new(uri: String, disk_dir: PathBuf) -> Self {
        Self { uri, disk_dir }
    }

    /// The overlay disk for VM `name`.
    pub fn disk_path(&self, name: &str) -> PathBuf {
        self.disk_dir.join(format!("{name}{DISK_SUFFIX}"))
    }

    fn xml_path(&self, name: &str) -> PathBuf {
        self.disk_dir.join(format!("{name}{XML_SUFFIX}"))
    }

    /// UEFI variable store for VM `name`. libvirt creates it from the firmware's template on start unless the
    /// agent already wrote it from `uefi_vars`; transient domains don't take it with them, so the agent deletes it.
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

    /// The domain XML for `spec`: its template filled in, or the one generated from its hardware.
    pub fn domain_xml(&self, spec: &DomainSpec<'_>) -> String {
        let generated;
        let template = match spec.template {
            Some(template) => template,
            None => {
                generated = domain_template(spec.hardware);
                &generated
            }
        };
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
        template
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
        if let Some(vars) = &spec.hardware.uefi_vars {
            self.write_vars(spec.name, vars).await?;
        }
        let xml_path = self.xml_path(spec.name);
        tokio::fs::write(&xml_path, self.domain_xml(spec)).await?;
        let output = self.virsh(&["create"], Some(&xml_path)).await?;
        check("virsh create", output)?;

        // without its marker the agent would take the domain for someone else's and never stop it. It was
        // created just now under a name libvirt didn't have, so it's this agent's to stop regardless
        if self.owns_domain(spec.name).await? != Some(true) {
            let output = self.virsh(&["destroy", spec.name], None).await?;
            check("virsh destroy", output)?;
            return Err(Error::Libvirt(format!(
                "domain {} came up without the agent's marker in <metadata>, stopped it",
                spec.name
            )));
        }
        info!("Started domain {}", spec.name);
        Ok(())
    }

    /// Start VM `name`'s UEFI variable store as a copy of `vars`. Never overwrites, like the overlay.
    async fn write_vars(&self, name: &str, vars: &Path) -> Result<()> {
        let contents = tokio::fs::read(vars).await.map_err(|e| {
            Error::Libvirt(format!("failed to read uefi_vars {}: {e}", vars.display()))
        })?;
        let path = self.nvram_path(name);
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await?;
        file.write_all(&trim_vars(contents)).await?;
        file.flush().await?;
        debug!("Wrote {} from {}", path.display(), vars.display());
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

    /// Stop VM `name` if it's one of this agent's, and delete its files. The files only go once the domain is
    /// gone: if it couldn't be destroyed, or can't be checked, QEMU may still be running on them. A domain with
    /// that name that isn't the agent's is left alone and reported as an error, and the files stay too; the
    /// startup sweep removes files with no domain of the agent's.
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
        if let Err(e) = &result {
            warn!(
                "Keeping the files of VM {}, it may still be running: {}",
                name, e
            );
            return result;
        }

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
        Ok(())
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
        )
    }

    fn spec<'a>(disk: &'a Path, bridge: Option<&'a str>, hardware: &'a Hardware) -> DomainSpec<'a> {
        DomainSpec {
            name: "kuiper-runner-1",
            disk,
            vcpus: 4,
            memory_mib: 8192,
            network: "default",
            bridge,
            hardware,
            template: None,
        }
    }

    /// Windows exported from Proxmox: i440fx, UEFI with its own variable store, virtio-scsi.
    fn windows() -> Hardware {
        Hardware {
            os: GuestOs::Windows,
            firmware: Firmware::Uefi,
            uefi_vars: Some(PathBuf::from("/var/lib/libvirt/images/win11_VARS.fd")),
            machine: Some(Machine::Pc),
            disk_bus: DiskBus::Scsi,
            ..Hardware::default()
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
        let xml = libvirt.domain_xml(&spec(&disk, None, &Hardware::default()));
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
    fn test_domain_xml_default_is_bios_virtio() {
        let xml = domain_template(&Hardware::default());
        assert!(!xml.contains("firmware") && !xml.contains("nvram") && !xml.contains("machine="));
        assert!(xml.contains("<target dev='vda' bus='virtio'/>"));
        assert!(xml.contains("<clock offset='utc'/>"));
        assert!(!xml.contains("hyperv") && !xml.contains("<tpm"));
    }

    #[test]
    fn test_domain_xml_windows() {
        let libvirt = libvirt();
        let disk = libvirt.disk_path("kuiper-runner-1");
        let hardware = windows();
        let xml = libvirt.domain_xml(&spec(&disk, None, &hardware));
        assert!(xml.contains("<os firmware='efi'>"));
        assert!(xml.contains("<type machine='pc'>hvm</type>"));
        assert!(xml.contains("<feature enabled='no' name='secure-boot'/>"));
        assert!(!xml.contains("enrolled-keys"));
        assert!(xml.contains(
            "<nvram>/var/lib/kuiper-libvirt-agent/vms/kuiper-runner-1.kuiper.VARS.fd</nvram>"
        ));
        assert!(xml.contains("<hyperv mode='custom'>"));
        assert!(xml.contains("<clock offset='localtime'>"));
        assert!(xml.contains("<controller type='scsi' model='virtio-scsi'/>"));
        assert!(xml.contains("<target dev='sda' bus='scsi'/>"));
        assert!(!xml.contains('{'), "unfilled placeholder:\n{xml}");
        assert!(libvirt.is_ours(&xml));
    }

    #[test]
    fn test_domain_xml_secure_boot_tpm() {
        let hardware = Hardware {
            firmware: Firmware::Uefi,
            secure_boot: true,
            machine: Some(Machine::Q35),
            tpm: true,
            ..Hardware::default()
        };
        let xml = domain_template(&hardware);
        assert!(xml.contains("<feature enabled='yes' name='secure-boot'/>"));
        // no uefi_vars, so start with Microsoft's keys
        assert!(xml.contains("<feature enabled='yes' name='enrolled-keys'/>"));
        assert!(xml.contains("<backend type='emulator' version='2.0'/>"));
    }

    #[test]
    fn test_domain_xml_template_wins() {
        let libvirt = libvirt();
        let disk = libvirt.disk_path("kuiper-runner-1");
        let hardware = Hardware::default();
        let mut spec = spec(&disk, None, &hardware);
        spec.template = Some("<domain><name>{name}</name><nvram>{nvram}</nvram></domain>");
        assert_eq!(
            libvirt.domain_xml(&spec),
            "<domain><name>kuiper-runner-1</name><nvram>/var/lib/kuiper-libvirt-agent/vms/kuiper-runner-1.kuiper.VARS.fd</nvram></domain>"
        );
    }

    #[test]
    fn test_check_template() {
        assert!(check_template(&domain_template(&Hardware::default())).is_ok());
        assert!(check_template("<domain><metadata>{metadata}</metadata></domain>").is_ok());
        for bad in [
            "<domain><name>{name}</name></domain>",
            "<domain>{metadata}<metadata></metadata></domain>",
            "<domain><metadata></metadata>{metadata}</domain>",
            "<domain><metadata>{metadata}</domain>",
        ] {
            assert!(check_template(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn test_trim_vars() {
        // a 4 MB build's store padded out to a 4 MiB Proxmox EFI disk
        let mut vars = vec![0xff; 528 * 1024];
        vars.resize(4 * 1024 * 1024, 0);
        assert_eq!(trim_vars(vars).len(), 528 * 1024);
        // a 2 MB build's
        let mut vars = vec![0xff; 128 * 1024];
        vars.resize(4 * 1024 * 1024, 0);
        assert_eq!(trim_vars(vars).len(), 128 * 1024);
        // already the right size, or not padding, stays as is
        assert_eq!(trim_vars(vec![0xff; 528 * 1024]).len(), 528 * 1024);
        assert_eq!(
            trim_vars(vec![0xff; 4 * 1024 * 1024]).len(),
            4 * 1024 * 1024
        );
    }

    #[test]
    fn test_domain_xml_bridge() {
        let libvirt = libvirt();
        let disk = libvirt.disk_path("kuiper-runner-1");
        let xml = libvirt.domain_xml(&spec(&disk, Some("br0"), &Hardware::default()));
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

    /// A domain that can't be destroyed, or checked, may still be running on its files.
    #[tokio::test]
    #[ignore = "needs virsh on the host"]
    async fn test_destroy_failure_keeps_files() {
        let dir = std::env::temp_dir().join(format!("kuiper-libvirt-keep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // virsh can't connect, so it can't tell whether the domain is gone
        let libvirt = Libvirt::new("bogus:///kuiper".to_string(), dir.clone());
        let name = "kuiper-runner-1";
        let files = [
            libvirt.disk_path(name),
            libvirt.xml_path(name),
            libvirt.nvram_path(name),
        ];
        for file in &files {
            std::fs::write(file, b"").unwrap();
        }

        assert!(libvirt.destroy(name).await.is_err());
        for file in &files {
            assert!(file.exists(), "{} was removed", file.display());
        }

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Runs the real virsh/qemu-img sequence against `qemu:///session` (your user's libvirt, not the system's): a
    /// blank base image, an overlay, a transient UEFI domain with its own variable store and a TPM, user-mode
    /// networking and TCG, then destroy and sweep. Needs OVMF and swtpm.
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

        // a Proxmox-style EFI disk: the firmware's store padded out with zeros
        let vars = dir.join("base_VARS.fd");
        let mut contents = std::fs::read("/usr/share/OVMF/OVMF_VARS_4M.ms.fd").unwrap();
        let size = contents.len();
        contents.resize(4 * 1024 * 1024, 0);
        std::fs::write(&vars, contents).unwrap();
        let hardware = Hardware {
            uefi_vars: Some(vars),
            tpm: true,
            ..windows()
        };

        // no libvirt network in a session, and maybe no /dev/kvm
        let template = domain_template(&hardware)
            .replace("<domain type='kvm'>", "<domain type='qemu'>")
            .replace("<cpu mode='host-passthrough'/>", "")
            .replace(
                "{interface}",
                "<interface type='user'><model type='virtio'/></interface>",
            );
        let libvirt = Libvirt::new("qemu:///session".to_string(), dir.clone());
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
            hardware: &hardware,
            template: Some(&template),
        };
        libvirt.start_domain(&spec).await.unwrap();
        assert_eq!(libvirt.owns_domain(&name).await.unwrap(), Some(true));
        let nvram = libvirt.nvram_path(&name);
        assert_eq!(std::fs::metadata(&nvram).unwrap().len() as usize, size);
        // no OS, so no address
        assert_eq!(
            libvirt.domain_ip(&name, IpSource::Lease).await.unwrap(),
            None
        );

        // another agent (different disk dir) won't touch it
        let other = Libvirt::new("qemu:///session".to_string(), dir.join("other"));
        assert_eq!(other.owns_domain(&name).await.unwrap(), Some(false));
        assert!(other.destroy(&name).await.is_err());
        assert_eq!(libvirt.owns_domain(&name).await.unwrap(), Some(true));

        libvirt.destroy(&name).await.unwrap();
        assert_eq!(libvirt.owns_domain(&name).await.unwrap(), None);
        assert!(!disk.exists());
        assert!(!nvram.exists());
        // destroying again is fine
        libvirt.destroy(&name).await.unwrap();

        // a crashed run's leftovers go, other files stay
        libvirt.create_overlay(&name, &base).await.unwrap();
        libvirt.start_domain(&spec).await.unwrap();
        libvirt.sweep_leftovers().await.unwrap();
        assert_eq!(libvirt.owns_domain(&name).await.unwrap(), None);
        assert!(!disk.exists());
        assert!(!nvram.exists());
        assert!(base.exists());

        // a template that loses the marker: the VM is stopped rather than left running unclaimed
        let unmarked = template.replace("{metadata}", "");
        let spec = DomainSpec {
            template: Some(&unmarked),
            ..spec
        };
        libvirt.create_overlay(&name, &base).await.unwrap();
        let err = libvirt.start_domain(&spec).await.unwrap_err().to_string();
        assert!(err.contains("marker"), "{err}");
        assert_eq!(libvirt.owns_domain(&name).await.unwrap(), None);
        libvirt.destroy(&name).await.unwrap();
        assert!(!disk.exists());

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
