//! TCP socket implementation for DPDK
//!
//! This module provides TCP-specific functionality on top of the DPDK sockets.
//! The key component is the `DpdkTcpStream` which implements `AsyncRead` and
//! `AsyncWrite` traits, making it compatible with tokio's async I/O ecosystem.
//!
//! # Example
//!
//! ```rust
//! use dpdk_socket::{DpdkTcpStream, DpdkOptions, init};
//! use std::net::{IpAddr, Ipv4Addr, SocketAddr};
//! use tokio::io::{AsyncReadExt, AsyncWriteExt};
//!
//! async fn example() -> Result<(), Box<dyn std::error::Error>> {
//!     // Initialize DPDK (omitted for brevity)
//!
//!     // Connect to a server
//!     let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 8080);
//!     let mut stream = DpdkTcpStream::connect(server_addr).await?;
//!
//!     // Send and receive data
//!     stream.write_all(b"Hello").await?;
//!     let mut buf = [0u8; 1024];
//!     let n = stream.read(&mut buf).await?;
//!
//!     Ok(())
//! }
//! ```

use bytes::{BytesMut, Buf};
use futures::Future;
use rand;
use std::{
    io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    task::{Context, Poll},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time,
};
use tracing::{debug, error, info};
use crate::common::{DpdkSocket, Result, DpdkError};
// Include the auto-generated DPDK bindings
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
// Reexport the mock DPDK symbols for convenience
pub mod dpdk {
    pub use super::mock_dpdk::*;
}

// Define TCP flag constants since they aren't in the bindings
#[allow(dead_code)]
pub mod tcp_flags {
    pub const RTE_TCP_FIN_FLAG: u8 = 0x01;
    pub const RTE_TCP_SYN_FLAG: u8 = 0x02;
    pub const RTE_TCP_RST_FLAG: u8 = 0x04;
    pub const RTE_TCP_PSH_FLAG: u8 = 0x08;
    pub const RTE_TCP_ACK_FLAG: u8 = 0x10;
    pub const RTE_TCP_URG_FLAG: u8 = 0x20;
    pub const RTE_TCP_ECE_FLAG: u8 = 0x40;
    pub const RTE_TCP_CWR_FLAG: u8 = 0x80;
}

/// TCP connection state
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpState {
    Closed,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}
/// TCP connection parameters
#[derive(Debug, Clone)]
pub struct TcpParams {
    /// Maximum number of connection attempts
    pub max_retries: usize,
    /// Timeout for connection attempts in milliseconds
    pub connect_timeout_ms: u64,
    /// Keep-alive interval in milliseconds
    pub keepalive_interval_ms: u64,
    /// Keep-alive timeout in milliseconds
    pub keepalive_timeout_ms: u64,
    /// TCP flags for SYN packet
    pub syn_flags: u8,
    /// TCP flags for ACK packet
    pub ack_flags: u8,
    /// Initial sequence number
    pub initial_seq: u32,
}

impl Default for TcpParams {
    fn default() -> Self {
        Self {
            max_retries: 5,
            connect_timeout_ms: 1000,
            keepalive_interval_ms: 10000,
            keepalive_timeout_ms: 2000,
            syn_flags: tcp_flags::RTE_TCP_SYN_FLAG,
            ack_flags: tcp_flags::RTE_TCP_ACK_FLAG,
            initial_seq: 0,
        }
    }
}

/// TCP connection management
pub struct TcpConnection {
    // Local address used for the connection (kept for potential future use)
    #[allow(dead_code)]
    local_addr: SocketAddr,
    
    peer_addr: SocketAddr,
    state: Arc<Mutex<TcpState>>,
    params: TcpParams,
    seq_num: Arc<Mutex<u32>>,
    ack_num: Arc<Mutex<u32>>,
    last_activity: Arc<Mutex<std::time::Instant>>,
}

impl TcpConnection {
    /// Create a new TCP connection
    pub fn new(local_addr: SocketAddr, peer_addr: SocketAddr, params: Option<TcpParams>) -> Self {
        let params_value = params.unwrap_or_default();
        let initial_seq = params_value.initial_seq;
        
        Self {
            local_addr,
            peer_addr,
            state: Arc::new(Mutex::new(TcpState::Closed)),
            params: params_value,
            seq_num: Arc::new(Mutex::new(initial_seq)),
            ack_num: Arc::new(Mutex::new(0)),
            last_activity: Arc::new(Mutex::new(std::time::Instant::now())),
        }
    }
    
