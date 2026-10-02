//! # tze_hud_protocol
//!
//! gRPC protocol layer for tze_hud. Defines the service, server implementation,
//! and client helpers for agent communication.

pub mod auth;
pub mod convert;
pub mod dedup;
pub mod lease;
pub mod session;
pub mod session_server;
pub mod subscriptions;
pub mod token;

/// Generated protobuf types and gRPC service definitions.
///
/// Types from `types.proto` and `events.proto` are in the top-level module
/// (package `tze_hud.protocol.v1`). The `session` submodule contains the
/// bidirectional streaming session protocol types from `session.proto`
/// (package `tze_hud.protocol.v1.session`).
pub mod proto {
    tonic::include_proto!("tze_hud.protocol.v1");

    /// Generated types for the bidirectional streaming session protocol (RFC 0005).
    pub mod session {
        tonic::include_proto!("tze_hud.protocol.v1.session");
    }
}
