//! Local HTTP MCP server primitives shared by the editor host.

pub mod http;
pub mod protocol;
pub mod server;
pub mod tools;

pub use protocol::SUPPORTED_PROTOCOL_VERSIONS;
pub use server::{McpConfig, McpServer, ToolCall};
pub use tools::{Permission, Permissions, ToolResult, ToolSpec};

/// Хеш токена доступа, который хранится вместо самого токена: blake3 в нижнем hex.
///
/// Токен — случайные 32 байта, поэтому быстрого хеша без соли достаточно: подобрать исходное
/// значение по хешу нельзя, а файл с хешем не даёт доступа к серверу.
pub fn token_hash(token: &str) -> String {
    blake3::hash(token.as_bytes()).to_hex().to_string()
}

/// Version of this crate.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
