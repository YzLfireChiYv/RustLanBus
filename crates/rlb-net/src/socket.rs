use std::fmt;
use std::io;
use std::mem::MaybeUninit;
use std::net::{SocketAddr, SocketAddrV4};
use std::num::NonZeroUsize;
use std::ops::Deref;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rlb_model::{Endpoint, ErrorCode, QosPolicy};
use socket2::{Domain, MaybeUninitSlice, Protocol, Socket, Type};

use crate::platform;
use crate::pool::{DatagramLease, DatagramPool, MAX_UDP_APPLICATION_BYTES};

const MAX_IPV4_HEADER_BYTES: usize = 60;
const UDP_HEADER_BYTES: usize = 8;

/// Expedited Forwarding DSCP codepoint used by V2 media.
pub const DSCP_EF: u8 = 46;
/// IPv4 DS field for EF 46 with ECN cleared.
pub const DS_FIELD_EF: u8 = DSCP_EF << 2;
/// Worst-case IPv4 packet size for a maximum-size V2 UDP application payload.
pub const MAX_IPV4_PACKET_BYTES: usize =
    MAX_IPV4_HEADER_BYTES + UDP_HEADER_BYTES + MAX_UDP_APPLICATION_BYTES;

/// Whether one send contains media/recovery traffic or ordinary control.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DatagramClass {
    /// MEDIA, GAP, EPOCH_END, NACK, or retransmitted MEDIA.
    Media,
    /// Direct handshake, subscription, Registry binding, or diagnostics control.
    Control,
}

/// Explicit socket waiting behavior.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoMode {
    /// Every send and receive returns immediately when the socket cannot progress.
    Nonblocking,
    /// Blocking calls have finite, independently configured timeouts.
    Timed {
        /// Maximum receive wait.
        receive_timeout: Duration,
        /// Maximum send wait.
        send_timeout: Duration,
    },
}

impl IoMode {
    #[must_use]
    const fn is_nonblocking(self) -> bool {
        matches!(self, Self::Nonblocking)
    }
}

/// Construction settings for one exclusively bound IPv4 UDP socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LanUdpSocketConfig {
    local_address: SocketAddrV4,
    io_mode: IoMode,
    receive_buffer_bytes: Option<NonZeroUsize>,
    send_buffer_bytes: Option<NonZeroUsize>,
    initial_media_qos: QosPolicy,
}

impl LanUdpSocketConfig {
    /// Creates a configuration with OS-default buffer sizes and DSCP 0.
    ///
    /// The room policy can be applied later with
    /// `LanUdpSocket::set_media_qos_policy`.
    #[must_use]
    pub const fn new(local_address: SocketAddrV4, io_mode: IoMode) -> Self {
        Self {
            local_address,
            io_mode,
            receive_buffer_bytes: None,
            send_buffer_bytes: None,
            initial_media_qos: QosPolicy::Off,
        }
    }

    /// Requests a receive socket buffer size.
    #[must_use]
    pub const fn with_receive_buffer_bytes(mut self, bytes: NonZeroUsize) -> Self {
        self.receive_buffer_bytes = Some(bytes);
        self
    }

    /// Requests a send socket buffer size.
    #[must_use]
    pub const fn with_send_buffer_bytes(mut self, bytes: NonZeroUsize) -> Self {
        self.send_buffer_bytes = Some(bytes);
        self
    }

    /// Selects the media QoS policy applied during construction.
    #[must_use]
    pub const fn with_initial_media_qos(mut self, policy: QosPolicy) -> Self {
        self.initial_media_qos = policy;
        self
    }

    /// Returns the requested local bind address.
    #[must_use]
    pub const fn local_address(self) -> SocketAddrV4 {
        self.local_address
    }

    /// Returns the explicit I/O mode.
    #[must_use]
    pub const fn io_mode(self) -> IoMode {
        self.io_mode
    }
}

/// Direction of an invalid timeout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimeoutDirection {
    /// Receive timeout.
    Receive,
    /// Send timeout.
    Send,
}

/// Direction of a socket buffer request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketBufferDirection {
    /// Receive buffer.
    Receive,
    /// Send buffer.
    Send,
}

/// Stage at which socket construction failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindStage {
    /// Create the IPv4 datagram socket.
    CreateSocket,
    /// Disable `SO_REUSEADDR`.
    DisableAddressReuse,
    /// Enable Windows `SO_EXCLUSIVEADDRUSE`.
    EnableExclusiveAddressUse,
    /// Enable IPv4 Don't Fragment.
    EnableDontFragment,
    /// Set blocking or nonblocking mode.
    SetNonblocking,
    /// Set the receive timeout.
    SetReceiveTimeout,
    /// Set the send timeout.
    SetSendTimeout,
    /// Request the receive socket buffer.
    SetReceiveBuffer,
    /// Request the send socket buffer.
    SetSendBuffer,
    /// Set the initial media DS field.
    SetInitialQos,
    /// Bind the local IPv4 endpoint.
    Bind,
    /// Query the effective local endpoint.
    QueryLocalAddress,
}

/// A closed operating-system failure category plus its platform code.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OsFailure {
    /// Portable category captured from `std::io::Error`.
    pub kind: OsFailureKind,
    /// Winsock or errno value, when supplied by the platform.
    pub raw_os_error: Option<i32>,
}

impl OsFailure {
    fn capture(error: &io::Error) -> Self {
        Self {
            kind: OsFailureKind::from(error.kind()),
            raw_os_error: error.raw_os_error(),
        }
    }
}

impl fmt::Display for OsFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.raw_os_error {
            Some(code) => write!(formatter, "{:?} (OS error {code})", self.kind),
            None => write!(formatter, "{:?}", self.kind),
        }
    }
}