    /// Get the current TCP state
    pub fn state(&self) -> TcpState {
        *self.state.lock().unwrap()
    }
    
    /// Check if the connection is established
    pub fn is_established(&self) -> bool {
        self.state() == TcpState::Established
    }
    
    /// Update the connection state
    fn set_state(&self, state: TcpState) {
        let mut current_state = self.state.lock().unwrap();
        debug!("TCP state transition: {:?} -> {:?}", *current_state, state);
        *current_state = state;
    }
    
    /// Update sequence number
    fn update_seq(&self, increment: u32) {
        let mut seq = self.seq_num.lock().unwrap();
        *seq = seq.wrapping_add(increment);
    }
    
    /// Update acknowledgement number
    fn update_ack(&self, new_ack: u32) {
        let mut ack = self.ack_num.lock().unwrap();
        *ack = new_ack;
    }
    
    /// Record activity to reset keepalive timer
    pub fn record_activity(&self) {
        let mut last = self.last_activity.lock().unwrap();
        *last = std::time::Instant::now();
    }
    
    /// Check if keepalive should be sent
    pub fn should_send_keepalive(&self) -> bool {
        if self.state() != TcpState::Established {
            return false;
        }
        
        let last = *self.last_activity.lock().unwrap();
        let elapsed = last.elapsed().as_millis() as u64;
        elapsed > self.params.keepalive_interval_ms
    }
}

impl DpdkTcpStream {
    /// Connect to a remote TCP endpoint using default parameters
    pub async fn connect(addr: SocketAddr) -> Result<Self> {
        Self::connect_with_options(addr, None, None).await
    }

    /// Connect to a remote TCP endpoint with specified local address
    pub async fn connect_from(addr: SocketAddr, local_addr: SocketAddr) -> Result<Self> {
        Self::connect_with_options(addr, Some(local_addr), None).await
    }
    
    /// Connect to a remote TCP endpoint with custom TCP parameters
    pub async fn connect_with_params(addr: SocketAddr, params: TcpParams) -> Result<Self> {
        Self::connect_with_options(addr, None, Some(params)).await
    }

    /// Connect to a remote TCP endpoint with proper TCP handshake and all options
    pub async fn connect_with_options(
        addr: SocketAddr, 
        local_addr: Option<SocketAddr>, 
        params: Option<TcpParams>
    ) -> Result<Self> {
        // Use provided local address or bind to any address with random port
        let local_addr = local_addr.unwrap_or_else(|| {
            SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
                0, // Use a random port for outgoing connections
            )
        });
        
        let socket = DpdkSocket::bind_tcp(local_addr).await?;
        let params = params.unwrap_or_default();
        
        // Create TCP connection object
        let connection = TcpConnection::new(socket.local_addr(), addr, Some(params.clone()));
        
        // Start TCP handshake
        let result = Self::perform_handshake(&socket, &connection).await;
        
        if let Err(e) = result {
            error!("TCP handshake failed: {}", e);
            return Err(e);
        }
        
        let stream = Self {
            socket,
            peer_addr: addr,
            read_buffer: BytesMut::new(),
            connection: Arc::new(connection),
        };
        
        // Start keepalive task
        let connection_clone = stream.connection.clone();
        let socket_clone = stream.socket.clone();
        tokio::spawn(async move {
            Self::keepalive_task(socket_clone, connection_clone).await;
        });
        
