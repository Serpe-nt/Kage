//! Why a call to a sidecar failed.

use crate::contract::{ErrorCode, FlowEvent};

/// A failed call to a sidecar.
///
/// The variants tell apart what a scheduler must tell apart: the sidecar
/// was never reached (try again, or another container), it was reached and
/// refused (a contract error with a stable code), or something went wrong
/// after the request was sent (the visit may have happened).
#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    /// The client could not be built from its settings.
    #[error("the sidecar client cannot be set up: {0}")]
    Setup(String),
    /// No connection to the sidecar could be made, after every retry. The
    /// request was never sent.
    #[error("the sidecar at {url} could not be reached after {attempts} attempt(s): {source}")]
    Connect {
        /// The sidecar's base URL.
        url: String,
        /// How often the connection was tried.
        attempts: u32,
        /// The last connection error.
        #[source]
        source: wreq::Error,
    },
    /// The sidecar did not answer in time. The request was sent: for a
    /// navigation the visit may have happened.
    #[error("the sidecar did not answer `{operation}` in time")]
    Timeout {
        /// The operation of the contract that was called.
        operation: &'static str,
    },
    /// The connection failed after the request was sent.
    #[error("the connection to the sidecar failed during `{operation}`: {source}")]
    Transport {
        /// The operation of the contract that was called.
        operation: &'static str,
        /// The error.
        #[source]
        source: wreq::Error,
    },
    /// The sidecar refused the request with an error of the contract.
    #[error("the sidecar refused `{operation}` with {code} ({status}): {message}")]
    Refused {
        /// The operation of the contract that was called.
        operation: &'static str,
        /// The HTTP status.
        status: u16,
        /// The contract's stable error code.
        code: ErrorCode,
        /// The sidecar's sentence about it, for the log.
        message: String,
    },
    /// The sidecar answered with something the contract does not describe.
    #[error(
        "the sidecar answered `{operation}` with status {status} and a body that is not the contract's: {detail}"
    )]
    Unexpected {
        /// The operation of the contract that was called.
        operation: &'static str,
        /// The HTTP status.
        status: u16,
        /// What was wrong with the body.
        detail: String,
    },
    /// The answer of a flow ended without its final event: the flow broke
    /// after the sidecar had begun to answer. The steps listed here did
    /// happen; what came after them is not known.
    #[error(
        "the flow was interrupted: the sidecar's answer ended after {} step(s) without a final event",
        .steps.len()
    )]
    FlowInterrupted {
        /// The events received before the answer ended, in order.
        steps: Vec<FlowEvent>,
    },
}

impl BrowserError {
    /// What went wrong, in a word: the contract's code for a refusal, else
    /// what kind of failure it was. For a span or a metric label; it holds
    /// nothing of the request.
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Setup(_) => "setup",
            Self::Connect { .. } => "unreachable",
            Self::Timeout { .. } => "timeout",
            Self::Transport { .. } => "transport",
            Self::Refused { code, .. } => code.as_str(),
            Self::Unexpected { .. } => "unexpected",
            Self::FlowInterrupted { .. } => "flow_interrupted",
        }
    }

    /// The contract's error code, when the sidecar refused the request.
    pub const fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Refused { code, .. } => Some(*code),
            _ => None,
        }
    }

    /// Whether the sidecar was never reached, so that nothing happened and
    /// the call may be made again, here or on another container.
    pub const fn is_unreachable(&self) -> bool {
        matches!(self, Self::Connect { .. })
    }

    /// Whether the sidecar no longer knows the context: it was closed, or
    /// closed itself after being idle. The caller opens it again.
    pub const fn is_context_gone(&self) -> bool {
        matches!(self.code(), Some(ErrorCode::ContextNotFound))
    }

    /// Whether the sidecar has no room for another context right now.
    pub const fn is_at_capacity(&self) -> bool {
        matches!(self.code(), Some(ErrorCode::ContextLimit))
    }

    /// Whether the persona's context is open with other parameters than
    /// were asked for: someone else opened it.
    pub const fn is_context_conflict(&self) -> bool {
        matches!(self.code(), Some(ErrorCode::ContextConflict))
    }

    /// Whether another operation runs on the context: someone else is
    /// using it.
    pub const fn is_context_busy(&self) -> bool {
        matches!(self.code(), Some(ErrorCode::ContextBusy))
    }

    /// Whether the engine bundles another browser major than the request
    /// names: a persona of an older major is moved with
    /// [`crate::SidecarClient::upgrade_persona`] before its context opens.
    pub const fn is_browser_major_mismatch(&self) -> bool {
        matches!(self.code(), Some(ErrorCode::BrowserMajorMismatch))
    }
}
