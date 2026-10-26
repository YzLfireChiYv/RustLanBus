#![forbid(unsafe_op_in_unsafe_fn)]

//! Allocation-bounded IPv4 UDP primitives for the V2 LAN media path.
//!
//! The socket wrapper owns one bound socket, keeps address reuse disabled,
//! requests IPv4 Don't Fragment, and makes blocking behavior explicit. The
//! datagram pool allocates all packet storage at construction; leasing,
//! receiving, sending, and recycling slots do not allocate.

mod platform;
mod pool;
mod socket;

pub use pool::{DatagramLease, DatagramLengthError, DatagramPool, MAX_UDP_APPLICATION_BYTES};
pub use socket::{
    BindError, BindStage, DS_FIELD_EF, DSCP_EF, DatagramClass, IoMode, LanUdpSocket,
    LanUdpSocketConfig, MAX_IPV4_PACKET_BYTES, MtuReport, OsFailure, OsFailureKind, QosError,
    QosErrorStage, QosReport, ReceiveFromError, ReceivedDatagram, ReportError, ReportStage,
    SendToError, SocketBufferDirection, SocketBufferReport, SocketReport, TimeoutDirection,
};
