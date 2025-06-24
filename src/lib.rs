mod auth;
pub mod config;
mod coordination;
/**
 * MCP Proxy Library
 *
 * A Rust implementation of the MCP proxy that provides:
 * 1. HTTP client that connects to a remote HTTP/SSE server and exposes it as a stdio server
 * 2. HTTP server that connects to a local stdio server and exposes it via HTTP streaming (/message) and SSE (/sse) endpoints
 */
pub mod proxy_handler;
pub mod sse_client;
pub mod sse_server;
mod utils;

// Export main functions
pub use self::sse_client::run_sse_client;
pub use self::sse_server::run_sse_server;
