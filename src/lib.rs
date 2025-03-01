//! High-level async UDP/TCP socket library using DPDK

use bytes::{Bytes, BytesMut};
use bytes::Buf;
use futures::Future;
use once_cell::sync::OnceCell;
use std::{
    fmt, io,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
};
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncWrite},
    sync::{mpsc, oneshot},
};
use tracing::{error};

// Include the auto-generated DPDK bindings
include!(concat!(env!("OUT_DIR"), "/bindings.rs"));

// Reexport the mock DPDK symbols for convenience
pub mod dpdk {
    pub use super::mock_dpdk::*;
}

// DPDK constants
const RX_RING_SIZE: u16 = 1024;
const TX_RING_SIZE: u16 = 1024;
const NUM_MBUFS: u32 = 8191;
const MBUF_CACHE_SIZE: u32 = 250;
// We use standard MTU size by default
#[allow(dead_code)]
const MAX_PACKET_SIZE: usize = 1500;

/// Error types for DPDK Socket operations
#[derive(Error, Debug)]
pub enum DpdkError {
    #[error("DPDK initialization failed: {0}")]
    InitFailed(i32),
    
    #[error("DPDK device configuration failed: {0}")]
    DeviceConfigFailed(i32),
    
    #[error("DPDK memory pool creation failed")]
    MemPoolFailed,
    
    #[error("DPDK socket already initialized")]
    AlreadyInitialized,
    
    #[error("DPDK not initialized")]
    NotInitialized,
    
    #[error("Invalid port ID: {0}")]
    InvalidPort(u16),
    
    #[error("I/O error: {0}")]
    IoError(#[from] io::Error),
    
    #[error("Send failed: {0}")]
    SendFailed(i32),
    
    #[error("Receive failed: {0}")]
    RecvFailed(i32),
    
    #[error("Packet buffer allocation failed")]
    BufferAllocationFailed,
}

// Result type for DPDK operations
pub type Result<T> = std::result::Result<T, DpdkError>;

/// Global DPDK context
static DPDK_CONTEXT: OnceCell<Arc<DpdkContext>> = OnceCell::new();

/// DPDK initialization options
#[derive(Debug, Clone)]
pub struct DpdkOptions {
    pub port_id: u16,
    pub num_rx_queues: u16,
    pub num_tx_queues: u16,
    pub args: Vec<String>,
}

impl Default for DpdkOptions {
    fn default() -> Self {
        Self {
            port_id: 0,
            num_rx_queues: 1,
            num_tx_queues: 1,
            args: vec![
                "dpdk-socket".to_string(),
                "-l".to_string(), "0-3".to_string(),
                "-n".to_string(), "4".to_string(),
                "--proc-type=auto".to_string(),
            ],
        }
    }
}

/// A wrapper around a DPDK pointer that implements Send + Sync
/// This is safe because we ensure that the DPDK pointer is only accessed
/// from a single thread at a time
struct DpdkPointer<T>(*mut T);

impl<T> DpdkPointer<T> {
    fn new(ptr: *mut T) -> Self {
        DpdkPointer(ptr)
    }
    
