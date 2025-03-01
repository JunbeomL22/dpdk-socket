use dpdk_socket::{DpdkOptions, DpdkTcpStream, init};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize DPDK with default options
    let options = DpdkOptions {
        port_id: 0,
        num_rx_queues: 1,
        num_tx_queues: 1,
        args: vec![
            "tcp-client".to_string(),
            "-l".to_string(), "0-3".to_string(),
            "-n".to_string(), "4".to_string(),
            "--proc-type=auto".to_string(),
        ],
    };
    
    init(options)?;
    println!("DPDK initialized successfully");
    
    // Connect to server
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 8080);
    let mut stream = DpdkTcpStream::connect(server_addr).await?;
    println!("Connected to server: {}", stream.peer_addr());
    println!("Local address: {}", stream.local_addr());
    
    // Send request
    let request = b"Hello, DPDK TCP!";
    stream.write_all(request).await?;
    println!("Sent request: {:?}", String::from_utf8_lossy(request));
    
    // Read response
    let mut buffer = vec![0u8; 1024];
    let n = stream.read(&mut buffer).await?;
    println!("Received response: {:?}", String::from_utf8_lossy(&buffer[..n]));
    
    Ok(())
}