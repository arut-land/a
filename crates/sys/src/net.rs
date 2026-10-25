//! Socket options above what `std` sets.

use std::io;
use std::net::TcpStream;
use std::time::Duration;

/// Has the kernel probe an idle connection, so a peer that vanished
/// (a sleeping laptop's NAT, a dropped route) fails the read waiting on
/// it in about two minutes instead of never. The probes are the kernel's:
/// the process does not wake for them.
pub fn keep_alive(socket: &TcpStream) -> io::Result<()> {
    keep_alive_with(socket, IDLE, INTERVAL, PROBES)
}

const IDLE: Duration = Duration::from_secs(60);
const INTERVAL: Duration = Duration::from_secs(10);
const PROBES: u32 = 5;

#[cfg(unix)]
fn keep_alive_with(
    socket: &TcpStream,
    idle: Duration,
    interval: Duration,
    probes: u32,
) -> io::Result<()> {
    use rustix::net::sockopt;
    sockopt::set_socket_keepalive(socket, true)?;
    sockopt::set_tcp_keepidle(socket, idle)?;
    sockopt::set_tcp_keepintvl(socket, interval)?;
    sockopt::set_tcp_keepcnt(socket, probes)?;
    Ok(())
}

#[cfg(windows)]
fn keep_alive_with(
    socket: &TcpStream,
    idle: Duration,
    interval: Duration,
    probes: u32,
) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket as _;
    use windows_sys::Win32::Networking::WinSock::{
        IPPROTO_TCP, SO_KEEPALIVE, SOCKET, SOL_SOCKET, TCP_KEEPCNT, TCP_KEEPIDLE, TCP_KEEPINTVL,
        WSAGetLastError, setsockopt,
    };
    let raw = SOCKET::try_from(socket.as_raw_socket()).expect("socket fits the platform handle");
    let seconds = |d: Duration| u32::try_from(d.as_secs()).unwrap_or(u32::MAX);
    // TCP_KEEPIDLE and friends take seconds (Windows 10 1709 and later).
    for (level, name, value) in [
        (SOL_SOCKET, SO_KEEPALIVE, 1),
        (IPPROTO_TCP, TCP_KEEPIDLE, seconds(idle)),
        (IPPROTO_TCP, TCP_KEEPINTVL, seconds(interval)),
        (IPPROTO_TCP, TCP_KEEPCNT, probes),
    ] {
        // SAFETY: `raw` is a live socket borrowed for the call; the value
        // is a u32 the call reads `size_of::<u32>()` bytes of.
        let failed = unsafe {
            setsockopt(
                raw,
                level,
                name,
                (&raw const value).cast(),
                i32::try_from(size_of::<u32>()).expect("u32 size fits i32"),
            )
        } != 0;
        if failed {
            // SAFETY: reads the calling thread's last socket error.
            return Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg_attr(miri, ignore = "Miri does not support TCP keep-alive options")]
    fn keep_alive_turns_probing_on() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        keep_alive(&socket).unwrap();
        assert!(probing(&socket));
    }

    #[cfg(unix)]
    fn probing(socket: &TcpStream) -> bool {
        rustix::net::sockopt::socket_keepalive(socket).unwrap()
    }

    #[cfg(windows)]
    fn probing(socket: &TcpStream) -> bool {
        use std::os::windows::io::AsRawSocket as _;
        use windows_sys::Win32::Networking::WinSock::{
            SO_KEEPALIVE, SOCKET, SOL_SOCKET, getsockopt,
        };
        let mut value = 0u32;
        let mut len = i32::try_from(size_of::<u32>()).expect("u32 size fits i32");
        // SAFETY: `value` and `len` outlive the call and describe a u32.
        let ok = unsafe {
            getsockopt(
                SOCKET::try_from(socket.as_raw_socket()).expect("socket fits the platform handle"),
                SOL_SOCKET,
                SO_KEEPALIVE,
                (&raw mut value).cast(),
                &raw mut len,
            )
        } == 0;
        ok && value != 0
    }
}