/// Portable error categories exposed by the UDP boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OsFailureKind {
    /// Address is already owned.
    AddressInUse,
    /// Requested local address is unavailable.
    AddressNotAvailable,
    /// Operation lacks OS permission.
    PermissionDenied,
    /// Network has no route.
    NetworkUnreachable,
    /// Target host is unreachable.
    HostUnreachable,
    /// Target actively refused the operation.
    ConnectionRefused,
    /// Existing association was reset.
    ConnectionReset,
    /// Existing association was aborted.
    ConnectionAborted,
    /// Socket is not connected where the OS required it.
    NotConnected,
    /// Underlying endpoint is closed.
    BrokenPipe,
    /// Nonblocking operation cannot progress.
    WouldBlock,
    /// OS timeout expired.
    TimedOut,
    /// System call was interrupted.
    Interrupted,
    /// Parameter was rejected.
    InvalidInput,
    /// Received OS data was invalid.
    InvalidData,
    /// Platform does not implement the operation.
    Unsupported,
    /// OS could not reserve memory.
    OutOfMemory,
    /// Remaining platform failures.
    Other,
}

impl From<io::ErrorKind> for OsFailureKind {
    fn from(kind: io::ErrorKind) -> Self {
        match kind {
            io::ErrorKind::AddrInUse => Self::AddressInUse,
            io::ErrorKind::AddrNotAvailable => Self::AddressNotAvailable,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::NetworkUnreachable => Self::NetworkUnreachable,
            io::ErrorKind::HostUnreachable => Self::HostUnreachable,
            io::ErrorKind::ConnectionRefused => Self::ConnectionRefused,
            io::ErrorKind::ConnectionReset => Self::ConnectionReset,
            io::ErrorKind::ConnectionAborted => Self::ConnectionAborted,
            io::ErrorKind::NotConnected => Self::NotConnected,
            io::ErrorKind::BrokenPipe => Self::BrokenPipe,
            io::ErrorKind::WouldBlock => Self::WouldBlock,
            io::ErrorKind::TimedOut => Self::TimedOut,
            io::ErrorKind::Interrupted => Self::Interrupted,
            io::ErrorKind::InvalidInput => Self::InvalidInput,
            io::ErrorKind::InvalidData => Self::InvalidData,
            io::ErrorKind::Unsupported => Self::Unsupported,
            io::ErrorKind::OutOfMemory => Self::OutOfMemory,
            _ => Self::Other,
        }
    }
}

/// Failure to construct an exclusively bound, DF-enabled UDP socket.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BindError {
    /// Timed mode requires a positive finite duration for each direction.
    ZeroTimeout {
        /// Direction whose duration was zero.
        direction: TimeoutDirection,
    },
    /// A requested socket buffer cannot be represented by the platform API.
    SocketBufferTooLarge {
        /// Requested buffer direction.
        direction: SocketBufferDirection,
        /// Rejected byte count.
        requested: usize,
        /// Largest accepted byte count.
        maximum: usize,
    },
    /// A system call failed during a named construction stage.
    OperatingSystem {
        /// Construction stage.
        stage: BindStage,
        /// Closed OS failure.
        failure: OsFailure,
    },
    /// The OS returned a non-IPv4 address for an IPv4 socket.
    UnexpectedLocalAddressFamily,
    /// Effective address ownership options did not enforce single-owner binding.
    SingleOwnerNotEffective {
        /// Queried `SO_REUSEADDR` state.
        reuse_address_enabled: bool,
        /// Queried Windows exclusive-address state.
        exclusive_address_use: Option<bool>,
    },
    /// IPv4 Don't Fragment did not remain enabled.
    DontFragmentNotEffective,
    /// Effective socket settings could not be queried.
    Report(ReportError),
}

impl BindError {
    /// Returns the stable V2 error when this is an MTU-path failure.
    #[must_use]
    pub const fn stable_code(self) -> Option<ErrorCode> {
        match self {
            Self::DontFragmentNotEffective => Some(ErrorCode::MediaPathMtuUnsupported),
            Self::OperatingSystem {
                stage: BindStage::EnableDontFragment,
                ..
            } => Some(ErrorCode::MediaPathMtuUnsupported),
            Self::Report(ReportError::OperatingSystem {
                stage: ReportStage::DontFragment,
                ..
            }) => Some(ErrorCode::MediaPathMtuUnsupported),
            _ => None,
        }
    }

    fn os(stage: BindStage, error: io::Error) -> Self {
        Self::OperatingSystem {
            stage,
            failure: OsFailure::capture(&error),
        }
    }
}

impl fmt::Display for BindError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroTimeout { direction } => {
                write!(formatter, "{direction:?} timeout must be greater than zero")
            }
            Self::SocketBufferTooLarge {
                direction,
                requested,
                maximum,
            } => write!(
                formatter,
                "{direction:?} socket buffer {requested} exceeds {maximum} bytes"
            ),
            Self::OperatingSystem { stage, failure } => {
                write!(formatter, "UDP bind stage {stage:?} failed: {failure}")
            }
            Self::UnexpectedLocalAddressFamily => {
                formatter.write_str("IPv4 socket returned a non-IPv4 local address")
            }
            Self::SingleOwnerNotEffective {
                reuse_address_enabled,
                exclusive_address_use,
            } => write!(
                formatter,
                "single-owner options ineffective: reuse={reuse_address_enabled}, exclusive={exclusive_address_use:?}"
            ),
            Self::DontFragmentNotEffective => {
                formatter.write_str("IPv4 Don't Fragment is not effective")
            }
            Self::Report(error) => write!(formatter, "socket report failed: {error}"),
        }
    }
}

impl std::error::Error for BindError {}

