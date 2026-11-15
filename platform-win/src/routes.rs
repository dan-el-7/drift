//! Stale default routes on Wi-Fi Direct subnets (failure catalogue F5).
//!
//! After Windows joins a phone's Wi-Fi Direct group as a P2P client it can leave
//! a static `0.0.0.0/0 → 192.168.49.1` on the Wi-Fi Direct virtual adapter. That
//! adapter's interface metric beats the real Wi-Fi, so the next time a group
//! comes up the laptop's internet (and any VPN tunnel) is routed into it. We only
//! ever touch default routes whose next hop is on a Wi-Fi Direct group subnet.

use std::process::Command;

use anyhow::{Result, bail};

/// Group subnets: Android GO (192.168.49.0/24) and Windows ICS/GO (192.168.137.0/24).
const P2P_PREFIXES: [&str; 2] = ["192.168.49.", "192.168.137."];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleRoute {
    pub interface: String,
    pub next_hop: String,
}

fn parse(out: &str) -> Vec<StaleRoute> {
    out.lines()
        .filter_map(|l| {
            let (iface, hop) = l.trim().split_once('|')?;
            P2P_PREFIXES.iter().any(|p| hop.starts_with(p)).then(|| StaleRoute { interface: iface.into(), next_hop: hop.into() })
        })
        .collect()
}

/// Default routes (active or persistent) pointing into a Wi-Fi Direct subnet.
pub fn find_stale() -> Result<Vec<StaleRoute>> {
    // Routes on ports that ignore default routes stay listed while the group is up but never carry
    // traffic (checked: 1.1.1.1 still goes out the real Wi-Fi), so they aren't a problem: skip them.
    let ps = "Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue | \
              Where-Object { (Get-NetIPInterface -InterfaceIndex $_.InterfaceIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue).IgnoreDefaultRoutes -ne 'Enabled' } | \
              ForEach-Object { $_.InterfaceAlias + '|' + $_.NextHop }";
    let out = { use std::os::windows::process::CommandExt; let mut c = Command::new("powershell"); c.creation_flags(0x0800_0000); c }.args(["-NoProfile", "-NonInteractive", "-Command", ps]).output()?;
    let mut routes = parse(&String::from_utf8_lossy(&out.stdout));
    routes.dedup();
    Ok(routes)
}

/// Remove them with one elevated call (UAC). Returns how many were found.
pub fn remove_stale() -> Result<usize> {
    // A group we just left takes its (DHCP) default route with it a moment later: only routes that
    // outlive the link are stale. Waiting here means no UAC prompt after an ordinary transfer.
    let mut stale = find_stale()?;
    // Windows keeps the P2P connection a little after we close it (seen: >11 s), so be patient.
    for _ in 0..40 {
        if stale.is_empty() {
            return Ok(0);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
        stale = find_stale()?;
    }
    if stale.is_empty() {
        return Ok(0);
    }
    let filter = P2P_PREFIXES.iter().map(|p| format!("$_.NextHop -like '{p}*'")).collect::<Vec<_>>().join(" -or ");
    let inner = format!(
        "foreach ($s in 'ActiveStore','PersistentStore') {{ Get-NetRoute -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -PolicyStore $s -ErrorAction SilentlyContinue | Where-Object {{ {filter} }} | Remove-NetRoute -Confirm:$false }}"
    );
    let ps = format!(
        "$p = Start-Process powershell -ArgumentList '-NoProfile','-Command','{}' -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",
        inner.replace('\'', "''")
    );
    tracing::info!(?stale, "removing stale Wi-Fi Direct default routes (UAC prompt)");
    let status = { use std::os::windows::process::CommandExt; let mut c = Command::new("powershell"); c.creation_flags(0x0800_0000); c }.args(["-NoProfile", "-NonInteractive", "-Command", &ps]).status()?;
    if !status.success() || !find_stale()?.is_empty() {
        bail!("stale routes were not removed (UAC declined?)");
    }
    Ok(stale.len())
}

/// Ports that are on a Wi-Fi Direct group right now (they hold a 192.168.49.x / 192.168.137.x address)
/// and would still take a default route from it. Only those: a port's role is known for sure only
/// while it is in a group, and touching any other port (a PC's main Wi-Fi while it's on Ethernet, say)
/// would break that network's internet later.
fn ports_taking_default_routes() -> Result<Vec<u32>> {
    let filter = P2P_PREFIXES.iter().map(|p| format!("$_.IPAddress -like '{p}*'")).collect::<Vec<_>>().join(" -or ");
    let ps = format!(
        "Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object {{ {filter} }} | ForEach-Object {{            $i = Get-NetIPInterface -InterfaceIndex $_.InterfaceIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue;            if ($i -and $i.IgnoreDefaultRoutes -eq 'Disabled') {{ $_.InterfaceIndex }} }}"
    );
    let out = { use std::os::windows::process::CommandExt; let mut c = Command::new("powershell"); c.creation_flags(0x0800_0000); c }.args(["-NoProfile", "-NonInteractive", "-Command", &ps]).output()?;
    let mut v: Vec<u32> = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.trim().parse().ok()).collect();
    v.dedup();
    Ok(v)
}

