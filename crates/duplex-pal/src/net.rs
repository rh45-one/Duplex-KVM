//! Cross-Platform DSCP QoS Socket Layer & Strict Interface Binding (`duplex-pal::net`).
//!
//! Enforces two mandatory safety invariants:
//! 1. Sockets NEVER bind to `0.0.0.0` (`Ipv4Addr::UNSPECIFIED`); they bind strictly to the
//!    dedicated point-to-point interface (`192.168.137.1` on Windows, `192.168.137.2` on Debian).
//! 2. Every UDP and TCP socket is tagged at the kernel IP layer with the ECN-shifted DSCP `IP_TOS`
//!    byte (`dscp << 2`):
//!    - HID Stream (UDP 48100): DSCP `EF` (`0x2E`) -> `IP_TOS = 0xB8` (`SO_PRIORITY = 6`)
//!    - Control & 50 Hz Heartbeat (TCP/UDP 48101): DSCP `CS6` (`0x30`) -> `IP_TOS = 0xC0` (`SO_PRIORITY = 7`)
//!    - Video Streams (UDP 48102-48103): DSCP `AF41` (`0x22`) -> `IP_TOS = 0x88` (`SO_PRIORITY = 4`)

use crate::PalError;
use duplex_proto::{
    dscp_to_tos, DSCP_AF41, DSCP_CS6, DSCP_EF, TOS_CONTROL_CS6, TOS_HID_EF, TOS_VIDEO_AF41,
};
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream, UdpSocket};
use tracing::{debug, warn};

/// Traffic class governing DSCP `IP_TOS` tagging, Linux `SO_PRIORITY` hardware ring queuing,
/// and anti-bufferbloat kernel socket buffer sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QosClass {
    /// UDP 48100: DSCP EF (`0x2E` -> `IP_TOS 0xB8`), ultra-low jitter USB HID stream.
    HidExpeditedForwarding,
    /// Port 48101: DSCP CS6 (`0x30` -> `IP_TOS 0xC0`), 50 Hz heartbeat & state machine control.
    ControlNetworkControl,
    /// UDP 48102..=48103: DSCP AF41 (`0x22` -> `IP_TOS 0x88`), paced native-resolution video.
    VideoAssuredForwarding,
}

impl QosClass {
    /// Returns the 6-bit DSCP value (`0x2E`, `0x30`, or `0x22`).
    #[inline]
    pub const fn dscp(self) -> u8 {
        match self {
            Self::HidExpeditedForwarding => DSCP_EF,
            Self::ControlNetworkControl => DSCP_CS6,
            Self::VideoAssuredForwarding => DSCP_AF41,
        }
    }

    /// Returns the 8-bit `IP_TOS` byte (`dscp << 2`) preserving the lower 2 ECN bits as `0b00`.
    #[inline]
    pub const fn ip_tos_byte(self) -> u8 {
        match self {
            Self::HidExpeditedForwarding => TOS_HID_EF,
            Self::ControlNetworkControl => TOS_CONTROL_CS6,
            Self::VideoAssuredForwarding => TOS_VIDEO_AF41,
        }
    }

    /// Returns the Linux `SO_PRIORITY` value (0..=7) mapped to NIC hardware TX queues (`mqprio`/`fq_codel`).
    #[inline]
    pub const fn linux_so_priority(self) -> i32 {
        match self {
            Self::ControlNetworkControl => 7,
            Self::HidExpeditedForwarding => 6,
            Self::VideoAssuredForwarding => 4,
        }
    }

    /// Returns `(recv_buf_bytes, send_buf_bytes)` tuned to prevent bufferbloat while avoiding drops.
    #[inline]
    pub const fn buffer_sizes(self) -> (i32, i32) {
        match self {
            // Tight 256 KiB buffers for HID and Control prevent stale queue accumulation
            Self::HidExpeditedForwarding | Self::ControlNetworkControl => (262_144, 262_144),
            // 4 MiB absorbs ~45 ms of paced 700 Mbit/s video bursts
            Self::VideoAssuredForwarding => (4 * 1024 * 1024, 4 * 1024 * 1024),
        }
    }
}

/// Validates that `ip` is a specific host interface IPv4 address and NEVER `0.0.0.0` (`UNSPECIFIED`),
/// broadcast, or multicast.
pub fn validate_strict_interface_ip(ip: Ipv4Addr) -> Result<(), PalError> {
    if ip.is_unspecified() {
        return Err(PalError::Network(
            "Security violation: binding to 0.0.0.0 (INADDR_ANY) is strictly forbidden; must bind to 192.168.137.1 or 192.168.137.2".into(),
        ));
    }
    if ip.is_broadcast() || ip.is_multicast() {
        return Err(PalError::Network(format!(
            "Invalid interface IP {ip}: broadcast/multicast addresses are forbidden"
        )));
    }
    Ok(())
}

