use dpdk_socket::{DpdkOptions, DpdkUdpSocket, init};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::time;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize DPDK with default options
    let options = DpdkOptions {
        port_id: 0,
        num_rx_queues: 1,
        num_tx_queues: 1,
        bind_pci_device: false, // Set to true if you want to bind to a specific PCI device
        pci_addr: "0000:00:08.0".to_string(), // Example PCI address, change as needed
        args: vec![
            "udp-echo".to_string(),
            "-l".to_string(), "0-3".to_string(),
            "-n".to_string(), "4".to_string(),
            "--proc-type=auto".to_string(),
        ],
    };
    
    init(options)?;
    println!("DPDK initialized successfully");
    
    // Bind UDP socket to local address
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 8080);
    let socket = DpdkUdpSocket::bind(addr).await?;
    println!("UDP socket bound to {}", socket.local_addr());
    
    // Echo server loop
    let mut buf = [0u8; 1024];
    loop {
        match socket.recv_from().await {
            Ok((data, src_addr)) => {
                println!("Received {} bytes from {}", data.len(), src_addr);
                
                // Echo back
                match socket.send_to(&data, src_addr).await {
                    Ok(sent) => {
                        println!("Sent {} bytes back to {}", sent, src_addr);
                    }
                    Err(e) => {
                        eprintln!("Failed to send response: {}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!("Error receiving data: {}", e);
                time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}