/// F5 at the root: make Windows ignore default routes handed out by Wi-Fi Direct groups (a phone's
/// group gives 192.168.49.1 as gateway, and the Wi-Fi Direct port's metric beats the real Wi-Fi, so
/// internet traffic and VPN tunnels would head into the phone). One UAC prompt per PC, then it sticks.
/// No-op when every port already has it. Returns how many ports were changed.
pub fn ensure_ignore_default_routes() -> Result<usize> {
    let ports = ports_taking_default_routes()?;
    if ports.is_empty() {
        return Ok(0);
    }
    let list = ports.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(",");
    let inner = format!(
        "foreach ($i in @({list})) {{ Set-NetIPInterface -InterfaceIndex $i -AddressFamily IPv4 -IgnoreDefaultRoutes Enabled -ErrorAction SilentlyContinue;          Get-NetRoute -InterfaceIndex $i -AddressFamily IPv4 -DestinationPrefix '0.0.0.0/0' -ErrorAction SilentlyContinue | Remove-NetRoute -Confirm:$false -ErrorAction SilentlyContinue }}"
    );
    let ps = format!(
        "$p = Start-Process powershell -ArgumentList '-NoProfile','-Command','{}' -Verb RunAs -WindowStyle Hidden -Wait -PassThru; exit $p.ExitCode",
        inner.replace('\'', "''")
    );
    tracing::info!(?ports, "Wi-Fi Direct ports would take default routes from groups; fixing (UAC prompt)");
    let status = { use std::os::windows::process::CommandExt; let mut c = Command::new("powershell"); c.creation_flags(0x0800_0000); c }.args(["-NoProfile", "-NonInteractive", "-Command", &ps]).status()?;
    if !status.success() || !ports_taking_default_routes()?.is_empty() {
        bail!("couldn't set IgnoreDefaultRoutes (UAC declined?)");
    }
    remember_guarded(&ports);
    Ok(ports.len())
}

/// The ports we changed, so uninstalling Drift can put them back (ui/windows/Shell/Cleanup.cs reads it).
pub const GUARD_FILE: &str = "route-guard.txt";

fn remember_guarded(ports: &[u32]) {
    let Some(dir) = std::env::var_os("APPDATA").map(|a| std::path::PathBuf::from(a).join("Drift")) else { return };
    let path = dir.join(GUARD_FILE);
    let mut have: Vec<String> = std::fs::read_to_string(&path).unwrap_or_default().lines().map(String::from).collect();
    for p in ports {
        if !have.contains(&p.to_string()) {
            have.push(p.to_string());
        }
    }
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(&path, have.join("
") + "
");
}

/// The next hop Windows would use to reach the internet right now (no PowerShell: GetBestRoute2).
pub fn internet_gateway() -> Option<std::net::Ipv4Addr> {
    use windows::Win32::NetworkManagement::IpHelper::{GetBestRoute2, MIB_IPFORWARD_ROW2};
    use windows::Win32::Networking::WinSock::{AF_INET, SOCKADDR_INET};
    unsafe {
        let mut dest: SOCKADDR_INET = std::mem::zeroed();
        dest.Ipv4.sin_family = AF_INET;
        dest.Ipv4.sin_addr.S_un.S_addr = u32::from_ne_bytes([1, 1, 1, 1]);
        let mut row: MIB_IPFORWARD_ROW2 = std::mem::zeroed();
        let mut src: SOCKADDR_INET = std::mem::zeroed();
        if GetBestRoute2(None, 0, None, &dest, 0, &mut row, &mut src).is_err() {
            return None;
        }
        let hop = std::net::Ipv4Addr::from(row.NextHop.Ipv4.sin_addr.S_un.S_addr.to_ne_bytes());
        (!hop.is_unspecified()).then_some(hop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_internet_gateway() {
        // on any connected PC this is the router; just check it's sane
        if let Some(g) = internet_gateway() {
            assert!(!g.is_loopback());
        }
    }

    #[test]
    fn only_p2p_next_hops_are_stale() {
        let out = "WiFi|10.0.0.1\r\nWiFi 2|192.168.49.1\r\nWiFi 3|192.168.137.1\r\nEthernet|192.168.1.1\r\n";
        assert_eq!(
            parse(out),
            vec![
                StaleRoute { interface: "WiFi 2".into(), next_hop: "192.168.49.1".into() },
                StaleRoute { interface: "WiFi 3".into(), next_hop: "192.168.137.1".into() },
            ]
        );
    }
}
