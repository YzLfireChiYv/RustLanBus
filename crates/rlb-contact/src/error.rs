//! Failures returned by the contact library.

use std::io;
use std::net::SocketAddr;

use rlb_model::IdGenerationError;
use thiserror::Error;

use crate::framing::FrameError;
use crate::wire::ContactCode;

/// Failure of configuration, binding, framing, or a control exchange.
#[derive(Debug, Error)]
pub enum ContactError {
    /// The caller supplied a room configuration the service will not start.
    #[error("{0}")]
    InvalidConfig(&'static str),
    /// A local request was incomplete before it reached the service.
    #[error("{0}")]
    InvalidRequest(&'static str),
    /// Binding the requested socket failed. The address is the one the caller supplied.
    #[error("contact bind failed for {address}: {source}")]
    Bind {
        /// Socket address passed to the operating system.
        address: SocketAddr,
        /// Operating-system bind error.
        #[source]
        source: io::Error,
    },
    /// A socket operation failed.
    #[error(transparent)]
    Io(#[from] io::Error),
    /// A frame or its JSON payload was rejected.
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// The service rejected one request. Membership is unchanged by this rejection.
    #[error("{message}")]
    Rejected {
        /// Closed contact error code.
        code: ContactCode,
        /// Fixed explanation. It contains no room password.
        message: String,
    },
    /// The decoded message broke a contact invariant.
    #[error("{0}")]
    Protocol(&'static str),
    /// The TCP connection closed.
    #[error("the contact control connection ended")]
    ControlLost,
    /// The service sent service-stop visibility and closed the control socket.
    #[error("the contact service stopped")]
    Stopped,
    /// A response did not arrive before the caller's deadline.
    #[error("the contact service did not answer before the deadline")]
    TimedOut,
    /// The contact owner thread panicked.
    #[error("the contact owner stopped unexpectedly")]
    OwnerPanic,
    /// The operating system could not supply a new instance identity.
    #[error("the operating-system random source is unavailable")]
    Random(#[from] IdGenerationError),
}

impl ContactError {
    /// Reports a failure of this control connection.
    ///
    /// Established direct media continues. None of these failures is a member departure.
    #[must_use]
    pub const fn is_local_control_failure(&self) -> bool {
        matches!(
            self,
            Self::Io(_)
                | Self::Frame(_)
                | Self::Protocol(_)
                | Self::ControlLost
                | Self::Stopped
                | Self::TimedOut
                | Self::OwnerPanic
        )
    }
}