/// Stage of a QoS socket-option failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QosErrorStage {
    /// Apply a new room media policy.
    SetPolicy,
    /// Apply one packet's media/control class.
    ApplyDatagramClass,
    /// Restore the configured media policy after an ordinary control send.
    RestorePolicy,
    /// Query the effective DS field.
    Query,
}

/// Closed failure returned while configuring or querying DSCP.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QosError {
    /// A DS-field socket operation failed.
    OperatingSystem {
        /// Failed operation.
        stage: QosErrorStage,
        /// Closed OS failure.
        failure: OsFailure,
    },
    /// The OS returned a value outside the eight-bit IPv4 DS field.
    ValueOutOfRange {
        /// Rejected OS value.
        actual: u32,
    },
}

impl QosError {
    fn os(stage: QosErrorStage, error: io::Error) -> Self {
        Self::OperatingSystem {
            stage,
            failure: OsFailure::capture(&error),
        }
    }
}

impl fmt::Display for QosError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OperatingSystem { stage, failure } => {
                write!(formatter, "QoS stage {stage:?} failed: {failure}")
            }
            Self::ValueOutOfRange { actual } => {
                write!(formatter, "OS returned invalid IPv4 DS field {actual}")
            }
        }
    }
}

impl std::error::Error for QosError {}

/// Effective packet-marking state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QosReport {
    /// Current room media policy.
    pub requested_policy: QosPolicy,
    /// DS field required by that policy.
    pub requested_ds_field: u8,
    /// DS field read back from the socket.
    pub actual_ds_field: u8,
}

impl QosReport {
    /// Returns whether the queried socket mark matches the policy exactly.
    #[must_use]
    pub const fn is_effective(self) -> bool {
        self.requested_ds_field == self.actual_ds_field
    }
}

/// Socket option queried for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportStage {
    /// `SO_REUSEADDR`.
    AddressReuse,
    /// Windows `SO_EXCLUSIVEADDRUSE`.
    ExclusiveAddressUse,
    /// IPv4 Don't Fragment.
    DontFragment,
    /// Receive socket buffer.
    ReceiveBuffer,
    /// Send socket buffer.
    SendBuffer,
    /// Receive timeout.
    ReceiveTimeout,
    /// Send timeout.
    SendTimeout,
    /// IPv4 DS field.
    Qos,
}

/// Closed diagnostics-query failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReportError {
    /// One socket option could not be queried.
    OperatingSystem {
        /// Failed query.
        stage: ReportStage,
        /// Closed OS failure.
        failure: OsFailure,
    },
    /// The OS returned a value outside the eight-bit IPv4 DS field.
    QosValueOutOfRange {
        /// Rejected OS value.
        actual: u32,
    },
}

impl ReportError {
    fn os(stage: ReportStage, error: io::Error) -> Self {
        Self::OperatingSystem {
            stage,
            failure: OsFailure::capture(&error),
        }
    }
}

impl fmt::Display for ReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OperatingSystem { stage, failure } => {
                write!(formatter, "socket report stage {stage:?} failed: {failure}")
            }
            Self::QosValueOutOfRange { actual } => {
                write!(formatter, "OS returned invalid IPv4 DS field {actual}")
            }
        }
    }
}

impl std::error::Error for ReportError {}

/// Requested and effective kernel socket-buffer sizes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketBufferReport {
    /// Requested bytes, or `None` when the OS default was retained.
    pub requested_bytes: Option<usize>,
    /// Value returned by `getsockopt`.
    pub actual_bytes: usize,
}

/// Effective MTU and fragmentation strategy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MtuReport {
    /// Maximum RustLanBus UDP application payload.
    pub max_udp_application_bytes: usize,
    /// Worst-case 60-byte IPv4 header plus UDP header and application payload.
    pub worst_case_ipv4_packet_bytes: usize,
    /// Whether the DF option was requested.
    pub dont_fragment_requested: bool,
    /// Whether the DF option was read back as enabled.
    pub dont_fragment_effective: bool,
}

/// Queried socket state suitable for low-frequency diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SocketReport {
    /// Effective local bind address.
    pub local_address: SocketAddrV4,
    /// Explicit mode selected at construction.
    pub io_mode: IoMode,
    /// Effective receive timeout.
    pub actual_receive_timeout: Option<Duration>,
    /// Effective send timeout.
    pub actual_send_timeout: Option<Duration>,
    /// Kernel receive-buffer report.
    pub receive_buffer: SocketBufferReport,
    /// Kernel send-buffer report.
    pub send_buffer: SocketBufferReport,
    /// Effective `SO_REUSEADDR` state.
    pub reuse_address_enabled: bool,
    /// Effective Windows `SO_EXCLUSIVEADDRUSE`, absent on other platforms.
    pub exclusive_address_use: Option<bool>,
    /// DF and MTU limits.
    pub mtu: MtuReport,
    /// Current media QoS setting and readback.
    pub qos: QosReport,
}

/// Closed send-to failure set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SendToError {
    /// Application payload exceeded V2's 1,200-byte maximum.
    ApplicationMtuExceeded {
        /// Rejected payload length.
        actual: usize,
        /// Fixed V2 maximum.
        maximum: usize,
    },
    /// The required per-datagram DS field could not be applied before send.
    QosBeforeSend(QosError),
    /// The control datagram was sent, then restoring the media DS field failed.
    QosRestoreAfterSend(QosError),
    /// Nonblocking socket could not accept the datagram.
    WouldBlock,
    /// Timed socket exceeded its send timeout.
    TimedOut,
    /// DF/PMTU rejected the datagram.
    PathMtuUnsupported {
        /// Closed OS failure from `sendto`.
        failure: OsFailure,
    },
    /// Datagram sockets are required to accept the whole packet atomically.
    PartialDatagram {
        /// Submitted payload length.
        expected: usize,
        /// Length reported by the OS.
        actual: usize,
    },
    /// Remaining `sendto` failures.
    OperatingSystem {
        /// Closed OS failure.
        failure: OsFailure,
    },
}

