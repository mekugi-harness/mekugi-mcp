use std::env;
use std::time::Duration;

#[cfg(unix)]
use std::ffi::OsStr;
#[cfg(unix)]
use std::os::unix::net::UnixDatagram;
#[cfg(unix)]
use tracing::warn;

/// Send a systemd sd_notify state message to `NOTIFY_SOCKET`, if configured.
/// Returns `true` if a notification datagram was sent.
pub fn notify(state: &str) -> bool {
    #[cfg(unix)]
    {
        let Some(socket_addr) = env::var_os("NOTIFY_SOCKET") else {
            return false;
        };
        if socket_addr.is_empty() {
            return false;
        }
        notify_socket(&socket_addr, state)
    }
    #[cfg(not(unix))]
    {
        let _ = state;
        false
    }
}

#[cfg(unix)]
fn notify_socket(socket_addr: &OsStr, state: &str) -> bool {
    let Ok(sock) = UnixDatagram::unbound() else {
        warn!("Failed to create UnixDatagram socket for sd_notify");
        return false;
    };

    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::net::SocketAddr;

        let bytes = socket_addr.as_bytes();
        if let Some(abstract_name) = bytes.strip_prefix(b"@") {
            match SocketAddr::from_abstract_name(abstract_name) {
                Ok(addr) => match sock.send_to_addr(state.as_bytes(), &addr) {
                    Ok(_) => return true,
                    Err(e) => {
                        warn!("Failed to send sd_notify to abstract socket: {e}");
                        return false;
                    }
                },
                Err(e) => {
                    warn!("Invalid abstract NOTIFY_SOCKET: {e}");
                    return false;
                }
            }
        }
    }

    match sock.send_to(state.as_bytes(), socket_addr) {
        Ok(_) => true,
        Err(e) => {
            warn!("Failed to send sd_notify to {:?}: {e}", socket_addr);
            false
        }
    }
}

/// Return half of `WATCHDOG_USEC` when systemd watchdog is enabled for this process.
pub fn watchdog_interval() -> Option<Duration> {
    let usec = env::var("WATCHDOG_USEC").ok();
    let pid = env::var("WATCHDOG_PID").ok();
    parse_watchdog_interval(usec.as_deref(), pid.as_deref(), std::process::id())
}

fn parse_watchdog_interval(
    usec: Option<&str>,
    pid: Option<&str>,
    current_pid: u32,
) -> Option<Duration> {
    if let Some(pid_str) = pid.map(str::trim).filter(|s| !s.is_empty()) {
        let target_pid = pid_str.parse::<u32>().ok()?;
        if target_pid != current_pid {
            return None;
        }
    }
    let usec_val = usec?.trim().parse::<u64>().ok()?;
    if usec_val == 0 {
        return None;
    }
    Some(Duration::from_micros((usec_val / 2).max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_watchdog_interval_handles_valid_and_invalid_inputs() {
        assert_eq!(parse_watchdog_interval(None, None, 1234), None);
        assert_eq!(parse_watchdog_interval(Some("0"), None, 1234), None);
        assert_eq!(parse_watchdog_interval(Some("abc"), None, 1234), None);
        assert_eq!(
            parse_watchdog_interval(Some("30000000"), None, 1234),
            Some(Duration::from_secs(15))
        );
        assert_eq!(
            parse_watchdog_interval(Some("30000000"), Some("1234"), 1234),
            Some(Duration::from_secs(15))
        );
        assert_eq!(
            parse_watchdog_interval(Some("30000000"), Some("9999"), 1234),
            None
        );
        assert_eq!(
            parse_watchdog_interval(Some("30000000"), Some("not-a-pid"), 1234),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn notify_socket_sends_payload_to_unix_datagram() {
        let dir = tempfile::TempDir::new().unwrap();
        let sock_path = dir.path().join("notify.sock");
        let server = UnixDatagram::bind(&sock_path).unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();

        assert!(notify_socket(sock_path.as_os_str(), "READY=1\nSTATUS=ok"));

        let mut buf = [0u8; 256];
        let n = server.recv(&mut buf).unwrap();
        assert_eq!(&buf[..n], b"READY=1\nSTATUS=ok");
    }

    #[cfg(unix)]
    #[test]
    fn notify_socket_returns_false_for_missing_path() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist.sock");
        assert!(!notify_socket(missing.as_os_str(), "READY=1"));
    }
}
