//! Request/response envelopes (decision D1).
//!
//! Request: `{"v":1,"id":<u64>,"method":"<snake_case>","params":{...}}`.
//! Response: `{"v":1,"id":<u64>,"result":{...}}` or
//! `{"v":1,"id":<u64>,"error":{"code":"...","message":"..."}}`.

use agalma_contracts::ContractError;
use serde::{Deserialize, Serialize};

use crate::error::contract_error_wire;

/// Protocol version carried in every envelope.
pub const PROTOCOL_VERSION: u32 = 1;

/// A client request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Protocol version.
    pub v: u32,
    /// Client-assigned request id, echoed in the response and used for replay.
    pub id: u64,
    /// Snake-case method name.
    pub method: String,
    /// Method parameters; defaults to JSON null when omitted.
    #[serde(default)]
    pub params: serde_json::Value,
}

impl Request {
    /// Build a request at the current [`PROTOCOL_VERSION`].
    pub fn new(id: u64, method: impl Into<String>, params: serde_json::Value) -> Self {
        Request {
            v: PROTOCOL_VERSION,
            id,
            method: method.into(),
            params,
        }
    }
}

/// A wire error payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseError {
    /// Error code: a `ContractError` variant name, `Unavailable`, or `Protocol`.
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

/// A server response. On success `result` is present (possibly JSON null); on
/// failure `error` is present. A response with neither is a protocol violation.
///
/// `result` is a double option: the outer `None` means the field is absent (an
/// error response), `Some(None)` means the field is present as JSON `null`, and
/// `Some(Some(v))` means it carries `v`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    /// Protocol version.
    pub v: u32,
    /// Echo of the request id.
    pub id: u64,
    /// Success payload when the call succeeded.
    #[serde(
        default,
        deserialize_with = "deserialize_some",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<Option<serde_json::Value>>,
    /// Error payload when the call failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ResponseError>,
}

/// Force a present field (even JSON `null`) to deserialize as `Some`; a missing
/// field still falls back to the field's `default` (`None`).
fn deserialize_some<'de, D, T>(de: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(de).map(Some)
}

impl Response {
    /// Build a success response.
    pub fn ok(id: u64, result: serde_json::Value) -> Self {
        Response {
            v: PROTOCOL_VERSION,
            id,
            result: Some(Some(result)),
            error: None,
        }
    }

    /// Build an error response from a raw code/message.
    pub fn error(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Response {
            v: PROTOCOL_VERSION,
            id,
            result: None,
            error: Some(ResponseError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }

    /// Build an error response from a [`ContractError`].
    pub fn from_contract_error(id: u64, error: &ContractError) -> Self {
        let (code, message) = contract_error_wire(error);
        Response::error(id, code, message)
    }

    /// The success payload, if this response carried a `result` field.
    ///
    /// A JSON `null` result surfaces as `Some(Value::Null)`; an error response
    /// (no result field) surfaces as `None`.
    pub fn result_value(&self) -> Option<serde_json::Value> {
        self.result
            .as_ref()
            .map(|inner| inner.clone().unwrap_or(serde_json::Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_serializes_with_expected_shape() {
        let req = Request::new(7, "echo", json!({"x": 1}));
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(
            value,
            json!({"v": 1, "id": 7, "method": "echo", "params": {"x": 1}})
        );
    }

    #[test]
    fn request_params_default_to_null() {
        let decoded: Request =
            serde_json::from_value(json!({"v": 1, "id": 2, "method": "m"})).unwrap();
        assert_eq!(decoded.params, serde_json::Value::Null);
    }

    #[test]
    fn ok_response_carries_result_without_error() {
        let resp = Response::ok(3, json!({"count": 5}));
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value, json!({"v": 1, "id": 3, "result": {"count": 5}}));
    }

    #[test]
    fn error_response_carries_error_without_result() {
        let resp = Response::error(4, "KnownFailure", "boom");
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(
            value,
            json!({"v": 1, "id": 4, "error": {"code": "KnownFailure", "message": "boom"}})
        );
    }

    #[test]
    fn null_result_is_distinct_from_missing() {
        let resp = Response::ok(9, serde_json::Value::Null);
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value, json!({"v": 1, "id": 9, "result": null}));
        let back: Response = serde_json::from_value(value).unwrap();
        assert_eq!(back.result, Some(None));
        assert_eq!(back.result_value(), Some(serde_json::Value::Null));

        // An error response has no `result` field at all.
        let err_resp = Response::error(9, "Protocol", "x");
        assert_eq!(err_resp.result, None);
        assert_eq!(err_resp.result_value(), None);
    }

    #[test]
    fn contract_error_maps_to_error_response() {
        let resp = Response::from_contract_error(5, &ContractError::Conflict("c".into()));
        assert_eq!(resp.error.unwrap().code, "Conflict");
    }
}