impl SendToError {
    /// Returns the stable V2 error for path-MTU failures.
    #[must_use]
    pub const fn stable_code(self) -> Option<ErrorCode> {
        match self {
            Self::PathMtuUnsupported { .. } => Some(ErrorCode::MediaPathMtuUnsupported),
            _ => None,
        }
    }
}

impl fmt::Display for SendToError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ApplicationMtuExceeded { actual, maximum } => {
                write!(formatter, "UDP payload {actual} exceeds {maximum} bytes")
            }
            Self::QosBeforeSend(error) => write!(formatter, "UDP send was not submitted: {error}"),
            Self::QosRestoreAfterSend(error) => {
                write!(
                    formatter,
                    "UDP send completed; QoS restoration failed: {error}"
                )
            }
            Self::WouldBlock => formatter.write_str("UDP send would block"),
            Self::TimedOut => formatter.write_str("UDP send timed out"),
            Self::PathMtuUnsupported { failure } => {
                write!(formatter, "DF UDP send exceeded the path MTU: {failure}")
            }
            Self::PartialDatagram { expected, actual } => {
                write!(formatter, "UDP send wrote {actual} of {expected} bytes")
            }
            Self::OperatingSystem { failure } => {
                write!(formatter, "UDP send failed: {failure}")
            }
        }
    }
}

impl std::error::Error for SendToError {}

/// Closed receive-from failure set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiveFromError {
    /// Every preallocated pool slot is currently leased.
    PoolExhausted,
    /// Nonblocking socket has no datagram ready.
    WouldBlock,
    /// Timed socket exceeded its receive timeout.
    TimedOut,
    /// The received datagram was truncated above the fixed V2 maximum.
    DatagramTooLarge {
        /// Fixed receive limit.
        maximum: usize,
    },
    /// The OS supplied a non-publishable source endpoint.
    InvalidSourceEndpoint,
    /// Remaining `recvfrom` failures.
    OperatingSystem {
        /// Closed OS failure.
        failure: OsFailure,
    },
}

impl ReceiveFromError {
    /// Returns the stable V2 code for resource, format, and endpoint failures.
    #[must_use]
    pub const fn stable_code(self) -> Option<ErrorCode> {
        match self {
            Self::PoolExhausted => Some(ErrorCode::MediaReceiveCapacity),
            Self::DatagramTooLarge { .. } => Some(ErrorCode::MediaPacketMalformed),
            Self::InvalidSourceEndpoint => Some(ErrorCode::MediaIdentityMismatch),
            Self::WouldBlock | Self::TimedOut | Self::OperatingSystem { .. } => None,
        }
    }
}

impl fmt::Display for ReceiveFromError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PoolExhausted => formatter.write_str("datagram pool exhausted"),
            Self::WouldBlock => formatter.write_str("UDP receive would block"),
            Self::TimedOut => formatter.write_str("UDP receive timed out"),
            Self::DatagramTooLarge { maximum } => {
                write!(formatter, "received UDP payload exceeds {maximum} bytes")
            }
            Self::InvalidSourceEndpoint => {
                formatter.write_str("OS returned an invalid IPv4 source endpoint")
            }
            Self::OperatingSystem { failure } => {
                write!(formatter, "UDP receive failed: {failure}")
            }
        }
    }
}

impl std::error::Error for ReceiveFromError {}

/// One received packet, its OS-observed source, and its pool lease.
pub struct ReceivedDatagram {
    lease: DatagramLease,
    source: Endpoint,
}

impl ReceivedDatagram {
    /// Returns the source address and port reported by `recvfrom`.
    #[must_use]
    pub const fn source(&self) -> Endpoint {
        self.source
    }

    /// Returns the exact received application bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.lease.as_bytes()
    }

    /// Returns the received byte count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.lease.len()
    }

    /// Returns whether this is a zero-length UDP datagram.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lease.is_empty()
    }

    /// Keeps the storage lease and discards the receive metadata.
    #[must_use]
    pub fn into_lease(self) -> DatagramLease {
        self.lease
    }
}

impl AsRef<[u8]> for ReceivedDatagram {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl Deref for ReceivedDatagram {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.as_bytes()
    }
}

impl fmt::Debug for ReceivedDatagram {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReceivedDatagram")
            .field("len", &self.len())
            .field("source", &self.source)
            .finish()
    }
}

struct SendState {
    media_qos: QosPolicy,
    applied_ds_field: Option<u8>,
}

/// One exclusively bound IPv4 LAN UDP socket.
///
/// The type does not implement `Clone` and never exposes the raw socket. Send
/// operations serialize DS-field changes so control packets use DSCP 0 and the
/// configured media mark is restored before the send lock is released.
pub struct LanUdpSocket {
    socket: Socket,
    local_address: SocketAddrV4,
    io_mode: IoMode,
    requested_receive_buffer_bytes: Option<usize>,
    requested_send_buffer_bytes: Option<usize>,
    send_state: Mutex<SendState>,
}

