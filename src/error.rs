//! Error types for MetalBond operations.

use thiserror::Error;

/// Errors that can occur in MetalBond operations.
#[derive(Debug, Error)]
pub enum Error {
    /// Connection error.
    #[error("connection error: {0}")]
    Connection(#[from] std::io::Error),

    /// Protocol error (invalid message, version mismatch, etc.).
    #[error("protocol error: {0}")]
    Protocol(String),

    /// Message encoding/decoding error.
    #[error("encoding error: {0}")]
    Encoding(#[from] prost::DecodeError),

    /// The connection is not in the established state.
    #[error("connection not established")]
    NotEstablished,

    /// The connection is closed.
    #[error("connection closed")]
    Closed,

    /// Timeout waiting for response.
    #[error("timeout")]
    Timeout,

    /// Already subscribed to this VNI.
    #[error("already subscribed to VNI {0}")]
    AlreadySubscribed(u32),

    /// Not subscribed to this VNI.
    #[error("not subscribed to VNI {0}")]
    NotSubscribed(u32),

    /// Route already announced.
    #[error("route already announced")]
    RouteAlreadyAnnounced,

    /// Route not found.
    #[error("route not found")]
    RouteNotFound,
}
