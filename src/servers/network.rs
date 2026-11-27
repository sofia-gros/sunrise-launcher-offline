//! Addresses to share with players. Choosing one does not change the server's bind
//! address, open router ports or claim that an internet connection has been tested.
use crate::i18n::tr;
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

/// Minecraft omits its default port; IPv6 literals need brackets with a port.
pub fn address(ip: IpAddr, port: u16) -> String {
    match (ip, port) {
        (IpAddr::V4(ip), 25565) => ip.to_string(),
        (IpAddr::V4(ip), port) => format!("{ip}:{port}"),
        (IpAddr::V6(ip), port) => format!("[{ip}]:{port}"),
    }
}

/// Reads the listener reported by the most recent completed engine startup.
pub(super) fn listener_from_startup(lines: &[String]) -> Option<std::net::SocketAddr> {
    let start = lines.iter().rposition(|line| {
        line.split_once("] [Supervisor/INFO]: ")
            .is_some_and(|(_, msg)| {
                msg.starts_with("Starting ") && msg.contains(" with the game files in ")
            })
    })?;
    let startup = &lines[start + 1..];
    let done = startup.iter().position(|line| startup_done(line))?;
    let announcement = startup[..done].iter().find_map(|line| {
        line.split_once("] [Server thread/INFO]: ")?
            .1
            .strip_prefix("Starting Minecraft server on ")
    })?;
    let (host, port) = announcement.rsplit_once(':')?;
    let port: u16 = port.parse().ok()?;
    let ip: IpAddr = match host {
        "*" => Ipv4Addr::UNSPECIFIED.into(),
        host => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse()
            .ok()?,
    };
    (port != 0).then(|| std::net::SocketAddr::new(ip.to_canonical(), port))
}

pub(super) fn startup_done(line: &str) -> bool {
    line.split_once("] [Server thread/INFO]: ")
        .is_some_and(|(_, message)| message.starts_with("Done ("))
}

/// Called only when the owner presses the detection button. ipify sees the
/// request's source IP; the lookup uses a dedicated agent without a proxy.
pub fn public_ipv4() -> Result<Ipv4Addr, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(8))
        .redirects(0)
        .try_proxy_from_env(false)
        .build();
    lookup(&agent, "https://api.ipify.org")
}

fn lookup(agent: &ureq::Agent, url: &str) -> Result<Ipv4Addr, String> {
    let response = agent
        .get(url)
        .call()
        .map_err(|_| tr("Public IP lookup failed").to_string())?;
    if response.status() != 200 {
        return Err(tr("Public IP lookup did not return HTTP 200").into());
    }
    read_public_ipv4(response.into_reader())
}

fn read_public_ipv4(reader: impl Read) -> Result<Ipv4Addr, String> {
    let mut body = String::new();
    reader
        .take(65)
        .read_to_string(&mut body)
        .map_err(|_| tr("Could not read the public IP address").to_string())?;
    if body.len() > 64 {
        return Err(tr("Invalid public IP response").into());
    }
    let ip: Ipv4Addr = body
        .trim()
        .parse()
        .map_err(|_| tr("Invalid public IP response").to_string())?;
    let [a, b, c, d] = ip.octets();
    if ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_documentation()
        || a == 0
        || a >= 240
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0 && !matches!(d, 9 | 10))
        || [a, b, c] == [192, 88, 99]
        || (a == 198 && (18..=19).contains(&b))
    {
        return Err(tr("The lookup did not return a public IPv4 address").into());
    }
    Ok(ip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_keep_nondefault_ports_and_bracket_ipv6() {
        assert_eq!(
            address("192.168.2.152".parse().unwrap(), 25565),
            "192.168.2.152"
        );
        assert_eq!(
            address("192.168.2.152".parse().unwrap(), 25566),
            "192.168.2.152:25566"
        );
        assert_eq!(
            address("2001:db8::1".parse().unwrap(), 25565),
            "[2001:db8::1]:25565"
        );
    }

    #[test]
    fn only_a_finished_current_engine_start_supplies_an_address() {
        let started =
            "[10:00:00] [Supervisor/INFO]: Starting engine with the game files in resources";
        let done = "[10:00:02] [Server thread/INFO]: Done (2s)!";
        let old_address = "[10:00:01] [Server thread/INFO]: Starting Minecraft server on *:25565";
        let new_address = "[10:00:01] [Server thread/INFO]: Starting Minecraft server on ::1:25567";
        let read = |lines: &[&str]| {
            listener_from_startup(
                &lines
                    .iter()
                    .map(|line| line.to_string())
                    .collect::<Vec<_>>(),
            )
        };
        assert_eq!(
            read(&[started, old_address, done]),
            Some("0.0.0.0:25565".parse().unwrap())
        );
        assert_eq!(
            read(&[started, old_address, done, started, new_address, done]),
            Some("[::1]:25567".parse().unwrap())
        );
        assert_eq!(
            read(&[started, old_address, done, started, new_address]),
            None
        );
        assert_eq!(read(&[new_address, done]), None);
        assert_eq!(
            read(&[
                started,
                "[10:00:01] [Server thread/INFO]: Starting Minecraft server on *:0",
                done
            ]),
            None
        );
    }

    #[test]
    fn lookup_rejects_redirect_even_with_a_valid_ipv4_body() {
        use std::io::{BufRead, BufReader, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let reply = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            for line in BufReader::new(&mut socket).lines() {
                if line.unwrap().is_empty() {
                    break;
                }
            }
            socket.write_all(
                b"HTTP/1.1 302 Found\r\nLocation: /next\r\nContent-Length: 7\r\nConnection: close\r\n\r\n8.8.8.8",
            ).unwrap();
        });
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .try_proxy_from_env(false)
            .timeout(Duration::from_secs(5))
            .build();
        assert_eq!(
            lookup(&agent, &url).unwrap_err(),
            "Public IP lookup did not return HTTP 200"
        );
        reply.join().unwrap();
    }

    #[test]
    fn lookup_accepts_plain_ipv4_and_trailing_newlines() {
        assert_eq!(
            read_public_ipv4(&b" 8.8.8.8\n"[..]).unwrap(),
            Ipv4Addr::new(8, 8, 8, 8)
        );
    }

    #[test]
    fn lookup_refuses_unusable_or_malformed_addresses() {
        for body in [
            "",
            "<html>error</html>",
            "8.8.8.8:25565",
            "::1",
            "192.168.2.152",
            "127.0.0.1",
            "100.64.0.1",
            "169.254.1.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "192.0.0.8",
            "192.88.99.2",
            "198.51.100.1",
            "198.18.0.1",
        ] {
            assert!(read_public_ipv4(body.as_bytes()).is_err(), "{body}");
        }
        assert!(read_public_ipv4(format!("{}8.8.8.8", " ".repeat(65)).as_bytes()).is_err());
        assert!(read_public_ipv4(&[0xff][..]).is_err());
    }
}