impl LanUdpSocket {
    /// Creates, configures, and binds one IPv4 UDP socket.
    pub fn bind(config: LanUdpSocketConfig) -> Result<Self, BindError> {
        validate_config(config)?;

        let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
            .map_err(|error| BindError::os(BindStage::CreateSocket, error))?;

        socket
            .set_reuse_address(false)
            .map_err(|error| BindError::os(BindStage::DisableAddressReuse, error))?;
        platform::set_exclusive_address_use(&socket)
            .map_err(|error| BindError::os(BindStage::EnableExclusiveAddressUse, error))?;
        platform::set_dont_fragment(&socket)
            .map_err(|error| BindError::os(BindStage::EnableDontFragment, error))?;

        if let Some(bytes) = config.receive_buffer_bytes {
            socket
                .set_recv_buffer_size(bytes.get())
                .map_err(|error| BindError::os(BindStage::SetReceiveBuffer, error))?;
        }
        if let Some(bytes) = config.send_buffer_bytes {
            socket
                .set_send_buffer_size(bytes.get())
                .map_err(|error| BindError::os(BindStage::SetSendBuffer, error))?;
        }

        configure_io_mode(&socket, config.io_mode)?;

        let initial_ds_field = qos_ds_field(config.initial_media_qos);
        platform::set_ds_field(&socket, initial_ds_field)
            .map_err(|error| BindError::os(BindStage::SetInitialQos, error))?;

        socket
            .bind(&SocketAddr::V4(config.local_address).into())
            .map_err(|error| BindError::os(BindStage::Bind, error))?;
        let local_address = socket
            .local_addr()
            .map_err(|error| BindError::os(BindStage::QueryLocalAddress, error))?
            .as_socket_ipv4()
            .ok_or(BindError::UnexpectedLocalAddressFamily)?;

        let bound = Self {
            socket,
            local_address,
            io_mode: config.io_mode,
            requested_receive_buffer_bytes: config.receive_buffer_bytes.map(NonZeroUsize::get),
            requested_send_buffer_bytes: config.send_buffer_bytes.map(NonZeroUsize::get),
            send_state: Mutex::new(SendState {
                media_qos: config.initial_media_qos,
                applied_ds_field: Some(initial_ds_field),
            }),
        };

        let report = bound.report().map_err(BindError::Report)?;
        if report.reuse_address_enabled || report.exclusive_address_use == Some(false) {
            return Err(BindError::SingleOwnerNotEffective {
                reuse_address_enabled: report.reuse_address_enabled,
                exclusive_address_use: report.exclusive_address_use,
            });
        }
        if !report.mtu.dont_fragment_effective {
            return Err(BindError::DontFragmentNotEffective);
        }

        Ok(bound)
    }

    /// Returns the effective local IPv4 bind address.
    #[must_use]
    pub const fn local_address(&self) -> SocketAddrV4 {
        self.local_address
    }

    /// Applies the authority's media policy and reports the socket readback.
    pub fn set_media_qos_policy(&self, policy: QosPolicy) -> Result<QosReport, QosError> {
        let requested = qos_ds_field(policy);
        let mut state = lock_unpoisoned(&self.send_state);

        platform::set_ds_field(&self.socket, requested)
            .map_err(|error| QosError::os(QosErrorStage::SetPolicy, error))?;
        state.media_qos = policy;

        let actual = match query_ds_field(&self.socket) {
            Ok(actual) => actual,
            Err(error) => {
                state.applied_ds_field = None;
                return Err(error);
            }
        };
        state.applied_ds_field = Some(actual);

        Ok(QosReport {
            requested_policy: policy,
            requested_ds_field: requested,
            actual_ds_field: actual,
        })
    }

    /// Sends one complete datagram to a validated IPv4 endpoint.
    ///
    /// Media uses the configured room QoS. Control is sent with DSCP 0, after
    /// which the configured media DS field is restored under the same send lock.
    pub fn send_to(
        &self,
        payload: &[u8],
        target: Endpoint,
        class: DatagramClass,
    ) -> Result<(), SendToError> {
        if payload.len() > MAX_UDP_APPLICATION_BYTES {
            return Err(SendToError::ApplicationMtuExceeded {
                actual: payload.len(),
                maximum: MAX_UDP_APPLICATION_BYTES,
            });
        }

        let target_address = SocketAddr::V4(SocketAddrV4::new(target.ip(), target.port())).into();
        let mut state = lock_unpoisoned(&self.send_state);
        let media_ds_field = qos_ds_field(state.media_qos);
        let packet_ds_field = match class {
            DatagramClass::Media => media_ds_field,
            DatagramClass::Control => 0,
        };

        apply_cached_ds_field(
            &self.socket,
            &mut state,
            packet_ds_field,
            QosErrorStage::ApplyDatagramClass,
        )
        .map_err(SendToError::QosBeforeSend)?;

        let send_result = self.socket.send_to(payload, &target_address);
        classify_send_result(send_result, payload.len(), self.io_mode)?;

        if packet_ds_field != media_ds_field {
            apply_cached_ds_field(
                &self.socket,
                &mut state,
                media_ds_field,
                QosErrorStage::RestorePolicy,
            )
            .map_err(SendToError::QosRestoreAfterSend)?;
        }

        Ok(())
    }

    /// Receives one datagram into a preallocated pool slot.
    ///
    /// The returned source endpoint comes only from the OS `recvfrom` result.
    pub fn recv_from(&self, pool: &DatagramPool) -> Result<ReceivedDatagram, ReceiveFromError> {
        let mut lease = match pool.try_lease() {
            Some(lease) => lease,
            None => return Err(discard_one_datagram(&self.socket, self.io_mode)),
        };

        let receive_result = {
            let bytes = lease.buffer_mut();
            let uninitialized = initialized_as_maybe_uninit(bytes);
            let mut buffers = [MaybeUninitSlice::new(uninitialized)];
            self.socket.recv_from_vectored(&mut buffers)
        };

        let (length, flags, source) = match receive_result {
            Ok(received) => received,
            Err(error) => return Err(classify_receive_error(&error, self.io_mode)),
        };
        if flags.is_truncated() {
            return Err(ReceiveFromError::DatagramTooLarge {
                maximum: MAX_UDP_APPLICATION_BYTES,
            });
        }

        let source = source
            .as_socket_ipv4()
            .and_then(|address| Endpoint::new(*address.ip(), address.port()).ok())
            .ok_or(ReceiveFromError::InvalidSourceEndpoint)?;

        lease
            .set_len(length)
            .map_err(|_| ReceiveFromError::DatagramTooLarge {
                maximum: MAX_UDP_APPLICATION_BYTES,
            })?;

        Ok(ReceivedDatagram { lease, source })
    }

