//! Whether the local backend can be reached from other machines.
//!
//! Ollama has no authentication, so one listening beyond loopback serves
//! anyone who can reach the machine, and so does a llama.cpp or vLLM server
//! started without an API key.  exnode only ever talks to the backend on
//! loopback; anything wider is exposure the provider didn't need.  Two
//! signals are read: `OLLAMA_HOST` in exnode's own environment, and the
//! kernel's table of listening TCP sockets (`/proc/net/tcp`, `tcp6`) for the
//! backend's port.  This module uses only `std`.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// Loopback, including an IPv4-mapped loopback address (`::ffff:127.0.0.1`).
pub fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback()),
    }
}

/// Whether an `OLLAMA_HOST` value makes Ollama listen beyond loopback:
/// `0.0.0.0`, `:11434` (no host: every interface), `[::]:11434`, a LAN
/// address or a host name other than `localhost`.  Empty is Ollama's
/// default, `127.0.0.1:11434`.
pub fn ollama_host_is_exposed(value: &str) -> bool {
    let s = value.trim();
    if s.is_empty() {
        return false;
    }
    let s = s.split_once("://").map_or(s, |(_, rest)| rest);
    let s = s.split('/').next().unwrap_or("");
    let host = if let Some(rest) = s.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else if s.matches(':').count() > 1 {
        s // a bare IPv6 address such as "::"
    } else {
        s.rsplit_once(':').map_or(s, |(h, _)| h)
    };
    if host.eq_ignore_ascii_case("localhost") {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => !is_loopback(ip),
        Err(_) => true, // empty (every interface) or a name that resolves who knows where
    }
}

/// The listening sockets in a `/proc/net/tcp` or `/proc/net/tcp6` table.
pub fn listening(table: &str) -> Vec<SocketAddr> {
    table.lines()
        .skip(1)
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            // fields: sl local_address rem_address st …; st 0A is TCP_LISTEN
            (f.len() > 3 && f[3] == "0A").then(|| parse_proc_addr(f[1])).flatten()
        })
        .collect()
}

/// `0100007F:2CAA` (IPv4) or 32 hex digits and a port (IPv6).  The kernel
/// prints each 32-bit word of the address as a native-endian integer.
fn parse_proc_addr(s: &str) -> Option<SocketAddr> {
    let (ip, port) = s.split_once(':')?;
    if !ip.is_ascii() {
        return None;
    }
    let port = u16::from_str_radix(port, 16).ok()?;
    let word = |i: usize| u32::from_str_radix(ip.get(i * 8..i * 8 + 8)?, 16).ok().map(u32::to_ne_bytes);
    let ip: IpAddr = match ip.len() {
        8 => Ipv4Addr::from(word(0)?).into(),
        32 => {
            let mut b = [0u8; 16];
            for i in 0..4 {
                b[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            Ipv6Addr::from(b).into()
        }
        _ => return None,
    };
    Some(SocketAddr::new(ip, port))
}

/// The sockets listening on `port` that other machines can reach.
pub fn exposed_on(listeners: &[SocketAddr], port: u16) -> Vec<SocketAddr> {
    listeners.iter().filter(|a| a.port() == port && !is_loopback(a.ip())).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_host_values() {
        for safe in ["127.0.0.1", "127.0.0.1:11434", "http://127.0.0.1:11434", "localhost:11434",
                     "[::1]:11434", "::1", "http://[::1]:11434/", "127.0.0.2", "", "  "] {
            assert!(!ollama_host_is_exposed(safe), "{safe}");
        }
        for open in ["0.0.0.0", "0.0.0.0:11434", "http://0.0.0.0:11434", ":11434", "::", "[::]:11434",
                     "192.168.1.20:11434", "https://ollama.lan:443", "myhost"] {
            assert!(ollama_host_is_exposed(open), "{open}");
        }
    }

    #[test]
    #[cfg(target_endian = "little")]
    fn proc_net_tables_are_read_for_listening_sockets() {
        let tcp = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n\
           0: 0100007F:2CAA 00000000:0000 0A 00000000:00000000 00:00000000 00000000   998        0 1 1 0 100 0 0 10 0\n\
           1: 00000000:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000     0        0 2 1 0 100 0 0 10 0\n\
           2: 0100007F:2CAA 0100007F:D3C2 01 00000000:00000000 00:00000000 00000000   998        0 3 1 0 20 4 30 10 -1\n";
        assert_eq!(listening(tcp), vec![
            "127.0.0.1:11434".parse::<SocketAddr>().unwrap(),
            "0.0.0.0:8080".parse().unwrap(),
        ]);
        let tcp6 = "  sl  local_address                         remote_address                        st\n\
           0: 00000000000000000000000001000000:2CAA 00000000000000000000000000000000:0000 0A\n\
           1: 00000000000000000000000000000000:2CAA 00000000000000000000000000000000:0000 0A\n\
           2: 0000000000000000FFFF00000100007F:1F90 00000000000000000000000000000000:0000 0A\n";
        let l6 = listening(tcp6);
        assert_eq!(l6, vec![
            "[::1]:11434".parse::<SocketAddr>().unwrap(),
            "[::]:11434".parse().unwrap(),
            "[::ffff:127.0.0.1]:8080".parse().unwrap(),
        ]);
        assert_eq!(exposed_on(&l6, 11434), vec!["[::]:11434".parse::<SocketAddr>().unwrap()]);
        assert!(exposed_on(&l6, 8080).is_empty(), "an IPv4-mapped loopback is loopback");
        assert!(exposed_on(&listening(tcp), 11434).is_empty());
        assert_eq!(exposed_on(&listening(tcp), 8080).len(), 1);
        assert!(listening("header\n 0: zz:zz 0 0A\n 1: é0000000:0001 0 0A\n").is_empty());
    }
}