        Ok(stream)
    }
    
    /// Connect to a remote TCP endpoint with proper TCP handshake (legacy method)
    pub async fn connect_with_handshake(addr: SocketAddr, params: Option<TcpParams>) -> Result<Self> {
        Self::connect_with_options(addr, None, params).await
    }
    
    /// Perform TCP handshake
    async fn perform_handshake(socket: &DpdkSocket, connection: &TcpConnection) -> Result<()> {
        let max_retries = connection.params.max_retries;
        let timeout = Duration::from_millis(connection.params.connect_timeout_ms);
        
        connection.set_state(TcpState::SynSent);
        
        // Generate random initial sequence number for added security
        let isn = rand::random::<u32>();
        *connection.seq_num.lock().unwrap() = isn;
        
        // Send SYN packet
        Self::send_tcp_control_packet(
            socket,
            connection.peer_addr,
            connection.params.syn_flags,
            isn,
            0,
            &[],
        ).await?;
        
        // Wait for SYN-ACK
        for attempt in 0..max_retries {
            match time::timeout(timeout, socket.recv_from()).await {
                Ok(Ok((data, src_addr))) => {
                    if src_addr != connection.peer_addr {
                        continue; // Packet from wrong source
                    }
                    
                    if let Some((flags, seq, _ack)) = Self::parse_tcp_packet(&data) {
                        if flags & tcp_flags::RTE_TCP_SYN_FLAG != 0 && flags & tcp_flags::RTE_TCP_ACK_FLAG != 0 {
                            // Got SYN-ACK, update sequence and ack numbers
                            connection.update_ack(seq.wrapping_add(1));
                            connection.update_seq(1); // SYN counts as 1 byte
                            
                            // Get sequence and acknowledgement numbers before await
                            let seq_num = *connection.seq_num.lock().unwrap();
                            let ack_num = *connection.ack_num.lock().unwrap();
                            
                            // Send ACK to complete handshake
                            Self::send_tcp_control_packet(
                                socket,
                                connection.peer_addr,
                                connection.params.ack_flags,
                                seq_num,
                                ack_num,
                                &[],
                            ).await?;
                            
                            connection.set_state(TcpState::Established);
                            connection.record_activity();
                            return Ok(());
                        }
                    }
                },
                Ok(Err(e)) => {
                    error!("Error receiving SYN-ACK: {}", e);
                    if attempt == max_retries - 1 {
                        connection.set_state(TcpState::Closed);
                        return Err(e);
                    }
                },
                Err(_) => {
                    // Timeout, retry
                    if attempt < max_retries - 1 {
                        debug!("Timeout waiting for SYN-ACK, retry {}/{}", attempt + 1, max_retries);
                        
                        // Get sequence number before await
                        let seq_num = *connection.seq_num.lock().unwrap();
                        
                        // Resend SYN
                        Self::send_tcp_control_packet(
                            socket,
                            connection.peer_addr,
                            connection.params.syn_flags,
                            seq_num,
                            0,
                            &[],
                        ).await?;
                    } else {
                        connection.set_state(TcpState::Closed);
                        return Err(DpdkError::IoError(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "TCP handshake timed out",
                        )));
                    }
                }
            }
        }
        
        connection.set_state(TcpState::Closed);
        Err(DpdkError::IoError(io::Error::new(
            io::ErrorKind::TimedOut,
            "TCP handshake failed after max retries",
        )))
    }
    
    /// Parse TCP packet to extract flags, sequence and ack numbers
    fn parse_tcp_packet(data: &[u8]) -> Option<(u8, u32, u32)> {
        if data.len() < 54 { // Minimum size for IP + TCP headers
            return None;
        }
        
        // TCP header starts at offset 34 in a standard TCP/IP packet
        // This is simplified and assumes standard headers
        let tcp_offset = 34;
        
        if data.len() < tcp_offset + 20 {
            return None;
        }
        
        // Extract TCP flags, sequence number, and ack number
        let flags = data[tcp_offset + 13];
        let seq = u32::from_be_bytes([
            data[tcp_offset + 4],
            data[tcp_offset + 5],
            data[tcp_offset + 6],
            data[tcp_offset + 7],
        ]);
        let ack = u32::from_be_bytes([
            data[tcp_offset + 8],
            data[tcp_offset + 9],
            data[tcp_offset + 10],
            data[tcp_offset + 11],
        ]);
        
        Some((flags, seq, ack))
    }
    
    /// Send TCP control packet (SYN, ACK, etc.)
    async fn send_tcp_control_packet(
        socket: &DpdkSocket,
        dst: SocketAddr,
        flags: u8,
        seq: u32,
        ack: u32,
        data: &[u8],
    ) -> Result<usize> {
        // For a complete implementation, you would construct a proper TCP packet here
        // with all the necessary headers and checksums
        
        // This is a simplified version that relies on the underlying DpdkSocket
        // to create most of the packet
        
        // Construct a minimal TCP header + data
        let mut packet = Vec::with_capacity(20 + data.len());
        
        // Source port and destination port (already handled by DpdkSocket)
        packet.extend_from_slice(&[0, 0, 0, 0]);
        
        // Sequence number
        packet.extend_from_slice(&seq.to_be_bytes());
        
        // Acknowledgement number
        packet.extend_from_slice(&ack.to_be_bytes());
        
        // Data offset, reserved, flags (5 << 4 = 20 bytes TCP header)
        packet.extend_from_slice(&[(5 << 4), flags]);
        
        // Window size (use 64KB)
        packet.extend_from_slice(&[0xFF, 0xFF]);
        
        // Checksum (set to 0 for now, DpdkSocket will calculate)
        packet.extend_from_slice(&[0, 0]);
        
        // Urgent pointer
        packet.extend_from_slice(&[0, 0]);
        
        // Add data if any
        packet.extend_from_slice(data);
        
        // Send via raw socket
        socket.send_to(&packet, dst).await
    }
    
    /// Keepalive task to maintain connection
    async fn keepalive_task(socket: DpdkSocket, connection: Arc<TcpConnection>) {
        let interval = Duration::from_millis(1000); // Check every second
        
        loop {
            time::sleep(interval).await;
            
            if connection.state() != TcpState::Established {
                break;
            }
            
            if connection.should_send_keepalive() {
                debug!("Sending TCP keepalive");
                
                // Send keepalive packet (ACK with previous sequence number)
                let seq = *connection.seq_num.lock().unwrap();
                let ack = *connection.ack_num.lock().unwrap();
                
                if let Err(e) = Self::send_tcp_control_packet(
                    &socket,
                    connection.peer_addr,
                    connection.params.ack_flags,
                    seq - 1, // Previous sequence number
                    ack,
                    &[],
                ).await {
                    error!("Failed to send keepalive: {}", e);
                    
                    // If this was a network error, might need to set connection as closed
                    // For now, we'll just log the error and continue
                }
                
                connection.record_activity();
            }
        }
    }
    
    /// Attempt to reconnect if the connection is closed
    pub async fn reconnect(&mut self) -> Result<()> {
        if self.connection.state() == TcpState::Established {
            // Already connected
            return Ok(());
        }
        
        info!("Attempting to reconnect to {}", self.peer_addr);
        
        // Perform handshake
        let result = Self::perform_handshake(&self.socket, &self.connection).await;
        
        if let Err(e) = result {
            error!("Reconnection failed: {}", e);
            return Err(e);
        }
        
        info!("Successfully reconnected to {}", self.peer_addr);
        Ok(())
    }
    
    /// Check if connection is active and try to reconnect if not
    pub async fn ensure_connected(&mut self) -> Result<()> {
        if self.connection.state() != TcpState::Established {
            // First check if connection can be reused
            match self.connection.state() {
                TcpState::Closed | TcpState::SynSent => {
                    // These states indicate a connection that hasn't succeeded yet
                    // or has been closed
                    info!("Connection is not established (state: {:?}), attempting reconnect", 
                          self.connection.state());
                    self.reconnect().await
                },
                _ => {
                    // Connection is in another state like FIN_WAIT, etc.
                    // Force close and reconnect
                    info!("Connection is in {:?} state, forcing reconnection", 
                          self.connection.state());
                    self.connection.set_state(TcpState::Closed);
                    self.reconnect().await
                }
            }
        } else {
            // Connection is already established
            debug!("Connection already established");
            Ok(())
        }
    }
}

