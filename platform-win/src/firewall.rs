//! Inbound firewall rule for Direct mode.
//!
//! The Wi-Fi Direct group's virtual adapter has no network profile, so Windows
//! treats it as Public and blocks inbound connections. We add one narrowly
//! scoped allow rule (TCP, Public profile, the two group subnets only, Drift's fixed
//! hosted ports), elevating once through a UAC prompt. The rule names ports, not
//! the exe path, so it sticks across updates and builds: one prompt per PC.
//! See CLAUDE.md "Firewall (decided)".

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};

pub const RULE_NAME: &str = "Drift (Wi-Fi Direct)";
/// Our hosted group (Windows GO) and a Drift phone's group (Android GO), where the phone connects in.
pub const GROUP_SUBNETS: &str = "192.168.137.0/24,192.168.49.0/24";

static ENSURED: AtomicBool = AtomicBool::new(false);

fn ports() -> String {
    let r = drift_net::upgrade::HOSTED_PORTS;
    format!("{}-{}", r.start(), r.end())
}

/// True if our rule exists in its current form (the hosted ports).
pub fn rule_exists() -> Result<bool> {
    let out = { use std::os::windows::process::CommandExt; let mut c = Command::new("netsh"); c.creation_flags(0x0800_0000); c }
        .args(["advfirewall", "firewall", "show", "rule", &format!("name={RULE_NAME}"), "verbose"])
        .output()?;
    if !out.status.success() {
        return Ok(false);
    }
    let text = String::from_utf8_lossy(&out.stdout).to_lowercase();
    Ok(text.contains(&ports()) && text.contains("192.168.137.0") && text.contains("192.168.49.0"))
}

/// Add the rule if missing. Shows one UAC prompt; cached for the process.
pub fn ensure() -> Result<()> {
    if ENSURED.load(Ordering::Relaxed) {
        return Ok(());
    }
    if rule_exists()? {
        ENSURED.store(true, Ordering::Relaxed);
        return Ok(());
    }
    // One elevated cmd: drop any older Drift rule (e.g. the per-exe one), then add ours.
    let cmdline = format!(
        "/c netsh advfirewall firewall delete rule name=\"{RULE_NAME}\" >nul & \
         netsh advfirewall firewall add rule name=\"{RULE_NAME}\" dir=in action=allow protocol=TCP localport={} profile=public remoteip={GROUP_SUBNETS}",
        ports()
    );
    // PowerShell single-quoted literal: escape ' exactly once.
    let ps = format!(
        "$p = Start-Process -FilePath cmd -ArgumentList '{}' -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",
        cmdline.replace('\'', "''")
    );
    tracing::info!("adding firewall rule {RULE_NAME:?} (UAC prompt)");
    let status = { use std::os::windows::process::CommandExt; let mut c = Command::new("powershell"); c.creation_flags(0x0800_0000); c }.args(["-NoProfile", "-NonInteractive", "-Command", &ps]).status()?;
    if !status.success() || !rule_exists()? {
        bail!("firewall rule was not added (UAC declined?)");
    }
    ENSURED.store(true, Ordering::Relaxed);
    Ok(())
}
