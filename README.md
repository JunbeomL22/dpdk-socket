# DPDK Socket

A high-level async UDP/TCP socket library for Rust using DPDK (Data Plane Development Kit).

## Features

- Async API built on Tokio
- UDP and TCP socket support
- Raw packet handling through DPDK
- High-performance networking

## Prerequisites

- DPDK 21.11 or later installed on your system
- Rust 1.60 or later
- pkg-config

## Installation

Add this to your `Cargo.toml`:

```toml
[dependencies]
dpdk-socket = "0.1.0"
```

## Usage

### Initializing DPDK

Before using any socket functions, you need to initialize DPDK:

```rust
use dpdk_socket::{DpdkOptions, init};

// Initialize DPDK with default options
let options = DpdkOptions::default();
init(options)?;
```

### UDP Example

```rust
use dpdk_socket::{DpdkUdpSocket, DpdkOptions, init};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize DPDK
    let options = DpdkOptions::default();
    init(options)?;
    
    // Create a UDP socket
    let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 8080);
    let socket = DpdkUdpSocket::bind(addr).await?;
    
    // Send data
    let remote_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 8080);
    socket.send_to(b"Hello, DPDK!", remote_addr).await?;
    
    // Receive data
    let (data, src_addr) = socket.recv_from().await?;
    println!("Received {} bytes from {}: {:?}", 
             data.len(), src_addr, String::from_utf8_lossy(&data));
    
    Ok(())
}
```

### TCP Example

```rust
use dpdk_socket::{DpdkTcpStream, DpdkOptions, init};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize DPDK
    let options = DpdkOptions::default();
    init(options)?;
    
    // Connect to server
    let server_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2)), 8080);
    let mut stream = DpdkTcpStream::connect(server_addr).await?;
    
    // Send request
    stream.write_all(b"Hello, DPDK TCP!").await?;
    
    // Read response
    let mut buffer = vec![0u8; 1024];
    let n = stream.read(&mut buffer).await?;
    println!("Received: {:?}", String::from_utf8_lossy(&buffer[..n]));
    
    Ok(())
}
```

## Configuration

You can customize DPDK initialization:

```rust
let options = DpdkOptions {
    port_id: 0,
    num_rx_queues: 2,
    num_tx_queues: 2,
    args: vec![
        "your-app-name".to_string(),
        "-l".to_string(), "0-3".to_string(),
        "-n".to_string(), "4".to_string(),
        "--proc-type=auto".to_string(),
    ],
};
```

## Notes on DPDK Setup

Before running applications that use this library, you need to:

1. Set up huge pages:
   ```bash
   sudo mkdir -p /dev/hugepages
   sudo mountpoint -q /dev/hugepages || sudo mount -t hugetlbfs nodev /dev/hugepages
   sudo echo 1024 > /sys/devices/system/node/node0/hugepages/hugepages-2048kB/nr_hugepages
   ```

2. Bind network interfaces to DPDK-compatible drivers:
   ```bash
   sudo dpdk-devbind.py --status
   sudo dpdk-devbind.py -b uio_pci_generic 0000:03:00.0
   ```

## Using With Specific NICs

If you have multiple NICs and want to use DPDK with a specific one (e.g., Intel NIC but not Realtek):

1. First, identify your NICs and their PCI addresses:
   ```bash
   lspci | grep Ethernet
   ```

2. Bind only the Intel NIC to DPDK driver, leaving your Realtek NIC for regular OS use:
   ```bash
   sudo dpdk-devbind.py --status                         # List all NICs
   sudo dpdk-devbind.py -b uio_pci_generic <INTEL_PCI_ADDRESS>  # Bind only Intel NIC
   ```

3. Configure the library to use the specific NIC:
   ```rust
   let options = DpdkOptions {
       port_id: 0,  // Use first DPDK-bound port (your Intel NIC)
       args: vec![
           "your-app".to_string(),
           "-l".to_string(), "0-3".to_string(),
           "--allow".to_string(), "<INTEL_PCI_ADDRESS>".to_string(),  // Explicitly allow this NIC
       ],
   };
   init(options)?;
   ```

4. You can verify which ports are available to DPDK with:
   ```bash
   sudo dpdk-devbind.py --status
   ```

3. Environment Variables for DPDK:

   ```bash
   # Enable debug logging
   export RUST_LOG=debug

   # DPDK EAL options can be configured via the application
   # (or by setting these env vars)
   export DPDK_SOCKET_PORT_ID=0  # Override default port ID
   export DPDK_SOCKET_CORES="0-3"  # Override default core mask
   export DPDK_SOCKET_MEMORY=1024  # Memory in MB for DPDK

   # If using different DPDK versions
   export PKG_CONFIG_PATH=/path/to/custom/dpdk/lib/pkgconfig
   ```

4. Required User Permissions:

   Your user needs proper permissions to access DPDK devices and huge pages:

   ```bash
   # Add your user to the required groups
   sudo usermod -a -G hugetlbfs $USER
   sudo usermod -a -G dpdk $USER

   # For development, you can run with sudo -E to preserve env vars
   sudo -E cargo run --example udp_echo
   ```

5. Mock Mode for Development:

   This library includes a "mock mode" for development without real DPDK hardware:
   
   ```bash
   # Enable mock mode in build.rs by setting:
   let link_real_dpdk = false;
   
   # Or create a feature flag in Cargo.toml:
   [features]
   mock = []
   
   # And in build.rs:
   #[cfg(feature = "mock")]
   let link_real_dpdk = false;
   #[cfg(not(feature = "mock"))]
   let link_real_dpdk = true;
   ```
   
   This allows you to test and develop the high-level API without needing actual DPDK hardware
   or interfaces, which is useful for CI/CD environments or development machines.

## License

This project is licensed under either of:

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.