    /// Queries effective ownership, wait, buffer, DF, and QoS settings.
    pub fn report(&self) -> Result<SocketReport, ReportError> {
        let reuse_address_enabled = self
            .socket
            .reuse_address()
            .map_err(|error| ReportError::os(ReportStage::AddressReuse, error))?;
        let exclusive_address_use = platform::exclusive_address_use(&self.socket)
            .map_err(|error| ReportError::os(ReportStage::ExclusiveAddressUse, error))?;
        let dont_fragment_effective = platform::dont_fragment(&self.socket)
            .map_err(|error| ReportError::os(ReportStage::DontFragment, error))?;
        let receive_buffer = self
            .socket
            .recv_buffer_size()
            .map_err(|error| ReportError::os(ReportStage::ReceiveBuffer, error))?;
        let send_buffer = self
            .socket
            .send_buffer_size()
            .map_err(|error| ReportError::os(ReportStage::SendBuffer, error))?;
        let receive_timeout = self
            .socket
            .read_timeout()
            .map_err(|error| ReportError::os(ReportStage::ReceiveTimeout, error))?;
        let send_timeout = self
            .socket
            .write_timeout()
            .map_err(|error| ReportError::os(ReportStage::SendTimeout, error))?;

        let qos = {
            let mut state = lock_unpoisoned(&self.send_state);
            let actual = platform::ds_field(&self.socket)
                .map_err(|error| ReportError::os(ReportStage::Qos, error))?;
            let actual =
                u8::try_from(actual).map_err(|_| ReportError::QosValueOutOfRange { actual })?;
            state.applied_ds_field = Some(actual);
            QosReport {
                requested_policy: state.media_qos,
                requested_ds_field: qos_ds_field(state.media_qos),
                actual_ds_field: actual,
            }
        };

        Ok(SocketReport {
            local_address: self.local_address,
            io_mode: self.io_mode,
            actual_receive_timeout: receive_timeout,
            actual_send_timeout: send_timeout,
            receive_buffer: SocketBufferReport {
                requested_bytes: self.requested_receive_buffer_bytes,
                actual_bytes: receive_buffer,
            },
            send_buffer: SocketBufferReport {
                requested_bytes: self.requested_send_buffer_bytes,
                actual_bytes: send_buffer,
            },
            reuse_address_enabled,
            exclusive_address_use,
            mtu: MtuReport {
                max_udp_application_bytes: MAX_UDP_APPLICATION_BYTES,
                worst_case_ipv4_packet_bytes: MAX_IPV4_PACKET_BYTES,
                dont_fragment_requested: true,
                dont_fragment_effective,
            },
            qos,
        })
    }
}

impl fmt::Debug for LanUdpSocket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LanUdpSocket")
            .field("local_address", &self.local_address)
            .field("io_mode", &self.io_mode)
            .finish_non_exhaustive()
    }
}

fn validate_config(config: LanUdpSocketConfig) -> Result<(), BindError> {
    if let IoMode::Timed {
        receive_timeout,
        send_timeout,
    } = config.io_mode
    {
        if receive_timeout.is_zero() {
            return Err(BindError::ZeroTimeout {
                direction: TimeoutDirection::Receive,
            });
        }
        if send_timeout.is_zero() {
            return Err(BindError::ZeroTimeout {
                direction: TimeoutDirection::Send,
            });
        }
    }

    const MAX_SOCKET_BUFFER_BYTES: usize = i32::MAX as usize;
    for (direction, requested) in [
        (SocketBufferDirection::Receive, config.receive_buffer_bytes),
        (SocketBufferDirection::Send, config.send_buffer_bytes),
    ] {
        if requested.is_some_and(|bytes| bytes.get() > MAX_SOCKET_BUFFER_BYTES) {
            return Err(BindError::SocketBufferTooLarge {
                direction,
                requested: requested.map_or(0, NonZeroUsize::get),
                maximum: MAX_SOCKET_BUFFER_BYTES,
            });
        }
    }
    Ok(())
}

fn configure_io_mode(socket: &Socket, io_mode: IoMode) -> Result<(), BindError> {
    socket
        .set_nonblocking(io_mode.is_nonblocking())
        .map_err(|error| BindError::os(BindStage::SetNonblocking, error))?;

    let (receive_timeout, send_timeout) = match io_mode {
        IoMode::Nonblocking => (None, None),
        IoMode::Timed {
            receive_timeout,
            send_timeout,
        } => (Some(receive_timeout), Some(send_timeout)),
    };
    socket
        .set_read_timeout(receive_timeout)
        .map_err(|error| BindError::os(BindStage::SetReceiveTimeout, error))?;
    socket
        .set_write_timeout(send_timeout)
        .map_err(|error| BindError::os(BindStage::SetSendTimeout, error))
}

const fn qos_ds_field(policy: QosPolicy) -> u8 {
    match policy {
        QosPolicy::AudioPriority => DS_FIELD_EF,
        QosPolicy::Off => 0,
    }
}

fn query_ds_field(socket: &Socket) -> Result<u8, QosError> {
    let actual =
        platform::ds_field(socket).map_err(|error| QosError::os(QosErrorStage::Query, error))?;
    u8::try_from(actual).map_err(|_| QosError::ValueOutOfRange { actual })
}