    fn get(&self) -> *mut T {
        self.0
    }
}

// These impls are safe because we ensure access is controlled and always from the worker thread
unsafe impl<T> Send for DpdkPointer<T> {}
unsafe impl<T> Sync for DpdkPointer<T> {}

// Implement Copy and Clone for the DpdkPointer to allow sharing it across threads
impl<T> Clone for DpdkPointer<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for DpdkPointer<T> {}

/// Represents the global DPDK context
struct DpdkContext {
    port_id: u16,
    pktmbuf_pool: DpdkPointer<dpdk::rte_mempool>,
    initialized: bool,
}

impl DpdkContext {
    /// Initialize DPDK with the given options
    fn init(options: DpdkOptions) -> Result<Arc<Self>> {
        if DPDK_CONTEXT.get().is_some() {
            return Err(DpdkError::AlreadyInitialized);
        }

        // Convert args to C format
        let c_args: Vec<_> = options.args.iter()
            .map(|arg| std::ffi::CString::new(arg.as_str()).unwrap())
            .collect();
        let mut c_argv: Vec<_> = c_args.iter()
            .map(|arg| arg.as_ptr() as *mut i8)
            .collect();

        // Initialize EAL
        let ret = unsafe {
            dpdk::rte_eal_init(c_argv.len() as i32, c_argv.as_mut_ptr())
        };
        
        if ret < 0 {
            return Err(DpdkError::InitFailed(ret));
        }

        // Check if the specified port is available
        let port_id = options.port_id;
        if unsafe { dpdk::rte_eth_dev_is_valid_port(port_id) } == 0 {
            return Err(DpdkError::InvalidPort(port_id));
        }

        // Create memory pool for packet buffers
        let mp_name = std::ffi::CString::new("mbuf_pool").unwrap();
        let pktmbuf_pool = unsafe {
            dpdk::rte_pktmbuf_pool_create(
                mp_name.as_ptr(),
                NUM_MBUFS,
                MBUF_CACHE_SIZE,
                0,
                dpdk::RTE_MBUF_DEFAULT_BUF_SIZE,
                dpdk::rte_socket_id() as i32,
            )
        };

        if pktmbuf_pool.is_null() {
            return Err(DpdkError::MemPoolFailed);
        }
        
        // Wrap the pointer in our Send+Sync wrapper
        let pktmbuf_pool_wrapped = DpdkPointer::new(pktmbuf_pool);

        // Configure the Ethernet device
        let mut port_conf: dpdk::rte_eth_conf = unsafe { std::mem::zeroed() };
        // Note: Newer DPDK versions have different struct layout, we'll use offloads instead
        // Set the max packet length via offloads
        port_conf.rxmode.offloads |= dpdk::RTE_ETH_RX_OFFLOAD_JUMBO_FRAME;
        // Set MTU to standard ethernet frame size - cast to u16 since our struct has that type
        port_conf.rxmode.mtu = (dpdk::RTE_ETHER_MAX_LEN & 0xFFFF) as u16;

        let ret = unsafe {
            dpdk::rte_eth_dev_configure(
                port_id,
                options.num_rx_queues,
                options.num_tx_queues,
                &port_conf,
            )
        };

        if ret != 0 {
            return Err(DpdkError::DeviceConfigFailed(ret));
        }

        // Set up RX queues
        for i in 0..options.num_rx_queues {
            let ret = unsafe {
                dpdk::rte_eth_rx_queue_setup(
                    port_id,
                    i,
                    RX_RING_SIZE,
                    dpdk::rte_eth_dev_socket_id(port_id) as u32,
                    std::ptr::null(),
                    pktmbuf_pool,
                )
            };
            
            if ret < 0 {
                return Err(DpdkError::DeviceConfigFailed(ret));
            }
        }

        // Set up TX queues
        for i in 0..options.num_tx_queues {
            let ret = unsafe {
                dpdk::rte_eth_tx_queue_setup(
                    port_id,
                    i,
                    TX_RING_SIZE,
                    dpdk::rte_eth_dev_socket_id(port_id) as u32,
                    std::ptr::null(),
                )
            };
            
            if ret < 0 {
                return Err(DpdkError::DeviceConfigFailed(ret));
            }
        }

        // Start the Ethernet device
        let ret = unsafe { dpdk::rte_eth_dev_start(port_id) };
        if ret < 0 {
            return Err(DpdkError::DeviceConfigFailed(ret));
        }

        // Enable promiscuous mode
        unsafe { dpdk::rte_eth_promiscuous_enable(port_id) };

        let context = Arc::new(Self {
            port_id,
            pktmbuf_pool: pktmbuf_pool_wrapped,
            initialized: true,
        });

        // Store the context in the global cell
        if DPDK_CONTEXT.set(context.clone()).is_err() {
            return Err(DpdkError::AlreadyInitialized);
        }

        Ok(context)
    }

    /// Get the global DPDK context
    fn get() -> Result<Arc<Self>> {
        DPDK_CONTEXT.get()
            .cloned()
            .ok_or(DpdkError::NotInitialized)
    }
}

impl Drop for DpdkContext {
    fn drop(&mut self) {
        if self.initialized {
            unsafe {
                dpdk::rte_eth_dev_stop(self.port_id);
                dpdk::rte_eth_dev_close(self.port_id);
                dpdk::rte_eal_cleanup();
            }
        }
    }
}

/// Packet information
#[derive(Debug, Clone)]
pub struct PacketInfo {
    pub src_addr: SocketAddr,
    pub dst_addr: SocketAddr,
}

/// Received packet with metadata
#[derive(Debug)]
pub struct ReceivedPacket {
    pub data: Bytes,
    pub info: PacketInfo,
}

/// Socket type (UDP or TCP)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SocketType {
    Udp,
    Tcp,
}

