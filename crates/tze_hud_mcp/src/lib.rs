//! # tze_hud_mcp
//!
//! MCP (Model Context Protocol) compatibility bridge for tze_hud.
//!
//! Implements the compatibility plane: a JSON-RPC 2.0 server that exposes
//! named tools for LLM interaction. This is intentionally NOT the hot path —
//! JSON overhead is acceptable here.
//!
//! ## Architecture
//!
//! The MCP bridge wraps a shared [`SceneGraph`] behind a mutex and translates
//! JSON-RPC tool calls into scene graph mutations. It bridges to the gRPC
//! control plane in spirit (same scene model) but speaks JSON-RPC for maximum
//! LLM compatibility.
//!
//! ## Tools
//!
//! Five verbs over surfaces (`zone:<name>`, `widget:<name>`,
//! `portal:<projection_id>`), specified in `docs/api.md`:
//!
//! | Tool           | Stage                                      |
//! |----------------|--------------------------------------------|
//! | `hud_surfaces` | Discover allowed surfaces and holdings     |
//! | `hud_publish`  | Claim and fill (zone, widget, portal)      |
//! | `hud_hold`     | Renew without resending content            |
//! | `hud_clear`    | Release                                    |
//! | `hud_input`    | Collect and ack portal replies and actions |

pub mod error;
pub mod portal_op;
pub mod schema;
pub mod server;
pub mod tools;
pub mod types;

pub use error::McpError;
pub use portal_op::PortalOp;
pub use server::{CallerContext, McpConfig, McpServer};
pub use types::{McpRequest, McpResponse, McpResult};
