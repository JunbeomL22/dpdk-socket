use dpdk_socket::{DpdkOptions, DpdkTcpStream, TcpParams, TcpState, init};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::{sleep, Duration},
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging (useful for debugging)
    tracing::info!("Starting TCP client example...");
    
    // Initialize DPDK with default options
    let options = DpdkOptions {
        port_id: 0,
        num_rx_queues: 1,
        num_tx_queues: 1,
        bind_pci_device: false, // Set to true if you want to bind to a specific PCI device
        pci_addr: "0000:02:00.1".to_string(), // Example PCI address, change as needed
        args: vec![
            "tcp-client".to_string(),
            "-l".to_string(), "0-3".to_string(),
            "-n".to_string(), "4".to_string(),
            "--proc-type=auto".to_string(),
        ],
    };
    
    init(options)?;
    println!("DPDK initialized successfully");
    
    // Configure TCP parameters
    let tcp_params = TcpParams {
        max_retries: 3,
        connect_timeout_ms: 2000,
        keepalive_interval_ms: 5000,
        keepalive_timeout_ms: 1000,
        ..Default::default()
    };
    
    // Connect to server with specific local address
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 8080);
    let local_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 0); // 0 = random port
    
    println!("Connecting to {}...", server_addr);
    
    // Use the new connect_with_options function
    let mut stream = DpdkTcpStream::connect_with_options(
        server_addr,
        Some(local_addr),
        Some(tcp_params)
    ).await?;
    
    println!("Connected to server: {}", stream.peer_addr());
    println!("Local address: {}", stream.local_addr());
    println!("Connection state: {:?}", stream.state());
    
    // Send request
    let request = b"Hello, DPDK TCP!";
    stream.write_all(request).await?;
    println!("Sent request: {:?}", String::from_utf8_lossy(request));
    
    // Read response
    let mut buffer = vec![0u8; 1024];
    let n = stream.read(&mut buffer).await?;
    println!("Received response: {:?}", String::from_utf8_lossy(&buffer[..n]));
    
    // Test automatic reconnection
    println!("\nTesting automatic reconnection...");
    
    // First, demonstrate explicit reconnection
    println!("1. First, let's simulate connection loss and explicit reconnection");
    
    // Manually set connection state to closed to simulate a connection loss
    // In a real scenario, this would happen due to network issues or timeout
    // We intentionally break the existing connection
    stream.set_state(TcpState::Closed);
    
    println!("Connection state after loss: {:?}", stream.state());
    
    // Explicitly reconnect
    println!("Explicitly reconnecting...");
    match stream.reconnect().await {
        Ok(_) => println!("Explicit reconnection successful"),
        Err(e) => println!("Explicit reconnection failed: {}", e),
    }
    
    // Now test automatic reconnection
    println!("\n2. Now testing automatic reconnection");
    println!("Simulating connection loss...");
    
    // Manually break connection again
    stream.set_state(TcpState::Closed);
    println!("Connection state: {:?}", stream.state());
    
    // Try sending data - should automatically reconnect
    println!("Sending data (should auto-reconnect)...");
    let request = b"Hello after auto-reconnect!";
    stream.write_all(request).await?;
    println!("Auto-reconnection worked! Data sent successfully.");
    
    // Read response
    let n = stream.read(&mut buffer).await?;
    println!("Received response: {:?}", String::from_utf8_lossy(&buffer[..n]));
    
    // Test connection to another server with the new simpler API
    println!("\n3. Testing simplified connection API");
    
    // Using the simple connect method to a different server
    let another_server = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 3)), 8080);
    println!("Connecting to {} using the simplified API...", another_server);
    
    // This may fail if the server doesn't exist, but it's just to demonstrate the API
    match DpdkTcpStream::connect(another_server).await {
        Ok(mut new_stream) => {
            println!("Connected to {} successfully!", another_server);
            
            // Send a ping
            new_stream.write_all(b"Ping with simple API").await?;
            
            // Clean up
            new_stream.shutdown().await?;
        },
        Err(e) => {
            println!("Couldn't connect to secondary server (as expected if it doesn't exist): {}", e);
        }
    }
    
    // Clean shutdown with proper FIN
    println!("\nPerforming clean shutdown of main connection...");
    stream.shutdown().await?;
    println!("Connection closed gracefully");
    
    // Sleep a bit to let the FIN packet be sent
    sleep(Duration::from_millis(100)).await;
    
    Ok(())
}