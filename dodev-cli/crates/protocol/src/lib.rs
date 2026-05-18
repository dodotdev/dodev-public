use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Maximum payload size: 10 MB.
pub const MAX_PAYLOAD_SIZE: usize = 10 * 1024 * 1024;

/// Header size: 1 byte msg_type + 16 bytes UUID.
pub const HEADER_SIZE: usize = 17;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("invalid message type: 0x{0:02x}")]
    InvalidMessageType(u8),

    #[error("insufficient data: need at least {HEADER_SIZE} bytes, got {0}")]
    InsufficientData(usize),

    #[error("payload too large: {0} bytes exceeds {MAX_PAYLOAD_SIZE} byte limit")]
    PayloadTooLarge(usize),

    #[error("serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),
}

// ---------------------------------------------------------------------------
// MessageType
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Auth = 0x01,
    AuthOk = 0x02,
    AuthFail = 0x03,
    HttpRequest = 0x10,
    HttpResponse = 0x11,
    Ping = 0x20,
    Pong = 0x21,
    Error = 0x30,
    Close = 0x40,
}

impl TryFrom<u8> for MessageType {
    type Error = ProtocolError;

    fn try_from(value: u8) -> Result<Self, <Self as TryFrom<u8>>::Error> {
        match value {
            0x01 => Ok(Self::Auth),
            0x02 => Ok(Self::AuthOk),
            0x03 => Ok(Self::AuthFail),
            0x10 => Ok(Self::HttpRequest),
            0x11 => Ok(Self::HttpResponse),
            0x20 => Ok(Self::Ping),
            0x21 => Ok(Self::Pong),
            0x30 => Ok(Self::Error),
            0x40 => Ok(Self::Close),
            other => Err(ProtocolError::InvalidMessageType(other)),
        }
    }
}

// ---------------------------------------------------------------------------
// TunnelMessage
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct TunnelMessage {
    pub msg_type: MessageType,
    pub request_id: Uuid,
    pub payload: Bytes,
}

impl TunnelMessage {
    /// Create a new tunnel message.
    pub fn new(msg_type: MessageType, request_id: Uuid, payload: Bytes) -> Self {
        Self {
            msg_type,
            request_id,
            payload,
        }
    }

    /// Encode the message into a binary frame.
    ///
    /// Wire format: `[msg_type: 1][request_id: 16][payload: rest]`
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(HEADER_SIZE + self.payload.len());
        buf.put_u8(self.msg_type as u8);
        buf.put_slice(self.request_id.as_bytes());
        buf.put_slice(&self.payload);
        buf.freeze()
    }

    /// Decode a binary frame into a tunnel message.
    ///
    /// Enforces `MAX_PAYLOAD_SIZE`.
    pub fn decode(data: Bytes) -> Result<Self, ProtocolError> {
        if data.len() < HEADER_SIZE {
            return Err(ProtocolError::InsufficientData(data.len()));
        }

        let mut cursor = data;

        let msg_type_byte = cursor[0];
        cursor.advance(1);

        let msg_type = MessageType::try_from(msg_type_byte)?;

        let uuid_bytes: [u8; 16] = cursor[..16]
            .try_into()
            .expect("slice is exactly 16 bytes");
        cursor.advance(16);

        let payload = cursor;

        if payload.len() > MAX_PAYLOAD_SIZE {
            return Err(ProtocolError::PayloadTooLarge(payload.len()));
        }

        Ok(Self {
            msg_type,
            request_id: Uuid::from_bytes(uuid_bytes),
            payload,
        })
    }

    // -----------------------------------------------------------------------
    // Convenience constructors
    // -----------------------------------------------------------------------

    /// Build an Auth message with a JSON payload.
    pub fn auth(api_key: &str, subdomain: Option<&str>) -> Result<Self, ProtocolError> {
        let auth = AuthPayload {
            key: api_key.to_string(),
            subdomain: subdomain.map(|s| s.to_string()),
        };
        let json = serde_json::to_vec(&auth)?;
        Ok(Self::new(
            MessageType::Auth,
            Uuid::new_v4(),
            Bytes::from(json),
        ))
    }

    /// Build an AuthOk message carrying the assigned subdomain.
    pub fn auth_ok(subdomain: &str) -> Self {
        Self::new(
            MessageType::AuthOk,
            Uuid::new_v4(),
            Bytes::from(subdomain.to_string()),
        )
    }

    /// Build an AuthFail message carrying the rejection reason.
    pub fn auth_fail(reason: &str) -> Self {
        Self::new(
            MessageType::AuthFail,
            Uuid::new_v4(),
            Bytes::from(reason.to_string()),
        )
    }

    /// Build a Ping message.
    pub fn ping() -> Self {
        Self::new(MessageType::Ping, Uuid::new_v4(), Bytes::new())
    }

    /// Build a Pong message echoing the original request_id.
    pub fn pong(request_id: Uuid) -> Self {
        Self::new(MessageType::Pong, request_id, Bytes::new())
    }

    /// Build a Close message.
    pub fn close() -> Self {
        Self::new(MessageType::Close, Uuid::new_v4(), Bytes::new())
    }

    /// Build an Error message.
    pub fn error(reason: &str) -> Self {
        Self::new(
            MessageType::Error,
            Uuid::new_v4(),
            Bytes::from(reason.to_string()),
        )
    }

    /// Build an HttpRequest message from an `HttpRequestPayload`.
    pub fn http_request(
        request_id: Uuid,
        payload: &HttpRequestPayload,
    ) -> Result<Self, ProtocolError> {
        let json = serde_json::to_vec(payload)?;
        Ok(Self::new(
            MessageType::HttpRequest,
            request_id,
            Bytes::from(json),
        ))
    }

    /// Build an HttpResponse message from an `HttpResponsePayload`.
    pub fn http_response(
        request_id: Uuid,
        payload: &HttpResponsePayload,
    ) -> Result<Self, ProtocolError> {
        let json = serde_json::to_vec(payload)?;
        Ok(Self::new(
            MessageType::HttpResponse,
            request_id,
            Bytes::from(json),
        ))
    }
}

