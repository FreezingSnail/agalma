//! Client core (decision D1/D2).
//!
//! Connects to a component server, sends serial requests (one in flight per
//! connection), and returns typed results. Connection losses are classified:
//! before the request is sent the call is [`IpcError::Unavailable`]; once the
//! request is on the wire an unobserved result is an `UnknownOutcome`, since an
//! effect may already have occurred. Replaying the same request id recovers the
//! recorded receipt from the server.

use std::io::{BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::Value;

use crate::envelope::{Request, Response, PROTOCOL_VERSION};
use crate::error::IpcError;
use crate::frame::{self, FrameError};

/// A client connection to a component server.
pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl Client {
    /// Connect to the server listening on `socket`.
    pub fn connect(socket: &Path) -> Result<Client, IpcError> {
        let stream = UnixStream::connect(socket).map_err(|e| {
            IpcError::unavailable(format!("connect to {}: {e}", socket.display()))
        })?;
        let reader = BufReader::new(
            stream
                .try_clone()
                .map_err(|e| IpcError::unavailable(format!("clone connection: {e}")))?,
        );
        Ok(Client {
            stream,
            reader,
            next_id: 1,
        })
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
        serde_json::from_value(result).map_err(|e| IpcError::protocol(format!("decode result: {e}")))
    }

    /// Call `method` with an explicit request `id` (for replay after a
    /// connection loss). Reusing an id is safe: the server returns its recorded
    /// receipt rather than re-running the effect.
    pub fn call_with_id(&mut self, id: u64, method: &str, params: Value) -> Result<Value, IpcError> {
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
                        IpcError::unavailable(format!("connection closed before request id {id} was sent"))
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
