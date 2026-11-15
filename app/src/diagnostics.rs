//! Environment checks that the app reports in plain language (failure
//! catalogue F11, F15). They never change anything on the user's system.

use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetHealth {
    Ok,
    /// TCP to a public IP works but names don't resolve: typically a VPN's DNS
    /// proxy stuck after network churn (seen with Cloudflare WARP).
    DnsBroken,
    /// No internet at all (not our concern to fix).
    Offline,
}

/// Pure classification, unit-tested.
pub fn classify(dns_ok: bool, tcp_ok: bool) -> NetHealth {
    match (dns_ok, tcp_ok) {
        (true, _) => NetHealth::Ok,
        (false, true) => NetHealth::DnsBroken,
        (false, false) => NetHealth::Offline,
    }
}

/// Blocking probe (run on a blocking thread): resolve a well-known name and
/// open a TCP connection to a well-known IP.
pub fn check_net() -> NetHealth {
    let dns_ok = ("dns.google", 443).to_socket_addrs().map(|mut a| a.next().is_some()).unwrap_or(false);
    let tcp_ok = TcpStream::connect_timeout(&SocketAddr::from(([1, 1, 1, 1], 443)), Duration::from_secs(3)).is_ok();
    classify(dns_ok, tcp_ok)
}

/// Is Google's Quick Share for Windows running? It registers the same RFCOMM
/// service and BLE advertising, so Drift's Bluetooth path can't work alongside it.
pub fn quick_share_running() -> bool {
    #[cfg(windows)]
    {
        { use std::os::windows::process::CommandExt; let mut c = std::process::Command::new("tasklist"); c.creation_flags(0x0800_0000); c }
            .args(["/FI", "IMAGENAME eq nearby_share.exe", "/NH"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_lowercase().contains("nearby_share.exe"))
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification() {
        assert_eq!(classify(true, true), NetHealth::Ok);
        assert_eq!(classify(true, false), NetHealth::Ok);
        assert_eq!(classify(false, true), NetHealth::DnsBroken);
        assert_eq!(classify(false, false), NetHealth::Offline);
    }
}
