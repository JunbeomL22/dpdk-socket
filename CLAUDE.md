# DPDK-Socket Project Guidelines

## Build & Test Commands
- Build: `cargo build`
- Run tests: `cargo test`
- Run specific test: `cargo test test_name`
- Build with optimizations: `cargo build --release`
- Run example: `cargo run --example udp_echo`
- Run with additional logging: `RUST_LOG=debug cargo run --example tcp_client`
- Check code format: `cargo fmt --check`
- Apply code formatting: `cargo fmt`
- Run lints: `cargo clippy`

## Code Style Guidelines
- **Formatting**: Follow Rust style conventions with `cargo fmt`
- **Imports**: Group in order: std, external crates, local modules
- **Error Handling**: Use `thiserror` for custom errors, include context
- **Naming**: Use snake_case for functions/variables, CamelCase for types
- **Documentation**: Document public API with examples
- **Unsafe Code**: Minimize unsafe blocks, document safety considerations
- **DPDK FFI**: Use proper error handling for all unsafe DPDK calls
- **Async/Tokio**: Follow tokio best practices for async code
- **Logging**: Use tracing macros (debug, info, warn, error)
- **Testing**: Write unit tests for core functionality