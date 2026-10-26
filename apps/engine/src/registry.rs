//! Member-side Registry C TCP client.
//!
//! Socket ownership lives here. Framing comes from `rlb-registry`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::thread;
use std::time::{Duration, Instant};

use rlb_model::Endpoint;
use rlb_registry::{
    ClientMessage, DecoderPhase, FrameDecoder, FrameSendQueue, RLBREG2_PREFACE, ServerMessage,
    decode_json_frame,
};

use crate::error::EngineError;

/// One TCP session toward a member-registry endpoint.
pub struct RegistryClient {
    stream: TcpStream,
    decoder: FrameDecoder,
    send_queue: FrameSendQueue,
    endpoint: Endpoint,
}

impl RegistryClient {
    /// Opens TCP 17099, writes the RLBREG2 preface, and waits for the server preface.
    pub fn connect(endpoint: Endpoint) -> Result<Self, EngineError> {
        let addr = SocketAddr::from((endpoint.ip(), endpoint.port()));
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_millis(200)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        stream.write_all(RLBREG2_PREFACE)?;
        stream.flush()?;
        let mut client = Self {
            stream,
            decoder: FrameDecoder::default(),
            send_queue: FrameSendQueue::default(),
            endpoint,
        };
        client.read_preface()?;
        Ok(client)
    }

    /// One provisioning request/response on a short-lived TCP session.
    pub fn provisioning_roundtrip(
        endpoint: Endpoint,
        message: &ClientMessage,
    ) -> Result<ServerMessage, EngineError> {
        let mut client = Self::connect(endpoint)?;
        client.send(message)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            let mut messages = client.poll()?;
            if let Some(reply) = messages.pop() {
                return Ok(reply);
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(EngineError::Persist(
            "registry provisioning response timed out",
        ))
    }

    /// Returns the configured registry endpoint.
    #[must_use]
    pub const fn endpoint(&self) -> Endpoint {
        self.endpoint
    }

    /// Queues one client JSON frame and flushes the socket.
    pub fn send(&mut self, message: &ClientMessage) -> Result<(), EngineError> {
        self.send_queue
            .push_json(message)
            .map_err(|_| EngineError::Persist("registry client send queue rejected the frame"))?;
        self.flush_send()
    }

    /// Reads available bytes and returns complete server messages.
    pub fn poll(&mut self) -> Result<Vec<ServerMessage>, EngineError> {
        let mut buf = [0_u8; 4096];
        match self.stream.read(&mut buf) {
            Ok(0) => Err(EngineError::Persist("registry TCP closed")),
            Ok(n) => {
                let frames = self
                    .decoder
                    .feed(&buf[..n])
                    .map_err(|_| EngineError::Persist("registry frame decode failed"))?;
                let mut messages = Vec::new();
                for payload in frames {
                    messages.push(
                        decode_json_frame::<ServerMessage>(&payload).map_err(|_| {
                            EngineError::Persist("registry JSON frame is malformed")
                        })?,
                    );
                }
                Ok(messages)
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                Ok(Vec::new())
            }
            Err(error) => Err(error.into()),
        }
    }

    fn read_preface(&mut self) -> Result<(), EngineError> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while self.decoder.phase() != DecoderPhase::Frames {
            if Instant::now() >= deadline {
                return Err(EngineError::Persist("registry preface timed out"));
            }
            let _ = self.poll()?;
            if self.decoder.phase() != DecoderPhase::Frames {
                thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(())
    }

    fn flush_send(&mut self) -> Result<(), EngineError> {
        while let Some(frame) = self.send_queue.pop() {
            self.stream.write_all(&frame)?;
        }
        self.stream.flush()?;
        Ok(())
    }
}