// Extend the existing DpdkTcpStream struct
pub struct DpdkTcpStream {
    socket: DpdkSocket,
    peer_addr: SocketAddr,
    read_buffer: BytesMut,
    connection: Arc<TcpConnection>,
}

impl DpdkTcpStream {
    /// Get the peer address
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }
    
    /// Get the local address
    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }
    
    /// Get the connection state
    pub fn state(&self) -> TcpState {
        self.connection.state()
    }
    
    /// Set the connection state (mostly for testing)
    pub fn set_state(&self, state: TcpState) {
        self.connection.set_state(state);
    }
    
    /// Gracefully shut down the connection
    pub async fn shutdown(&mut self) -> io::Result<()> {
        match Pin::new(self).poll_shutdown(&mut Context::from_waker(futures::task::noop_waker_ref())) {
            Poll::Ready(Ok(())) => Ok(()),
            Poll::Ready(Err(e)) => Err(e),
            Poll::Pending => Err(io::Error::new(io::ErrorKind::Other, "Shutdown polling failed")),
        }
    }
}

// Override the existing implementations to include connection state management
impl AsyncRead for DpdkTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        
        // Check if connection is established
        if this.connection.state() != TcpState::Established {
            // Create a new future for reconnection
            let reconnect_future = this.ensure_connected();
            let mut pinned_future = Box::pin(reconnect_future);
            
            // Poll the reconnection future
            match pinned_future.as_mut().poll(cx) {
                Poll::Ready(Ok(_)) => {
                    // Successfully reconnected, now we can continue with the read
                    debug!("Successfully reconnected in poll_read");
                },
                Poll::Ready(Err(e)) => {
                    // Reconnection failed
                    error!("Failed to reconnect in poll_read: {}", e);
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        format!("Failed to reconnect: {}", e),
                    )));
                },
                Poll::Pending => {
                    // Reconnection is still in progress
                    return Poll::Pending;
                }
            }
        }
        
        // Handle buffered data first
        if !this.read_buffer.is_empty() {
            let n = std::cmp::min(buf.remaining(), this.read_buffer.len());
            buf.put_slice(&this.read_buffer[..n]);
            
            let mut temp = std::mem::take(&mut this.read_buffer);
            temp.advance(n);
            this.read_buffer = temp;
            
            this.connection.record_activity();
            return Poll::Ready(Ok(()));
        }
        
        // Poll for new data
        let mut recv_fut = this.socket.recv_from();
        let recv_fut = unsafe { Pin::new_unchecked(&mut recv_fut) };
        
        match recv_fut.poll(cx) {
            Poll::Ready(Ok((data, src_addr))) => {
                // Verify the packet is from the expected peer
                if src_addr != this.peer_addr {
                    // Ignore packets from other sources
                    return Poll::Pending;
                }
                
                // Extract TCP header information
                if let Some((flags, seq, _ack)) = Self::parse_tcp_packet(&data) {
                    // Handle TCP flags
                    if flags & tcp_flags::RTE_TCP_FIN_FLAG != 0 {
                        // Received FIN, connection is closing
                        this.connection.set_state(TcpState::CloseWait);
                        
                        // Send ACK for FIN
                        let seq_num = *this.connection.seq_num.lock().unwrap();
                        let ack_num = seq.wrapping_add(1);
                        this.connection.update_ack(ack_num);
                        
                        // Spawn task to send ACK
                        let socket_clone = this.socket.clone();
                        let peer_addr = this.peer_addr;
                        let ack_flags = this.connection.params.ack_flags;
                        tokio::spawn(async move {
                            if let Err(e) = Self::send_tcp_control_packet(
                                &socket_clone,
                                peer_addr,
                                ack_flags,
                                seq_num,
                                ack_num,
                                &[],
                            ).await {
                                error!("Failed to send FIN-ACK: {}", e);
                            }
                        });
                        
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::BrokenPipe,
                            "Connection closed by peer",
                        )));
                    }
                    
                    // Calculate payload offset
                    let data_offset = 54; // Simplified, assuming standard headers
                    
                    if data.len() > data_offset {
                        // Update acknowledgement number
                        let payload_len = data.len() - data_offset;
                        this.connection.update_ack(seq.wrapping_add(payload_len as u32));
                        
                        // Acknowledge the data
                        let ack_num = *this.connection.ack_num.lock().unwrap();
                        let seq_num = *this.connection.seq_num.lock().unwrap();
                        
                        // Send ACK (in background to avoid blocking)
                        let socket_clone = this.socket.clone();
                        let peer_addr = this.peer_addr;
                        let ack_flags = this.connection.params.ack_flags;
                        tokio::spawn(async move {
                            if let Err(e) = Self::send_tcp_control_packet(
                                &socket_clone,
                                peer_addr,
                                ack_flags,
                                seq_num,
                                ack_num,
                                &[],
                            ).await {
                                error!("Failed to send ACK: {}", e);
                            }
                        });
                        
                        // Copy payload to buffer
                        let n = std::cmp::min(buf.remaining(), payload_len);
                        buf.put_slice(&data[data_offset..data_offset + n]);
                        
                        // Store excess data if any
                        if payload_len > n {
                            this.read_buffer.extend_from_slice(&data[data_offset + n..]);
                        }
                        
                        this.connection.record_activity();
                        return Poll::Ready(Ok(()));
                    }
                }
                
                // If we got a valid packet but no data, just record activity
                this.connection.record_activity();
                return Poll::Pending;
            },
            Poll::Ready(Err(e)) => {
                error!("Error receiving TCP data: {}", e);
                // Set connection state to closed on error
                this.connection.set_state(TcpState::Closed);
                Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)))
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for DpdkTcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        
        // Check if connection is established
        if this.connection.state() != TcpState::Established {
            // Create a new future for reconnection
            let reconnect_future = this.ensure_connected();
            let mut pinned_future = Box::pin(reconnect_future);
            
            // Poll the reconnection future
            match pinned_future.as_mut().poll(cx) {
                Poll::Ready(Ok(_)) => {
                    // Successfully reconnected, now we can continue with the write
                    debug!("Successfully reconnected in poll_write");
                },
                Poll::Ready(Err(e)) => {
                    // Reconnection failed
                    error!("Failed to reconnect in poll_write: {}", e);
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::NotConnected,
                        format!("Failed to reconnect: {}", e),
                    )));
                },
                Poll::Pending => {
                    // Reconnection is still in progress
                    return Poll::Pending;
                }
            }
        }
        
        // Get sequence and ack numbers
        let seq_num = *this.connection.seq_num.lock().unwrap();
        let ack_num = *this.connection.ack_num.lock().unwrap();
        
        // Create TCP flags - PSH and ACK for data
        let flags = dpdk::RTE_TCP_PSH_FLAG | dpdk::RTE_TCP_ACK_FLAG;
        
        // Build the packet (simplified, relies on DpdkSocket for most headers)
        let mut packet = Vec::with_capacity(20 + buf.len());
        
        // TCP header (simplified)
        packet.extend_from_slice(&[0, 0, 0, 0]); // Source/dest ports handled by socket
        packet.extend_from_slice(&seq_num.to_be_bytes());
        packet.extend_from_slice(&ack_num.to_be_bytes());
        packet.extend_from_slice(&[(5 << 4), flags]); // Data offset, flags
        packet.extend_from_slice(&[0xFF, 0xFF]); // Window size
        packet.extend_from_slice(&[0, 0]); // Checksum
        packet.extend_from_slice(&[0, 0]); // Urgent pointer
        
        // Add data
        packet.extend_from_slice(buf);
        
        // Send via raw socket
        let mut fut = this.socket.send_to(&packet, this.peer_addr);
        let pinned_fut = unsafe { Pin::new_unchecked(&mut fut) };
        
        match pinned_fut.poll(cx) {
            Poll::Ready(Ok(n)) => {
                // Update sequence number based on data sent
                // Note: For a real implementation, this should be the data actually sent, not the header
                let data_len = n - 20; // Subtract header size
                this.connection.update_seq(data_len as u32);
                this.connection.record_activity();
                Poll::Ready(Ok(data_len))
            },
            Poll::Ready(Err(e)) => {
                error!("Error sending TCP data: {}", e);
                // Connection might be broken
                this.connection.set_state(TcpState::Closed);
                Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)))
            },
            Poll::Pending => Poll::Pending,
        }
    }
    
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // DPDK does not buffer, so flush is a no-op
        Poll::Ready(Ok(()))
    }
    
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        
        // If already closed, just return success
        if matches!(
            this.connection.state(),
            TcpState::Closed | TcpState::FinWait1 | TcpState::FinWait2 |
            TcpState::Closing | TcpState::TimeWait | TcpState::LastAck
        ) {
            return Poll::Ready(Ok(()));
        }
        
        // Transition state
        this.connection.set_state(TcpState::FinWait1);
        
        // Send FIN packet
        let seq_num = *this.connection.seq_num.lock().unwrap();
        let ack_num = *this.connection.ack_num.lock().unwrap();
        
        let mut fut = Self::send_tcp_control_packet(
            &this.socket,
            this.peer_addr,
            tcp_flags::RTE_TCP_FIN_FLAG | tcp_flags::RTE_TCP_ACK_FLAG,
            seq_num,
            ack_num,
            &[],
        );
        
        let pinned_fut = unsafe { Pin::new_unchecked(&mut fut) };
        
        match pinned_fut.poll(cx) {
            Poll::Ready(Ok(_)) => {
                // Update sequence number for the FIN
                this.connection.update_seq(1);
                
                // In a real implementation, we should wait for FIN-ACK
                // before fully closing, but for simplicity we're considering
                // the socket closed once we send FIN
                
                // Transition to closed state
                this.connection.set_state(TcpState::Closed);
                Poll::Ready(Ok(()))
            },
            Poll::Ready(Err(e)) => {
                error!("Error sending FIN packet: {}", e);
                this.connection.set_state(TcpState::Closed);
                Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e)))
            },
            Poll::Pending => Poll::Pending,
        }
    }
}

// TCP flag constants are now defined in the tcp_flags module above