//! Showing the overlay on another PC in the same home network: the radio is plugged into
//! the gaming PC, OBS runs on the streaming PC and opens `http://GAMING-PC.local:7878/`.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

use serde::Serialize;
use tokio::net::TcpListener;

/// How other PCs reach this one.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Address {
    /// This PC's name with `.local` (`http://NAME.local:7878/`), which keeps working when
    /// the router hands out a new IP address. Windows, Macs, Linux and phones all find
    /// `.local` names; the bare name only works between Windows PCs.
    pub name: Option<String>,
    /// Its IP address on the home network, for when the name isn't found.
    pub ip: Option<IpAddr>,
}

pub fn address() -> Address {
    Address {
        name: host_name().map(|name| {
            // a name with a domain already (a Mac's NAME.local) is used as it is
            if name.contains('.') {
                name
            } else {
                format!("{name}.local")
            }
        }),
        ip: lan_ip(),
    }
}

pub fn host_name() -> Option<String> {
    let name = gethostname::gethostname().into_string().ok()?;
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_owned())
}

/// The names a request for this PC may use: its name, the name without a domain, and
/// that with `.local` (how Macs and Linux find Windows PCs), in lower case.
pub fn names() -> Vec<String> {
    let Some(name) = host_name() else {
        return Vec::new();
    };
    let name = name.to_ascii_lowercase();
    let short = name.split('.').next().unwrap_or(&name).to_owned();
    let mut names = vec![format!("{short}.local"), short, name];
    names.sort();
    names.dedup();
    names
}

/// The address this PC uses on the home network: the one it would send from to reach
/// the internet. Nothing is sent; connecting a UDP socket only picks the route.
pub fn lan_ip() -> Option<IpAddr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?; // a documentation address
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
}

/// Listens on `port` (0 picks a free one): on 127.0.0.1 only, or with `shared` on every
/// address, so other PCs can connect.
pub async fn listen(port: u16, shared: bool) -> io::Result<TcpListener> {
    if !shared {
        return TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await;
    }
    // IPv6 and IPv4 on one socket: a PC's name often finds its IPv6 address first.
    match dual_stack(port) {
        Err(e) if e.kind() != io::ErrorKind::AddrInUse => {
            // IPv6 turned off
            TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).await
        }
        bound => bound,
    }
}

fn dual_stack(port: u16) -> io::Result<TcpListener> {
    use socket2::{Domain, Protocol, Socket, Type};
    let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
    socket.set_only_v6(false)?;
    // as the standard library does for listeners (on Windows it would let others take
    // the port)
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    socket.bind(&SocketAddr::from((Ipv6Addr::UNSPECIFIED, port)).into())?;
    socket.listen(1024)?;
    socket.set_nonblocking(true)?;
    TcpListener::from_std(socket.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_include_the_short_name_and_dot_local() {
        let names = names();
        if let Some(name) = host_name() {
            let short = name.split('.').next().unwrap().to_ascii_lowercase();
            assert!(names.contains(&short), "{names:?}");
            assert!(names.contains(&format!("{short}.local")), "{names:?}");
            assert!(names.iter().all(|n| *n == n.to_ascii_lowercase()));
        }
    }

    #[tokio::test]
    async fn shared_listens_on_every_address_and_switches_back() {
        let shared = listen(0, true).await.unwrap();
        let port = shared.local_addr().unwrap().port();
        assert!(shared.local_addr().unwrap().ip().is_unspecified());
        // IPv4 loopback reaches the shared socket too
        std::net::TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        drop(shared);
        let local = listen(port, false).await.unwrap();
        assert_eq!(
            local.local_addr().unwrap(),
            (Ipv4Addr::LOCALHOST, port).into()
        );
    }
}