/// An existing socket may remain bound after its interface loses its address.
/// Probe a fresh ephemeral binding rather than trusting that stale socket.
pub fn local_link_address_available(ip: Ipv4Addr) -> bool {
    validate_strict_interface_ip(ip).is_ok() && UdpSocket::bind((ip, 0)).is_ok()
}

#[cfg(target_os = "linux")]
fn apply_raw_socket_qos(fd: std::os::unix::io::RawFd, qos: QosClass) -> Result<(), PalError> {
    let tos: libc::c_int = i32::from(dscp_to_tos(qos.dscp()));
    debug_assert_eq!(tos as u8, qos.ip_tos_byte());

    unsafe {
        let rc_tos = libc::setsockopt(
            fd,
            libc::IPPROTO_IP,
            libc::IP_TOS,
            &tos as *const libc::c_int as *const libc::c_void,
            core::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );
        if rc_tos != 0 {
            return Err(PalError::Network(format!(
                "setsockopt(IP_TOS=0x{:02X}) failed: {}",
                tos,
                std::io::Error::last_os_error()
            )));
        }

        let prio: libc::c_int = qos.linux_so_priority();
        let _ = libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PRIORITY,
            &prio as *const libc::c_int as *const libc::c_void,
            core::mem::size_of::<libc::c_int>() as libc::socklen_t,
        );

        let (rcv_buf, snd_buf) = qos.buffer_sizes();
        let set = |option: libc::c_int, value: libc::c_int| {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                option,
                &value as *const libc::c_int as *const libc::c_void,
                core::mem::size_of::<libc::c_int>() as libc::socklen_t,
            ) == 0
        };
        // The *FORCE variants bypass net.core.[rw]mem_max when privileged.
        if !set(libc::SO_RCVBUFFORCE, rcv_buf) {
            set(libc::SO_RCVBUF, rcv_buf);
        }
        if !set(libc::SO_SNDBUFFORCE, snd_buf) {
            set(libc::SO_SNDBUF, snd_buf);
        }
        let mut effective: libc::c_int = 0;
        let mut length = core::mem::size_of::<libc::c_int>() as libc::socklen_t;
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            &mut effective as *mut libc::c_int as *mut libc::c_void,
            &mut length,
        );
        // Linux reports twice the requested size (bookkeeping overhead).
        if qos == QosClass::VideoAssuredForwarding && effective < rcv_buf {
            warn!(
                requested = rcv_buf,
                effective,
                "Video receive buffer is limited by net.core.rmem_max; run scripts/debian-setup-link.sh to raise it"
            );
        }
    }

    debug!(
        ?qos,
        ip_tos_hex = format_args!("0x{:02X}", tos),
        so_priority = qos.linux_so_priority(),
        "Applied Linux kernel DSCP IP_TOS & SO_PRIORITY socket tagging"
    );
    Ok(())
}

#[cfg(target_os = "windows")]
fn apply_raw_socket_qos(
    raw_sock: std::os::windows::io::RawSocket,
    qos: QosClass,
) -> Result<(), PalError> {
    use windows::Win32::Networking::WinSock::{
        setsockopt, IPPROTO_IP, IP_TOS, SOCKET, SOL_SOCKET, SO_RCVBUF, SO_SNDBUF,
    };

    let sock = SOCKET(raw_sock as usize);
    let tos: i32 = i32::from(dscp_to_tos(qos.dscp()));
    debug_assert_eq!(tos as u8, qos.ip_tos_byte());

    unsafe {
        let tos_bytes = core::slice::from_raw_parts(
            &tos as *const i32 as *const u8,
            core::mem::size_of::<i32>(),
        );
        let rc = setsockopt(sock, IPPROTO_IP.0, IP_TOS, Some(tos_bytes));
        if rc != 0 {
            warn!(
                ?qos,
                ip_tos_hex = format_args!("0x{:02X}", tos),
                "WinSock setsockopt(IP_TOS) returned non-zero (ensure DisableUserTOSSetting=0 or QoS policy is active)"
            );
        }

        let (rcv_buf, snd_buf) = qos.buffer_sizes();
        let rcv_bytes = core::slice::from_raw_parts(
            &rcv_buf as *const i32 as *const u8,
            core::mem::size_of::<i32>(),
        );
        let snd_bytes = core::slice::from_raw_parts(
            &snd_buf as *const i32 as *const u8,
            core::mem::size_of::<i32>(),
        );
        let _ = setsockopt(sock, SOL_SOCKET, SO_RCVBUF, Some(rcv_bytes));
        let _ = setsockopt(sock, SOL_SOCKET, SO_SNDBUF, Some(snd_bytes));
    }

    debug!(
        ?qos,
        ip_tos_hex = format_args!("0x{:02X}", tos),
        "Applied Windows WinSock DSCP IP_TOS socket tagging"
    );
    Ok(())
}

