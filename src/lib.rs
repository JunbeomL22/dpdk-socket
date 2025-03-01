//! # DPDK Socket
//! 
//! A high-level async UDP/TCP socket library for Rust using DPDK (Data Plane Development Kit).
//! 
//! ## Overview
//! 
//! This library provides an async-friendly interface to the DPDK networking library, allowing
//! for high-performance networking applications in Rust. The library is built on top of tokio
//! and is designed to integrate smoothly with other tokio-based networking code.
//! 
//! ## Module Structure
//! 
//! The library is organized into the following modules:
//! 
//! - `common`: Contains shared functionality like DPDK initialization, error types, socket base implementation
//! - `tcp`: Contains TCP-specific socket implementation with `AsyncRead`/`AsyncWrite` support
//! - `udp`: Contains UDP-specific socket implementation with a simpler send/receive interface
//! 
//! ## Basic Usage
//! 
//! First, initialize DPDK with appropriate options:
//! 
//! ```rust
//! use dpdk_socket::{DpdkOptions, init};
//! 
//! // Initialize DPDK with default options
//! let options = DpdkOptions::default();
//! init(options).expect("Failed to initialize DPDK");
//! ```
//! 
//! ### UDP Example
//! 
//! ```rust
//! use dpdk_socket::{DpdkUdpSocket, DpdkOptions, init};
//! use std::net::{IpAddr, Ipv4Addr, SocketAddr};
//! 
//! async fn udp_example() -> Result<(), Box<dyn std::error::Error>> {
//!     // Create a UDP socket
//!     let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 8080);
//!     let socket = DpdkUdpSocket::bind(addr).await?;
//!     
//!     // Send data
//!     let remote_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 8080);
//!     socket.send_to(b"Hello, DPDK!", remote_addr).await?;
//!     
//!     // Receive data
//!     let (data, src_addr) = socket.recv_from().await?;
//!     println!("Received {} bytes from {}", data.len(), src_addr);
//!     
//!     Ok(())
//! }
//! ```
//! 
//! ### TCP Example
//! 
//! ```rust
//! use dpdk_socket::{DpdkTcpStream, DpdkOptions, init};
//! use std::net::{IpAddr, Ipv4Addr, SocketAddr};
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//! 
//! async fn tcp_example() -> Result<(), Box<dyn std::error::Error>> {
//!     // Connect to server
//!     let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 8080);
//!     let mut stream = DpdkTcpStream::connect(server_addr).await?;
//!     
//!     // Send request
//!     stream.write_all(b"Hello, DPDK TCP!").await?;
//!     
//!     // Read response
//!     let mut buffer = vec![0u8; 1024];
//!     let n = stream.read(&mut buffer).await?;
//!     println!("Received: {:?}", String::from_utf8_lossy(&buffer[..n]));
//!     
//!     Ok(())
//! }
//! ```
//! 
//! ## Advanced Configuration
//! 
//! For more advanced configuration, see the `DpdkOptions` struct documentation
//! and the examples directory.

pub mod common;
pub mod tcp;
pub mod udp;

// Re-export main types and functions for ease of use
pub use common::{
    init, DpdkError, DpdkOptions, PacketInfo, Result, SocketType,
    DpdkSocket, // Re-export for advanced usage
};
pub use tcp::{DpdkTcpStream, TcpState, TcpParams};
pub use udp::DpdkUdpSocket;

#[cfg(test)]
mod tests {
    use super::*;

    // These tests would require a DPDK environment to run
    // so they are disabled by default
    
    #[test]
    #[ignore]
    fn test_dpdk_init() {
        // Initialize DPDK with default options
        let options = DpdkOptions::default();
        assert!(init(options).is_ok());
    }
}
