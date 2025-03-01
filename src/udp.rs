//! UDP socket implementation for DPDK
//!
//! This module provides a simplified UDP-specific interface on top of the DPDK socket 
//! functionality. It offers a familiar API similar to Rust's standard UDP sockets
//! but powered by DPDK for high-performance networking.
//!
//! # Example
//!
//! ```rust
//! use dpdk_socket::{DpdkUdpSocket, DpdkOptions, init};
//! use std::net::{IpAddr, Ipv4Addr, SocketAddr};
//!
//! async fn example() -> Result<(), Box<dyn std::error::Error>> {
//!     // Initialize DPDK (omitted for brevity)
//!
//!     // Bind to an address
//!     let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 8080);
//!     let socket = DpdkUdpSocket::bind(addr).await?;
//!
//!     // Send data
//!     let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 9000);
//!     socket.send_to(b"Hello, UDP!", target).await?;
//!
//!     // Receive data
//!     let (data, src) = socket.recv_from().await?;
//!     println!("Received {} bytes from {}", data.len(), src);
//!
//!     Ok(())
//! }
//! ```

use bytes::Bytes;
use std::net::SocketAddr;

use crate::common::{DpdkSocket, Result};

/// UDP-specific functionality for DPDK sockets
pub struct DpdkUdpSocket {
    socket: DpdkSocket,
}

impl DpdkUdpSocket {
    /// Bind to a local address
    pub async fn bind(addr: SocketAddr) -> Result<Self> {
        let socket = DpdkSocket::bind_udp(addr).await?;
        Ok(Self { socket })
    }

    /// Send data to the specified destination
    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> Result<usize> {
        self.socket.send_to(buf, addr).await
    }

    /// Receive data and the sender's address
    pub async fn recv_from(&self) -> Result<(Bytes, SocketAddr)> {
        self.socket.recv_from().await
    }

    /// Get the local address
    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }
}