fn apply_cached_ds_field(
    socket: &Socket,
    state: &mut SendState,
    requested: u8,
    stage: QosErrorStage,
) -> Result<(), QosError> {
    if state.applied_ds_field == Some(requested) {
        return Ok(());
    }
    if let Err(error) = platform::set_ds_field(socket, requested) {
        state.applied_ds_field = None;
        return Err(QosError::os(stage, error));
    }
    state.applied_ds_field = Some(requested);
    Ok(())
}

fn classify_send_result(
    result: io::Result<usize>,
    expected: usize,
    io_mode: IoMode,
) -> Result<(), SendToError> {
    match result {
        Ok(actual) if actual == expected => Ok(()),
        Ok(actual) => Err(SendToError::PartialDatagram { expected, actual }),
        Err(error) if platform::is_message_too_large(&error) => {
            Err(SendToError::PathMtuUnsupported {
                failure: OsFailure::capture(&error),
            })
        }
        Err(error)
            if error.kind() == io::ErrorKind::TimedOut
                || (error.kind() == io::ErrorKind::WouldBlock
                    && matches!(io_mode, IoMode::Timed { .. })) =>
        {
            Err(SendToError::TimedOut)
        }
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Err(SendToError::WouldBlock),
        Err(error) => Err(SendToError::OperatingSystem {
            failure: OsFailure::capture(&error),
        }),
    }
}

fn classify_receive_error(error: &io::Error, io_mode: IoMode) -> ReceiveFromError {
    if error.kind() == io::ErrorKind::TimedOut
        || (error.kind() == io::ErrorKind::WouldBlock && matches!(io_mode, IoMode::Timed { .. }))
    {
        ReceiveFromError::TimedOut
    } else if error.kind() == io::ErrorKind::WouldBlock {
        ReceiveFromError::WouldBlock
    } else {
        ReceiveFromError::OperatingSystem {
            failure: OsFailure::capture(error),
        }
    }
}

fn discard_one_datagram(socket: &Socket, io_mode: IoMode) -> ReceiveFromError {
    let mut discarded = [MaybeUninit::uninit(); MAX_UDP_APPLICATION_BYTES];
    let mut buffers = [MaybeUninitSlice::new(&mut discarded)];
    match socket.recv_from_vectored(&mut buffers) {
        Ok(_) => ReceiveFromError::PoolExhausted,
        Err(error) => classify_receive_error(&error, io_mode),
    }
}

