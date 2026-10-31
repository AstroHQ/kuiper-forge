//! Runs the deb/rpm maintainer scripts against stub system commands to check
//! what they do to the systemd service on install, upgrade and removal.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

const UNIT: &str = "kuiper-proxmox-agent.service";

fn script(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("packaging/scripts")
        .join(name)
}

/// Runs `name` with `args` under `sh`, with systemctl and the user/dir setup
/// commands stubbed out, and returns the systemctl invocations it made.
fn run(name: &str, args: &[&str]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("systemctl.log");
    let stub = |cmd: &str, body: &str| {
        let path = dir.path().join(cmd);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    };
    stub("systemctl", &format!("echo \"$*\" >> '{}'", log.display()));
    // Pretend the kuiper user/group already exist and skip creating dirs.
    for cmd in ["getent", "install", "groupadd", "useradd"] {
        stub(cmd, "exit 0");
    }

    let path = format!(
        "{}:{}",
        dir.path().display(),
        std::env::var("PATH").unwrap()
    );
    let out = Command::new("sh")
        .arg(script(name))
        .args(args)
        .env("PATH", path)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{name} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
fn postinstall_restarts_running_service_on_upgrade() {
    // dpkg: `configure <old-version>`; rpm: `2` (an upgrade).
    for args in [&["configure", "0.4.0"][..], &["2"]] {
        let calls = run("postinstall.sh", args);
        assert_eq!(
            calls,
            ["daemon-reload".to_owned(), format!("try-restart {UNIT}")],
            "postinstall {args:?}"
        );
    }
}

#[test]
fn postinstall_never_starts_or_enables_on_fresh_install() {
    // dpkg: `configure` with no old version; rpm: `1` (first install).
    for args in [&["configure"][..], &["1"]] {
        let calls = run("postinstall.sh", args);
        assert!(
            calls
                .iter()
                .all(|c| c == "daemon-reload" || c.starts_with("try-restart ")),
            "postinstall {args:?} made {calls:?}"
        );
    }
}

#[test]
fn preremove_leaves_service_alone_on_upgrade() {
    // dpkg: `upgrade <new-version>`; rpm: `1` (a package remains).
    for args in [&["upgrade", "0.6.0"][..], &["1"]] {
        assert!(run("preremove.sh", args).is_empty(), "preremove {args:?}");
    }
}

#[test]
fn preremove_stops_and_disables_on_removal() {
    for args in [&["remove"][..], &["purge"], &["0"]] {
        assert_eq!(
            run("preremove.sh", args),
            [format!("disable --now {UNIT}")],
            "preremove {args:?}"
        );
    }
}