/// Commands for the DPDK worker
enum DpdkCommand {
    Send {
        data: Bytes,
        dst: SocketAddr,
        reply: oneshot::Sender<Result<usize>>,
    },
    Close,
}

/// Packet receiver handle
struct PacketReceiver {
    rx: mpsc::Receiver<ReceivedPacket>,
    waker: Arc<Mutex<Option<Waker>>>,
}

/// DPDK Socket
pub struct DpdkSocket {
    socket_type: SocketType,
    local_addr: SocketAddr,
    tx: mpsc::Sender<DpdkCommand>,
    receiver: Arc<Mutex<PacketReceiver>>,
}

impl fmt::Debug for DpdkSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DpdkSocket")
            .field("socket_type", &self.socket_type)
            .field("local_addr", &self.local_addr)
            .finish()
    }
}

/// Initialize DPDK with the specified options
pub fn init(options: DpdkOptions) -> Result<()> {
    DpdkContext::init(options)?;
    Ok(())
}

impl DpdkSocket {
    /// Create a new UDP socket bound to the specified address
    pub async fn bind_udp(addr: SocketAddr) -> Result<Self> {
        Self::bind(SocketType::Udp, addr).await
    }

    /// Create a new TCP socket bound to the specified address
    pub async fn bind_tcp(addr: SocketAddr) -> Result<Self> {
        Self::bind(SocketType::Tcp, addr).await
    }