fn initialized_as_maybe_uninit(bytes: &mut [u8]) -> &mut [MaybeUninit<u8>] {
    // SAFETY: `MaybeUninit<u8>` has the same layout as `u8`. This conversion
    // only relaxes initialization requirements while Winsock/recvmsg writes the
    // buffer, and callers expose no bytes beyond the returned receive length.
    unsafe {
        std::slice::from_raw_parts_mut(bytes.as_mut_ptr().cast::<MaybeUninit<u8>>(), bytes.len())
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;

    use super::*;

    const TEST_TIMEOUT: Duration = Duration::from_millis(500);

    fn timed_config(local_address: SocketAddrV4) -> LanUdpSocketConfig {
        LanUdpSocketConfig::new(
            local_address,
            IoMode::Timed {
                receive_timeout: TEST_TIMEOUT,
                send_timeout: TEST_TIMEOUT,
            },
        )
    }

    fn loopback_endpoint(socket: &LanUdpSocket) -> Endpoint {
        let address = socket.local_address();
        Endpoint::new(*address.ip(), address.port()).unwrap()
    }

    #[test]
    fn duplicate_bind_is_rejected_and_single_owner_options_are_reported() {
        let first = LanUdpSocket::bind(LanUdpSocketConfig::new(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            IoMode::Nonblocking,
        ))
        .unwrap();
        let report = first.report().unwrap();

        assert!(!report.reuse_address_enabled);
        #[cfg(windows)]
        assert_eq!(report.exclusive_address_use, Some(true));
        assert!(report.mtu.dont_fragment_requested);
        assert!(report.mtu.dont_fragment_effective);
        assert_eq!(
            report.mtu.worst_case_ipv4_packet_bytes,
            MAX_IPV4_PACKET_BYTES
        );
        assert_eq!(report.io_mode, IoMode::Nonblocking);
        assert_eq!(report.actual_receive_timeout, None);
        assert_eq!(report.actual_send_timeout, None);

        let duplicate = LanUdpSocket::bind(LanUdpSocketConfig::new(
            first.local_address(),
            IoMode::Nonblocking,
        ));
        assert!(matches!(
            duplicate,
            Err(BindError::OperatingSystem {
                stage: BindStage::Bind,
                failure: OsFailure {
                    kind: OsFailureKind::AddressInUse,
                    ..
                },
            })
        ));
    }

    #[test]
    fn mtu_limit_rejects_oversized_application_datagram() {
        let socket = LanUdpSocket::bind(LanUdpSocketConfig::new(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            IoMode::Nonblocking,
        ))
        .unwrap();
        let target = loopback_endpoint(&socket);
        let payload = [0_u8; MAX_UDP_APPLICATION_BYTES + 1];

        let error = socket
            .send_to(&payload, target, DatagramClass::Media)
            .unwrap_err();
        assert_eq!(
            error,
            SendToError::ApplicationMtuExceeded {
                actual: MAX_UDP_APPLICATION_BYTES + 1,
                maximum: MAX_UDP_APPLICATION_BYTES,
            }
        );
        assert_eq!(error.stable_code(), None);
    }

    #[test]
    fn loopback_receive_preserves_payload_and_real_source_endpoint() {
        let sender =
            LanUdpSocket::bind(timed_config(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))).unwrap();
        let receiver = LanUdpSocket::bind(
            timed_config(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
                .with_receive_buffer_bytes(NonZeroUsize::new(64 * 1_024).unwrap())
                .with_send_buffer_bytes(NonZeroUsize::new(64 * 1_024).unwrap()),
        )
        .unwrap();
        let pool = DatagramPool::new(NonZeroUsize::new(2).unwrap());
        let payload = b"RLB2 loopback";

        sender
            .send_to(payload, loopback_endpoint(&receiver), DatagramClass::Media)
            .unwrap();
        let received = receiver.recv_from(&pool).unwrap();

        assert_eq!(received.as_bytes(), payload);
        assert_eq!(received.source(), loopback_endpoint(&sender));
        assert_eq!(pool.available(), 1);

        let report = receiver.report().unwrap();
        assert_eq!(report.receive_buffer.requested_bytes, Some(64 * 1_024));
        assert_eq!(report.send_buffer.requested_bytes, Some(64 * 1_024));
        assert!(report.receive_buffer.actual_bytes > 0);
        assert!(report.send_buffer.actual_bytes > 0);
        assert_eq!(
            report.io_mode,
            IoMode::Timed {
                receive_timeout: TEST_TIMEOUT,
                send_timeout: TEST_TIMEOUT,
            }
        );
        assert!(report.actual_receive_timeout.is_some());
        assert!(report.actual_send_timeout.is_some());

        drop(received);
        assert_eq!(pool.available(), pool.capacity());
    }

    #[test]
    fn nonblocking_receive_drops_a_packet_when_the_pool_is_exhausted() {
        let socket = LanUdpSocket::bind(LanUdpSocketConfig::new(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            IoMode::Nonblocking,
        ))
        .unwrap();
        let sender = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let pool = DatagramPool::new(NonZeroUsize::new(1).unwrap());
        let held = pool.try_lease().unwrap();

        assert_eq!(
            socket.recv_from(&pool).unwrap_err(),
            ReceiveFromError::WouldBlock
        );

        sender.send_to(b"drop me", socket.local_address()).unwrap();
        assert_eq!(
            socket.recv_from(&pool).unwrap_err(),
            ReceiveFromError::PoolExhausted
        );

        drop(held);

        assert_eq!(
            socket.recv_from(&pool).unwrap_err(),
            ReceiveFromError::WouldBlock
        );
        assert_eq!(pool.available(), pool.capacity());
    }

    #[test]
    fn timed_receive_returns_timeout_and_recycles_its_slot() {
        let socket =
            LanUdpSocket::bind(timed_config(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))).unwrap();
        let pool = DatagramPool::new(NonZeroUsize::new(1).unwrap());

        assert_eq!(
            socket.recv_from(&pool).unwrap_err(),
            ReceiveFromError::TimedOut
        );
        assert_eq!(pool.available(), pool.capacity());
    }

    #[test]
    fn oversized_receive_is_rejected_and_recycles_its_slot() {
        let receiver =
            LanUdpSocket::bind(timed_config(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))).unwrap();
        let sender = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let pool = DatagramPool::new(NonZeroUsize::new(1).unwrap());
        let payload = [0_u8; MAX_UDP_APPLICATION_BYTES + 1];

        sender.send_to(&payload, receiver.local_address()).unwrap();
        assert_eq!(
            receiver.recv_from(&pool).unwrap_err(),
            ReceiveFromError::DatagramTooLarge {
                maximum: MAX_UDP_APPLICATION_BYTES,
            }
        );
        assert_eq!(pool.available(), pool.capacity());
    }

    #[test]
    fn qos_off_is_queryable_and_ef_is_reported_when_supported() {
        let socket = LanUdpSocket::bind(LanUdpSocketConfig::new(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            IoMode::Nonblocking,
        ))
        .unwrap();
        let receiver = LanUdpSocket::bind(LanUdpSocketConfig::new(
            SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0),
            IoMode::Nonblocking,
        ))
        .unwrap();

        let off = socket.report().unwrap().qos;
        assert_eq!(off.requested_policy, QosPolicy::Off);
        assert_eq!(off.requested_ds_field, 0);
        assert_eq!(off.actual_ds_field, 0);
        assert!(off.is_effective());

        match socket.set_media_qos_policy(QosPolicy::AudioPriority) {
            Ok(priority) => {
                assert_eq!(priority.requested_ds_field, DS_FIELD_EF);
                assert_eq!(priority.actual_ds_field, DS_FIELD_EF);
                assert!(priority.is_effective());
                socket
                    .send_to(
                        b"RLBD control",
                        loopback_endpoint(&receiver),
                        DatagramClass::Control,
                    )
                    .unwrap();
                assert_eq!(socket.report().unwrap().qos, priority);
            }
            #[cfg(windows)]
            Err(QosError::OperatingSystem { failure, .. }) => {
                assert!(matches!(
                    failure.kind,
                    OsFailureKind::InvalidInput
                        | OsFailureKind::PermissionDenied
                        | OsFailureKind::Unsupported
                ));
            }
            Err(error) => panic!("unexpected QoS configuration error: {error}"),
        }

        let off = socket.set_media_qos_policy(QosPolicy::Off).unwrap();
        assert_eq!(off.actual_ds_field, 0);
        assert!(off.is_effective());
    }

    #[test]
    #[cfg(any(windows, target_os = "linux", target_os = "android"))]
    fn operating_system_message_too_large_maps_to_path_mtu_error() {
        #[cfg(windows)]
        let raw_os_error = 10_040;
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let raw_os_error = 90;

        let error = classify_send_result(
            Err(io::Error::from_raw_os_error(raw_os_error)),
            MAX_UDP_APPLICATION_BYTES,
            IoMode::Nonblocking,
        )
        .unwrap_err();

        assert!(matches!(error, SendToError::PathMtuUnsupported { .. }));
        assert_eq!(
            error.stable_code(),
            Some(ErrorCode::MediaPathMtuUnsupported)
        );
    }
}