/// Applies DSCP `IP_TOS` (`dscp << 2`) and buffer sizing to an existing `UdpSocket`.
pub fn apply_udp_qos(socket: &UdpSocket, qos: QosClass) -> Result<(), PalError> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        apply_raw_socket_qos(socket.as_raw_fd(), qos)
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::io::AsRawSocket;
        apply_raw_socket_qos(socket.as_raw_socket(), qos)
    }
}

/// Applies DSCP `IP_TOS` (`dscp << 2`), `TCP_NODELAY`, and non-blocking mode to a `TcpStream`.
pub fn configure_tcp_stream_qos(stream: &TcpStream, qos: QosClass) -> Result<(), PalError> {
    stream
        .set_nodelay(true)
        .map_err(|e| PalError::Network(format!("set_nodelay(true) failed: {e}")))?;
    stream
        .set_nonblocking(true)
        .map_err(|e| PalError::Network(format!("set_nonblocking(true) failed: {e}")))?;

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        apply_raw_socket_qos(stream.as_raw_fd(), qos)
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::io::AsRawSocket;
        apply_raw_socket_qos(stream.as_raw_socket(), qos)
    }
}

/// Binds a non-blocking `UdpSocket` strictly to `bind_ip:port` (rejecting `0.0.0.0`)
/// and tags it with the specified `QosClass` (`IP_TOS = dscp << 2`).
pub fn bind_qos_udp_socket(
    bind_ip: Ipv4Addr,
    port: u16,
    qos: QosClass,
) -> Result<UdpSocket, PalError> {
    validate_strict_interface_ip(bind_ip)?;
    let addr = SocketAddr::V4(SocketAddrV4::new(bind_ip, port));
    let socket = UdpSocket::bind(addr)
        .map_err(|e| PalError::Network(format!("UDP bind({addr}) failed: {e}")))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| PalError::Network(format!("UDP set_nonblocking({addr}) failed: {e}")))?;
    apply_udp_qos(&socket, qos)?;
    Ok(socket)
}

/// Binds a non-blocking `TcpListener` strictly to `bind_ip:port` (rejecting `0.0.0.0`)
/// and tags it with `QosClass::ControlNetworkControl` (`DSCP CS6` / `IP_TOS 0xC0`).
pub fn bind_qos_tcp_listener(
    bind_ip: Ipv4Addr,
    port: u16,
    qos: QosClass,
) -> Result<TcpListener, PalError> {
    validate_strict_interface_ip(bind_ip)?;
    let addr = SocketAddr::V4(SocketAddrV4::new(bind_ip, port));
    let listener = TcpListener::bind(addr)
        .map_err(|e| PalError::Network(format!("TCP bind({addr}) failed: {e}")))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| PalError::Network(format!("TCP set_nonblocking({addr}) failed: {e}")))?;

    #[cfg(target_os = "linux")]
    {
        use std::os::unix::io::AsRawFd;
        apply_raw_socket_qos(listener.as_raw_fd(), qos)?;
    }
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::io::AsRawSocket;
        apply_raw_socket_qos(listener.as_raw_socket(), qos)?;
    }
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dscp_tos_ecn_bitshift_values() {
        assert_eq!(QosClass::HidExpeditedForwarding.dscp(), 0x2E);
        assert_eq!(QosClass::HidExpeditedForwarding.ip_tos_byte(), 0xB8);

        assert_eq!(QosClass::ControlNetworkControl.dscp(), 0x30);
        assert_eq!(QosClass::ControlNetworkControl.ip_tos_byte(), 0xC0);

        assert_eq!(QosClass::VideoAssuredForwarding.dscp(), 0x22);
        assert_eq!(QosClass::VideoAssuredForwarding.ip_tos_byte(), 0x88);
    }

    #[test]
    fn test_reject_unspecified_0_0_0_0_binding() {
        assert!(validate_strict_interface_ip(Ipv4Addr::UNSPECIFIED).is_err());
        assert!(validate_strict_interface_ip(Ipv4Addr::BROADCAST).is_err());
        assert!(validate_strict_interface_ip(Ipv4Addr::new(192, 168, 137, 1)).is_ok());
        assert!(validate_strict_interface_ip(Ipv4Addr::new(192, 168, 137, 2)).is_ok());
    }
}
