//! Client core (decision D1/D2).
//!
//! Connects to a component server, sends serial requests (one in flight per
//! connection), and returns typed results. Connection losses are classified:
//! before the request is sent the call is [`IpcError::Unavailable`]; once the
//! request is on the wire an unobserved result is an `UnknownOutcome`, since an
//! effect may already have occurred. Replaying the same request id recovers the
//! recorded receipt from the server.
//!
//! # Id invariant
//!
//! Auto-allocated ids (used by [`Client::call`] and [`Client::call_typed`]) are
//! **connection-unique**: each connection seeds `next_id` from a per-process
//! unique base ([`seed_next_id`]) and increments monotonically, so a fresh
//! connection never reuses another connection's ids. This matters because the
//! server's receipt map is keyed by request id only and is intentionally global
//! — explicit [`Client::call_with_id`] replay must survive reconnects. Without
//! the unique seed, a new connection starting at id 1 would replay the previous
//! connection's recorded response for id 1 (a cross-method, wrong-typed replay).

use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::envelope::{Request, Response, PROTOCOL_VERSION};
use crate::error::IpcError;
use crate::frame::{self, FrameError};

/// Process-global counter giving each connection a distinct auto-id band.
static CONNECTION_SEQ: AtomicU64 = AtomicU64::new(0);

/// Width of the per-connection id band: a connection's auto ids stay inside the
/// aligned `2^BAND_BITS` block that its seed begins.
const BAND_BITS: u32 = 20;

/// Seed for a connection's auto-allocated request ids.
///
/// Auto ids must be unique per connection (see the module docs). The seed packs
/// a *connection slot* into the high bits and shifts it left by [`BAND_BITS`],
/// so each slot owns a disjoint aligned block of ids. The slot combines three
/// parts: the process id (distinguishes processes sharing one server), wall-clock
/// nanoseconds (extra entropy across process restarts), and a process-global
/// connection counter (distinguishes connections within a process). Because the
/// slot is unique per connection, so is its id block.
pub(crate) fn seed_next_id() -> u64 {
    let pid = u64::from(std::process::id()) & 0x003F_FFFF; // 22 bits
    let seq = CONNECTION_SEQ.fetch_add(1, Ordering::Relaxed) & 0x0003_FFFF; // 18 bits
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let entropy = (nanos >> 8) & 0xF; // 4 bits
                                      // slot = pid[43..22] | entropy[21..18] | seq[17..0]; the disjoint bit fields
                                      // keep `seq` canonical, so distinct connections get distinct slots.
    ((pid << 22) | (entropy << 18) | seq) << BAND_BITS
}

/// A client connection to a component server.
pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    /// Connect to the server listening on `socket`.
    pub fn connect(socket: &Path) -> Result<Client, IpcError> {
        let stream = UnixStream::connect(socket)
            .map_err(|e| IpcError::unavailable(format!("connect to {}: {e}", socket.display())))?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| IpcError::unavailable(format!("clone connection: {e}")))?,
        );
        Ok(Client {
            stream,
            reader,
            next_id: seed_next_id(),
        })
    }

    /// The next auto-allocated request id.
    ///
    /// Auto ids are connection-unique (see the module docs); this is exposed for
    /// tests that assert the id invariant across connections.
    pub fn next_id(&self) -> u64 {
        self.next_id
    }

    /// Call `method` with `params`, assigning a fresh request id.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, IpcError> {
        let id = self.next_id;
        self.next_id += 1;
        self.call_with_id(id, method, params)
    }

    /// Call `method`, serialize `params`, and convert the result to `R`.
    pub fn call_typed<P: Serialize, R: DeserializeOwned>(
        &mut self,
        method: &str,
        params: &P,
    ) -> Result<R, IpcError> {
        let value = serde_json::to_value(params)
            .map_err(|e| IpcError::protocol(format!("encode params: {e}")))?;
        let id = self.next_id;
        self.next_id += 1;
        let result = self.call_with_id(id, method, value)?;
        serde_json::from_value(result)
            .map_err(|e| IpcError::protocol(format!("decode result: {e}")))
    }

    /// Call `method` with an explicit request `id` (for replay after a
    /// connection loss). Reusing an id is safe: the server returns its recorded
    /// receipt rather than re-running the effect.
    pub fn call_with_id(
        &mut self,
        id: u64,
        method: &str,
        params: Value,
    ) -> Result<Value, IpcError> {
        let request = Request::new(id, method, params);
        let payload = frame::encode_json(&request)
            .map_err(|e| IpcError::protocol(format!("encode request id {id}: {e}")))?;
        let framed = frame::frame_payload(&payload)
            .map_err(|e| IpcError::protocol(format!("frame request id {id}: {e}")))?;

        self.send(&framed, id)?;
        let payload = self.recv(id)?;

        let response: Response = serde_json::from_slice(&payload).map_err(|e| {
            IpcError::protocol(format!("malformed response envelope for id {id}: {e}"))
        })?;

        if response.id != id {
            return Err(IpcError::protocol(format!(
                "response id {} does not match request id {id}",
                response.id
            )));
        }
        if response.v != PROTOCOL_VERSION {
            return Err(IpcError::protocol(format!(
                "response protocol version {} unsupported (expected {})",
                response.v, PROTOCOL_VERSION
            )));
        }
        if let Some(error) = response.error {
            return Err(IpcError::from_wire(&error.code, &error.message));
        }
        match response.result {
            Some(inner) => Ok(inner.unwrap_or(serde_json::Value::Null)),
            None => Err(IpcError::protocol(format!(
                "response id {id} carried neither result nor error"
            ))),
        }
    }

    /// Write a full frame, classifying failures by whether any bytes went out.
    fn send(&mut self, buf: &[u8], id: u64) -> Result<(), IpcError> {
        let mut written = 0usize;
        while written < buf.len() {
            match self.stream.write(&buf[written..]) {
                Ok(0) => {
                    return Err(if written == 0 {
                        IpcError::unavailable(format!(
                            "connection closed before request id {id} was sent"
                        ))
                    } else {
                        IpcError::unknown_outcome(format!(
                            "connection closed after {written} bytes of request id {id}"
                        ))
                    });
                }
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(if written == 0 {
                        IpcError::unavailable(format!("send request id {id} failed: {e}"))
                    } else {
                        IpcError::unknown_outcome(format!(
                            "send request id {id} failed after {written} bytes: {e}"
                        ))
                    });
                }
            }
        }
        Ok(())
    }

    /// Read one response frame, classifying connection loss as `UnknownOutcome`.
    fn recv(&mut self, id: u64) -> Result<Vec<u8>, IpcError> {
        match frame::read_payload(&mut self.reader) {
            Ok(Some(payload)) => Ok(payload),
            Ok(None) => Err(IpcError::unknown_outcome(format!(
                "connection closed before response to request id {id}"
            ))),
            Err(FrameError::Io(e)) => Err(IpcError::unknown_outcome(format!(
                "read response id {id}: {e}"
            ))),
            Err(e) => Err(IpcError::protocol(format!(
                "invalid response frame for id {id}: {e}"
            ))),
        }
    }
}