    /// Internal method to bind a socket
    async fn bind(socket_type: SocketType, addr: SocketAddr) -> Result<Self> {
        let context = DpdkContext::get()?;
        
        // Create channels for communication
        let (tx, rx) = mpsc::channel(100);
        let (packet_tx, packet_rx) = mpsc::channel(100);
        
        let waker = Arc::new(Mutex::new(None::<Waker>));
        let receiver = Arc::new(Mutex::new(PacketReceiver {
            rx: packet_rx,
            waker: waker.clone(),
        }));
        
        let socket = Self {
            socket_type,
            local_addr: addr,
            tx,
            receiver,
        };

        // Clone shared data for the worker task
        let port_id = context.port_id;
        let pktmbuf_pool = context.pktmbuf_pool;
        let socket_type_worker = socket_type;
        let local_addr_worker = addr;
        let waker_worker = waker.clone();

        // Create a thread for DPDK packet processing to avoid Send/Sync issues with raw pointers
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
                
            rt.block_on(async move {
                let mut command_rx = rx;
                
                // Process incoming packets and commands
                loop {
                    tokio::select! {
                        cmd = command_rx.recv() => {
                            match cmd {
                                Some(DpdkCommand::Send { data, dst, reply }) => {
                                    let result = Self::send_packet(
                                        port_id,
                                        pktmbuf_pool,
                                        socket_type_worker,
                                        local_addr_worker,
                                        dst,
                                        &data,
                                    );
                                    let _ = reply.send(result);
                                }
                                Some(DpdkCommand::Close) => {
                                    break;
                                }
                                None => {
                                    break;
                                }
                            }
                        }
                        
                        // TODO: This would be more efficient with proper DPDK event polling
                        _ = tokio::time::sleep(tokio::time::Duration::from_millis(1)) => {
                            // Process received packets
                            let packets = Self::recv_packets(
                                port_id,
                                pktmbuf_pool,
                                socket_type_worker,
                                local_addr_worker,
                            );
                            
                            for packet in packets {
                                if packet_tx.send(packet).await.is_err() {
                                    break;
                                }
                                
                                // Wake up any waiting receivers
                                if let Some(waker) = waker_worker.lock().unwrap().take() {
                                    waker.wake();
                                }
                            }
                        }
                    }
                }
            });
        });

        Ok(socket)
    }

    /// Send packet using DPDK
    fn send_packet(
        port_id: u16,
        pktmbuf_pool: DpdkPointer<dpdk::rte_mempool>,
        socket_type: SocketType,
        src: SocketAddr,
        dst: SocketAddr,
        data: &[u8],
    ) -> Result<usize> {
        // Allocate a packet buffer
        let mbuf = unsafe { dpdk::rte_pktmbuf_alloc(pktmbuf_pool.get()) };
        if mbuf.is_null() {
            return Err(DpdkError::BufferAllocationFailed);
        }

        let eth_hdr_size = std::mem::size_of::<dpdk::rte_ether_hdr>();
        let ip_hdr_size = std::mem::size_of::<dpdk::rte_ipv4_hdr>();
        
        let proto_hdr_size = match socket_type {
            SocketType::Udp => std::mem::size_of::<dpdk::rte_udp_hdr>(),
            SocketType::Tcp => std::mem::size_of::<dpdk::rte_tcp_hdr>(),
        };

        let total_header_size = eth_hdr_size + ip_hdr_size + proto_hdr_size;
        
        // Set up Ethernet header
        unsafe {
            let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let data_ptr = dpdk::rte_pktmbuf_mtod_offset(mbuf, null_ptr, 0) as *mut dpdk::rte_ether_hdr;
            let eth_hdr = &mut *data_ptr;
            
            // For a real implementation, you would use ARP to get the MAC addresses
            // This is simplified for demonstration
            eth_hdr.s_addr.addr_bytes = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
            eth_hdr.d_addr.addr_bytes = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
            eth_hdr.ether_type = u16::to_be(dpdk::RTE_ETHER_TYPE_IPV4);
            
            // Set up IP header
            let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let ip_hdr = dpdk::rte_pktmbuf_mtod_offset(mbuf, null_ptr, eth_hdr_size as u32) as *mut dpdk::rte_ipv4_hdr;
            let ip = &mut *ip_hdr;
            
            ip.version_ihl = (4 << 4) | 5; // IPv4, 5 32-bit words (20 bytes)
            ip.type_of_service = 0;
            ip.total_length = u16::to_be((ip_hdr_size + proto_hdr_size + data.len()) as u16);
            ip.packet_id = 0;
            ip.fragment_offset = 0;
            ip.time_to_live = 64;
            
            ip.next_proto_id = match socket_type {
                SocketType::Udp => dpdk::IPPROTO_UDP,
                SocketType::Tcp => dpdk::IPPROTO_TCP,
            };
            
            match (src.ip(), dst.ip()) {
                (IpAddr::V4(src_ip), IpAddr::V4(dst_ip)) => {
                    ip.src_addr = u32::from_be_bytes(src_ip.octets());
                    ip.dst_addr = u32::from_be_bytes(dst_ip.octets());
                },
                _ => {
                    // For simplicity, only supporting IPv4
                    dpdk::rte_pktmbuf_free(mbuf);
                    return Err(DpdkError::IoError(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "Only IPv4 is supported",
                    )));
                }
            }
            
            // Calculate IP checksum
            ip.hdr_checksum = 0;
            ip.hdr_checksum = Self::calculate_ipv4_checksum(ip);
            
            // Set up transport protocol header (UDP or TCP)
            match socket_type {
                SocketType::Udp => {
                    let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                    let udp_hdr = dpdk::rte_pktmbuf_mtod_offset(
                        mbuf,
                        null_ptr, 
                        (eth_hdr_size + ip_hdr_size) as u32,
                    ) as *mut dpdk::rte_udp_hdr;
                    let udp = &mut *udp_hdr;
                    
                    udp.src_port = u16::to_be(src.port());
                    udp.dst_port = u16::to_be(dst.port());
                    udp.dgram_len = u16::to_be((std::mem::size_of::<dpdk::rte_udp_hdr>() + data.len()) as u16);
                    udp.dgram_cksum = 0; // Set to 0 for now, calculate later if needed
                },
                SocketType::Tcp => {
                    let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                    let tcp_hdr = dpdk::rte_pktmbuf_mtod_offset(
                        mbuf,
                        null_ptr,
                        (eth_hdr_size + ip_hdr_size) as u32,
                    ) as *mut dpdk::rte_tcp_hdr;
                    let tcp = &mut *tcp_hdr;
                    
                    tcp.src_port = u16::to_be(src.port());
                    tcp.dst_port = u16::to_be(dst.port());
                    tcp.sent_seq = 0;
                    tcp.recv_ack = 0;
                    tcp.data_off = 5 << 4; // 5 32-bit words (20 bytes)
                    tcp.tcp_flags = dpdk::RTE_TCP_PSH_FLAG | dpdk::RTE_TCP_ACK_FLAG;
                    tcp.rx_win = u16::to_be(8192);
                    tcp.cksum = 0; // Set to 0 for now, calculate later if needed
                    tcp.tcp_urp = 0;
                }
            }
            
            // Copy packet data
            let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
            let data_dest = dpdk::rte_pktmbuf_mtod_offset(mbuf, null_ptr, total_header_size as u32) as *mut u8;
            std::ptr::copy_nonoverlapping(data.as_ptr(), data_dest, data.len());
            
            // Set packet length
            dpdk::rte_pktmbuf_append(mbuf, (total_header_size + data.len()) as u16);
            
            // Send the packet
            let mut tx_buf = [mbuf];
            let nb_tx = dpdk::rte_eth_tx_burst(port_id, 0, tx_buf.as_mut_ptr(), 1);
            
            if nb_tx < 1 {
                dpdk::rte_pktmbuf_free(mbuf);
                return Err(DpdkError::SendFailed(-1));
            }
        }
        
        Ok(data.len())
    }

    /// Receive packets
    fn recv_packets(
        port_id: u16,
        _pktmbuf_pool: DpdkPointer<dpdk::rte_mempool>,
        socket_type: SocketType,
        local_addr: SocketAddr,
    ) -> Vec<ReceivedPacket> {
        let mut result = Vec::new();
        
        // Receive up to 32 packets at a time
        let mut rx_mbufs: [*mut dpdk::rte_mbuf; 32] = [std::ptr::null_mut(); 32];
        
        unsafe {
            let nb_rx = dpdk::rte_eth_rx_burst(port_id, 0, rx_mbufs.as_mut_ptr(), 32);
            
            if nb_rx == 0 {
                return result;
            }
            
            for i in 0..nb_rx {
                let mbuf = rx_mbufs[i as usize];
                
                // Parse Ethernet header
                let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                let eth_hdr = dpdk::rte_pktmbuf_mtod(mbuf, null_ptr) as *mut dpdk::rte_ether_hdr;
                if (*eth_hdr).ether_type != u16::to_be(dpdk::RTE_ETHER_TYPE_IPV4) {
                    dpdk::rte_pktmbuf_free(mbuf);
                    continue;
                }
                
                // Parse IP header
                let eth_hdr_size = std::mem::size_of::<dpdk::rte_ether_hdr>();
                let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                let ip_hdr = dpdk::rte_pktmbuf_mtod_offset(mbuf, null_ptr, eth_hdr_size as u32) as *mut dpdk::rte_ipv4_hdr;
                
                let next_proto = (*ip_hdr).next_proto_id;
                let expected_proto = match socket_type {
                    SocketType::Udp => dpdk::IPPROTO_UDP,
                    SocketType::Tcp => dpdk::IPPROTO_TCP,
                };
                
                if next_proto != expected_proto {
                    dpdk::rte_pktmbuf_free(mbuf);
                    continue;
                }
                
                // Check if the packet is destined for our socket
                let dst_ip = Ipv4Addr::from(u32::from_be((*ip_hdr).dst_addr));
                
                if !matches!(local_addr.ip(), IpAddr::V4(addr) if addr == dst_ip) {
                    dpdk::rte_pktmbuf_free(mbuf);
                    continue;
                }
                
                let ip_hdr_size = std::mem::size_of::<dpdk::rte_ipv4_hdr>();
                let src_ip = Ipv4Addr::from(u32::from_be((*ip_hdr).src_addr));
                
                // Parse transport protocol header (UDP or TCP)
                match socket_type {
                    SocketType::Udp => {
                        let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                        let udp_hdr = dpdk::rte_pktmbuf_mtod_offset(
                            mbuf,
                            null_ptr,
                            (eth_hdr_size + ip_hdr_size) as u32,
                        ) as *mut dpdk::rte_udp_hdr;
                        
                        let dst_port = u16::from_be((*udp_hdr).dst_port);
                        
                        // Check if the packet is destined for our port
                        if dst_port != local_addr.port() {
                            dpdk::rte_pktmbuf_free(mbuf);
                            continue;
                        }
                        
                        let src_port = u16::from_be((*udp_hdr).src_port);
                        let udp_hdr_size = std::mem::size_of::<dpdk::rte_udp_hdr>();
                        
                        // Extract the packet data
                        let data_offset = eth_hdr_size + ip_hdr_size + udp_hdr_size;
                        let data_len = dpdk::rte_pktmbuf_data_len(mbuf) as usize - data_offset;
                        
                        let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                        let data_ptr = dpdk::rte_pktmbuf_mtod_offset(mbuf, null_ptr, data_offset as u32) as *mut u8;
                        let mut data = BytesMut::with_capacity(data_len);
                        std::ptr::copy_nonoverlapping(data_ptr, data.as_mut_ptr(), data_len);
                        data.set_len(data_len);
                        
                        let src_addr = SocketAddr::new(IpAddr::V4(src_ip), src_port);
                        let dst_addr = SocketAddr::new(IpAddr::V4(dst_ip), dst_port);
                        
                        result.push(ReceivedPacket {
                            data: data.freeze(),
                            info: PacketInfo {
                                src_addr,
                                dst_addr,
                            },
                        });
                    },
                    SocketType::Tcp => {
                        let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                        let tcp_hdr = dpdk::rte_pktmbuf_mtod_offset(
                            mbuf,
                            null_ptr,
                            (eth_hdr_size + ip_hdr_size) as u32,
                        ) as *mut dpdk::rte_tcp_hdr;
                        
                        let dst_port = u16::from_be((*tcp_hdr).dst_port);
                        
                        // Check if the packet is destined for our port
                        if dst_port != local_addr.port() {
                            dpdk::rte_pktmbuf_free(mbuf);
                            continue;
                        }
                        
                        let src_port = u16::from_be((*tcp_hdr).src_port);
                        let data_off = ((*tcp_hdr).data_off >> 4) as usize;
                        let tcp_hdr_size = data_off * 4; // Header length in bytes
                        
                        // Extract the packet data
                        let data_offset = eth_hdr_size + ip_hdr_size + tcp_hdr_size;
                        let data_len = dpdk::rte_pktmbuf_data_len(mbuf) as usize - data_offset;
                        
                        let null_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
                        let data_ptr = dpdk::rte_pktmbuf_mtod_offset(mbuf, null_ptr, data_offset as u32) as *mut u8;
                        let mut data = BytesMut::with_capacity(data_len);
                        std::ptr::copy_nonoverlapping(data_ptr, data.as_mut_ptr(), data_len);
                        data.set_len(data_len);
                        
                        let src_addr = SocketAddr::new(IpAddr::V4(src_ip), src_port);
                        let dst_addr = SocketAddr::new(IpAddr::V4(dst_ip), dst_port);
                        
                        result.push(ReceivedPacket {
                            data: data.freeze(),
                            info: PacketInfo {
                                src_addr,
                                dst_addr,
                            },
                        });
                    }
                }
                
                dpdk::rte_pktmbuf_free(mbuf);
            }
        }
        
        result
    }

    /// Calculate IPv4 checksum
    fn calculate_ipv4_checksum(ip: &mut dpdk::rte_ipv4_hdr) -> u16 {
        let mut sum: u32 = 0;
        
        // Treat the header as an array of 16-bit words
        let hdr_ptr = ip as *const dpdk::rte_ipv4_hdr as *const u16;
        
        // Calculate the sum of all 16-bit words in the header
        for i in 0..(std::mem::size_of::<dpdk::rte_ipv4_hdr>() / 2) {
            // Skip the checksum field itself (at offset 5)
            if i != 5 {
                unsafe {
                    sum += u16::from_be(*hdr_ptr.add(i)) as u32;
                }
            }
        }
        
        // Add carry bits
        while (sum >> 16) != 0 {
            sum = (sum & 0xFFFF) + (sum >> 16);
        }
        
        // One's complement
        !(sum as u16)
    }

    /// Send data to the specified destination
    pub async fn send_to(&self, buf: &[u8], addr: SocketAddr) -> Result<usize> {
        let (tx, rx) = oneshot::channel();
        
        self.tx.send(DpdkCommand::Send {
            data: Bytes::copy_from_slice(buf),
            dst: addr,
            reply: tx,
        }).await.map_err(|_| DpdkError::IoError(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "Channel closed",
        )))?;
        
        rx.await.map_err(|_| DpdkError::IoError(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "Reply channel closed",
        )))?
    }

    /// Receive a packet
    pub async fn recv_from(&self) -> Result<(Bytes, SocketAddr)> {
        // Try to receive a packet without waiting first
        {
            let mut receiver = self.receiver.lock().unwrap();
            if let Ok(packet) = receiver.rx.try_recv() {
                return Ok((packet.data, packet.info.src_addr));
            }
            
            // Register waker before releasing the lock
            *receiver.waker.lock().unwrap() = Some(std::task::Context::from_waker(futures::task::noop_waker_ref()).waker().clone());
        } // Lock is dropped here
        
        // Use a poll_fn to handle the waiting logic
        let socket_receiver = &self.receiver;
        let future = futures::future::poll_fn(move |cx| {
            let mut receiver = socket_receiver.lock().unwrap();
            
            // Register the waker
            *receiver.waker.lock().unwrap() = Some(cx.waker().clone());
            
            // Try to receive a packet
            match receiver.rx.try_recv() {
                Ok(packet) => Poll::Ready(Ok((packet.data, packet.info.src_addr))),
                Err(mpsc::error::TryRecvError::Empty) => Poll::Pending,
                Err(mpsc::error::TryRecvError::Disconnected) => Poll::Ready(Err(DpdkError::IoError(
                    io::Error::new(io::ErrorKind::BrokenPipe, "Channel closed"),
                ))),
            }
        });
        
        future.await
    }

    /// Get the local address
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Get the socket type
    pub fn socket_type(&self) -> SocketType {
        self.socket_type
    }
}

