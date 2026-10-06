// ─── Session Lifecycle State Machine ────────────────────────────────────────

/// Session lifecycle states per RFC 0005 §1.1.
///
/// The state machine progresses through these states in response to protocol
/// events (stream open/close, SessionInit/Resume, heartbeat timeout, etc.).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// TCP/TLS establishment in progress. Initial state when gRPC stream is opened.
    Connecting,
    /// SessionInit received, validating credentials and capabilities.
    Handshaking,
    /// Bidirectional stream is open and agent is active.
    Active,
    /// Graceful close: agent sent SessionClose, waiting for stream termination.
    Disconnecting,
    /// Stream terminated. Leases are orphaned if previously Active.
    Closed,
    /// Agent is reconnecting within the grace period using a resume token.
    Resuming,
}

impl SessionState {
    /// Human-readable label for logging.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Connecting => "Connecting",
            Self::Handshaking => "Handshaking",
            Self::Active => "Active",
            Self::Disconnecting => "Disconnecting",
            Self::Closed => "Closed",
            Self::Resuming => "Resuming",
        }
    }
}