// ---------------------------------------------------------------------------
// Payload types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthPayload {
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subdomain: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequestPayload {
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, String)>,
    #[serde(with = "base64_bytes")]
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponsePayload {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    #[serde(with = "base64_bytes")]
    pub body: Vec<u8>,
}

// ---------------------------------------------------------------------------
// Base64 serde helper for binary body fields
// ---------------------------------------------------------------------------

mod base64_bytes {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(bytes.iter())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let v: Vec<u8> = Vec::deserialize(deserializer)?;
        Ok(v)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_auth() {
        let msg = TunnelMessage::auth("test-key-123", Some("myapp")).unwrap();
        let encoded = msg.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();

        assert_eq!(decoded.msg_type, MessageType::Auth);
        assert_eq!(decoded.request_id, msg.request_id);

        let payload: AuthPayload = serde_json::from_slice(&decoded.payload).unwrap();
        assert_eq!(payload.key, "test-key-123");
        assert_eq!(payload.subdomain.as_deref(), Some("myapp"));
    }

    #[test]
    fn roundtrip_auth_no_subdomain() {
        let msg = TunnelMessage::auth("key-only", None).unwrap();
        let encoded = msg.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();

        assert_eq!(decoded.msg_type, MessageType::Auth);
        let payload: AuthPayload = serde_json::from_slice(&decoded.payload).unwrap();
        assert_eq!(payload.key, "key-only");
        assert!(payload.subdomain.is_none());
    }

    #[test]
    fn roundtrip_auth_ok() {
        let msg = TunnelMessage::auth_ok("myapp.local.dev");
        let encoded = msg.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();

        assert_eq!(decoded.msg_type, MessageType::AuthOk);
        assert_eq!(
            std::str::from_utf8(&decoded.payload).unwrap(),
            "myapp.local.dev"
        );
    }

    #[test]
    fn roundtrip_auth_fail() {
        let msg = TunnelMessage::auth_fail("invalid api key");
        let encoded = msg.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();

        assert_eq!(decoded.msg_type, MessageType::AuthFail);
        assert_eq!(
            std::str::from_utf8(&decoded.payload).unwrap(),
            "invalid api key"
        );
    }