impl Drop for DpdkSocket {
    fn drop(&mut self) {
        // Send close command to worker
        let _ = self.tx.try_send(DpdkCommand::Close);
    }
}

/// TCP-specific functionality for DPDK sockets
pub struct DpdkTcpStream {
    socket: DpdkSocket,
    peer_addr: SocketAddr,
    read_buffer: BytesMut,
}

impl DpdkTcpStream {
    /// Connect to a remote TCP endpoint
    pub async fn connect(addr: SocketAddr) -> Result<Self> {
        // Bind to a random local port
        let local_addr = SocketAddr::new(
            IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            0, // Use a random port for outgoing connections
        );
        
        let socket = DpdkSocket::bind_tcp(local_addr).await?;
        
        // TODO: Implement proper TCP connection establishment
        // For a complete implementation, we would:
        // 1. Send SYN packet
        // 2. Wait for SYN-ACK
        // 3. Send ACK
        
        // For this simplified version, we'll just assume the connection is established
        
        Ok(Self {
            socket,
            peer_addr: addr,
            read_buffer: BytesMut::new(),
        })
    }

    /// Get the peer address
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer_addr
    }

    /// Get the local address
    pub fn local_addr(&self) -> SocketAddr {
        self.socket.local_addr()
    }
}

impl AsyncRead for DpdkTcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Get a mutable reference to the unpinned self
        let this = self.get_mut();
        
        // If there's data in the buffer, return it
        if !this.read_buffer.is_empty() {
            let n = std::cmp::min(buf.remaining(), this.read_buffer.len());
            buf.put_slice(&this.read_buffer[..n]);
            
            // Use Buf trait to advance the buffer position
            let mut temp = std::mem::take(&mut this.read_buffer);
            temp.advance(n);
            this.read_buffer = temp;
            
            return Poll::Ready(Ok(()));
        }
        
        // Create a recv_from future
        // We use polling directly to avoid borrowing issues
        let mut recv_fut = this.socket.recv_from();
        
        // Safety: This is safe because we won't use this future after this point
        let recv_fut = unsafe { Pin::new_unchecked(&mut recv_fut) };
        
        // Poll the future
        match recv_fut.poll(cx) {
            Poll::Ready(Ok((data, _))) => {
                let n = std::cmp::min(buf.remaining(), data.len());
                buf.put_slice(&data[..n]);
                
                // If we received more data than can fit in the buffer, store the rest
                if data.len() > n {
                    this.read_buffer.extend_from_slice(&data[n..]);
                }
                
                Poll::Ready(Ok(()))
            },
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
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
        
        // Create the send_to future directly
        let mut fut = this.socket.send_to(buf, this.peer_addr);
        
        // Safety: This is safe because we won't use fut after this point
        let pinned_fut = unsafe { Pin::new_unchecked(&mut fut) };
        
        match pinned_fut.poll(cx) {
            Poll::Ready(Ok(n)) => Poll::Ready(Ok(n)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(io::Error::new(io::ErrorKind::Other, e))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // DPDK does not buffer, so flush is a no-op
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // TODO: Implement proper TCP connection teardown
        // For a complete implementation, we would:
        // 1. Send FIN packet
        // 2. Wait for FIN-ACK
        
        Poll::Ready(Ok(()))
    }
}

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
