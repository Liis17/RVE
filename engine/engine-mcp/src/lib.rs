//! Local HTTP MCP server primitives shared by the editor host.

pub mod http;
pub mod protocol;
pub mod server;
pub mod tools;

pub use protocol::SUPPORTED_PROTOCOL_VERSIONS;
pub use server::{McpConfig, McpServer, ToolCall};
pub use tools::{Permission, Permissions, ToolResult, ToolSpec};

/// Version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