    #[test]
    fn roundtrip_ping_pong() {
        let ping = TunnelMessage::ping();
        let ping_id = ping.request_id;
        let encoded = ping.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();
        assert_eq!(decoded.msg_type, MessageType::Ping);
        assert!(decoded.payload.is_empty());

        let pong = TunnelMessage::pong(ping_id);
        let encoded = pong.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();
        assert_eq!(decoded.msg_type, MessageType::Pong);
        assert_eq!(decoded.request_id, ping_id);
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn roundtrip_close() {
        let msg = TunnelMessage::close();
        let encoded = msg.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();

        assert_eq!(decoded.msg_type, MessageType::Close);
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn roundtrip_error() {
        let msg = TunnelMessage::error("something went wrong");
        let encoded = msg.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();

        assert_eq!(decoded.msg_type, MessageType::Error);
        assert_eq!(
            std::str::from_utf8(&decoded.payload).unwrap(),
            "something went wrong"
        );
    }

    #[test]
    fn roundtrip_http_request() {
        let req = HttpRequestPayload {
            method: "POST".to_string(),
            uri: "/api/data".to_string(),
            headers: vec![
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Authorization".to_string(), "Bearer tok".to_string()),
            ],
            body: b"{\"hello\":\"world\"}".to_vec(),
        };
        let id = Uuid::new_v4();
        let msg = TunnelMessage::http_request(id, &req).unwrap();
        let encoded = msg.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();

        assert_eq!(decoded.msg_type, MessageType::HttpRequest);
        assert_eq!(decoded.request_id, id);

        let parsed: HttpRequestPayload = serde_json::from_slice(&decoded.payload).unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.uri, "/api/data");
        assert_eq!(parsed.headers.len(), 2);
        assert_eq!(parsed.body, b"{\"hello\":\"world\"}");
    }

    #[test]
    fn roundtrip_http_response() {
        let resp = HttpResponsePayload {
            status: 200,
            headers: vec![("Content-Type".to_string(), "text/plain".to_string())],
            body: b"OK".to_vec(),
        };
        let id = Uuid::new_v4();
        let msg = TunnelMessage::http_response(id, &resp).unwrap();
        let encoded = msg.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();

        assert_eq!(decoded.msg_type, MessageType::HttpResponse);
        assert_eq!(decoded.request_id, id);

        let parsed: HttpResponsePayload = serde_json::from_slice(&decoded.payload).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.body, b"OK");
    }

    #[test]
    fn decode_insufficient_data() {
        let data = Bytes::from_static(&[0x01; 5]); // Only 5 bytes, need 17
        let result = TunnelMessage::decode(data);
        assert!(matches!(result, Err(ProtocolError::InsufficientData(5))));
    }

    #[test]
    fn decode_invalid_message_type() {
        let mut buf = BytesMut::with_capacity(HEADER_SIZE);
        buf.put_u8(0xFF); // invalid type
        buf.put_slice(Uuid::new_v4().as_bytes());
        let result = TunnelMessage::decode(buf.freeze());
        assert!(matches!(
            result,
            Err(ProtocolError::InvalidMessageType(0xFF))
        ));
    }

    #[test]
    fn decode_header_only_no_payload() {
        // A valid message with exactly 17 bytes (no payload) should succeed.
        let msg = TunnelMessage::new(MessageType::Ping, Uuid::new_v4(), Bytes::new());
        let encoded = msg.encode();
        assert_eq!(encoded.len(), HEADER_SIZE);
        let decoded = TunnelMessage::decode(encoded).unwrap();
        assert_eq!(decoded.msg_type, MessageType::Ping);
        assert!(decoded.payload.is_empty());
    }

    #[test]
    fn message_type_roundtrip_all_variants() {
        let variants: Vec<(MessageType, u8)> = vec![
            (MessageType::Auth, 0x01),
            (MessageType::AuthOk, 0x02),
            (MessageType::AuthFail, 0x03),
            (MessageType::HttpRequest, 0x10),
            (MessageType::HttpResponse, 0x11),
            (MessageType::Ping, 0x20),
            (MessageType::Pong, 0x21),
            (MessageType::Error, 0x30),
            (MessageType::Close, 0x40),
        ];

        for (expected_type, byte_val) in variants {
            let converted = MessageType::try_from(byte_val).unwrap();
            assert_eq!(converted, expected_type);
            assert_eq!(converted as u8, byte_val);
        }
    }

    #[test]
    fn encode_preserves_uuid() {
        let id = Uuid::new_v4();
        let msg = TunnelMessage::new(MessageType::Auth, id, Bytes::from("test"));
        let encoded = msg.encode();
        let decoded = TunnelMessage::decode(encoded).unwrap();
        assert_eq!(decoded.request_id, id);
    }
}
