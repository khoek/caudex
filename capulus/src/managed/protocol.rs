use std::fmt::{self, Display, Formatter};

use rand::RngExt;
use serde::{Deserialize, Serialize};

pub const PROTOCOL_MAJOR: u16 = 1;
pub(crate) const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct RequestId([u8; 16]);

impl RequestId {
    pub fn random() -> Self {
        Self(rand::rng().random())
    }
}

impl Display for RequestId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, formatter)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct JobId([u8; 16]);

impl JobId {
    pub fn random() -> Self {
        Self(rand::rng().random())
    }

    pub fn parse(value: &str) -> Result<Self, ManagementError> {
        if value.len() != 32 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(ManagementError::InvalidJobId);
        }
        if value.bytes().any(|byte| byte.is_ascii_uppercase()) {
            return Err(ManagementError::InvalidJobId);
        }
        let mut bytes = [0_u8; 16];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            bytes[index] = (hex_nibble(pair[0]).ok_or(ManagementError::InvalidJobId)? << 4)
                | hex_nibble(pair[1]).ok_or(ManagementError::InvalidJobId)?;
        }
        Ok(Self(bytes))
    }
}

impl Display for JobId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write_hex(&self.0, formatter)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PeerCredentials {
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "kind", content = "version")]
pub enum VersionTarget {
    Latest,
    Exact(String),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "method")]
pub enum ManagementRequest {
    Info,
    Resolve {
        target: VersionTarget,
    },
    Redeploy {
        target: VersionTarget,
        reinstall_requesting_user: bool,
    },
    JobStatus {
        job: JobId,
    },
    Repair,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case", tag = "result")]
pub enum ManagementResponse {
    Info(AgentInfo),
    Resolved { version: String },
    Redeploy(RedeployOutcome),
    Job(RedeployJob),
    Repair(RepairOutcome),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentInfo {
    pub product: String,
    pub package: String,
    pub version: String,
    pub protocol_major: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RedeployOutcome {
    pub job: JobId,
    pub unit: String,
    pub version: String,
    pub started: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum JobPhase {
    Queued,
    Preparing,
    Toolchain,
    Resolving,
    Building,
    Validating,
    Staging,
    CommittingSystem,
    RestartingAgent,
    ReinstallingUser,
    Complete,
    Failed,
}

impl JobPhase {
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Complete | Self::Failed)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RedeployJob {
    pub job: JobId,
    pub product: String,
    pub version: String,
    pub unit: String,
    pub phase: JobPhase,
    pub detail: String,
    pub system_committed: bool,
    pub rollback_succeeded: Option<bool>,
    pub required_user_reinstalled: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepairOutcome {
    pub changed: bool,
    pub detail: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ErrorCode {
    BadRequest,
    Unauthorized,
    UnsupportedProtocol,
    NotFound,
    Conflict,
    Unavailable,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ManagementError {
    #[error("management frame exceeds the {MAX_FRAME_BYTES}-byte limit")]
    FrameTooLarge,
    #[error("management peer closed the connection before sending a complete frame")]
    EarlyEof,
    #[error("failed to encode management protocol CBOR: {0}")]
    Encode(String),
    #[error("failed to decode management protocol CBOR: {0}")]
    Decode(String),
    #[error("management response request ID did not match the request")]
    MismatchedRequestId,
    #[error("management protocol v{0} is not supported")]
    UnsupportedProtocol(u16),
    #[error("invalid redeploy job ID")]
    InvalidJobId,
    #[error("management request failed ({code:?}): {message}")]
    Remote { code: ErrorCode, message: String },
    #[error("management I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct RequestEnvelope {
    pub request_id: RequestId,
    pub minimum_protocol_major: u16,
    pub maximum_protocol_major: u16,
    pub request: ManagementRequest,
}

#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct ResponseEnvelope {
    pub request_id: RequestId,
    pub protocol_major: u16,
    pub body: ResponseBody,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", tag = "status", content = "body")]
pub(crate) enum ResponseBody {
    Ok(ManagementResponse),
    Error(ProtocolError),
}

pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, ManagementError> {
    let mut bytes = Vec::new();
    ciborium::into_writer(value, &mut bytes)
        .map_err(|error| ManagementError::Encode(error.to_string()))?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ManagementError::FrameTooLarge);
    }
    Ok(bytes)
}

pub(crate) fn decode<T>(bytes: &[u8]) -> Result<T, ManagementError>
where
    T: for<'de> Deserialize<'de>,
{
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(ManagementError::FrameTooLarge);
    }
    ciborium::from_reader(bytes).map_err(|error| ManagementError::Decode(error.to_string()))
}

fn write_hex(bytes: &[u8], formatter: &mut Formatter<'_>) -> fmt::Result {
    for byte in bytes {
        write!(formatter, "{byte:02x}")?;
    }
    Ok(())
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips_through_cbor() {
        let envelope = RequestEnvelope {
            request_id: RequestId([0xDE; 16]),
            minimum_protocol_major: PROTOCOL_MAJOR,
            maximum_protocol_major: PROTOCOL_MAJOR,
            request: ManagementRequest::Redeploy {
                target: VersionTarget::Exact("1.2.3".to_string()),
                reinstall_requesting_user: true,
            },
        };
        let encoded = encode(&envelope).unwrap();
        assert_eq!(
            hex(&encoded),
            concat!(
                "a46a726571756573745f69649018de18de18de18de18de18de18de18de18de18de18de18de18de18de18de18",
                "de766d696e696d756d5f70726f746f636f6c5f6d616a6f7201766d6178696d756d5f70726f746f636f6c5f6d",
                "616a6f72016772657175657374a3666d6574686f646872656465706c6f7966746172676574a2646b696e6465",
                "65786163746776657273696f6e65312e322e3378197265696e7374616c6c5f72657175657374696e675f7573",
                "6572f5"
            )
        );
        let decoded: RequestEnvelope = decode(&encoded).unwrap();

        assert_eq!(decoded.request_id, envelope.request_id);
        assert_eq!(decoded.request, envelope.request);
    }

    #[test]
    fn protocol_v1_ignores_additive_envelope_and_method_fields() {
        let envelope = RequestEnvelope {
            request_id: RequestId([0xDE; 16]),
            minimum_protocol_major: PROTOCOL_MAJOR,
            maximum_protocol_major: PROTOCOL_MAJOR,
            request: ManagementRequest::Redeploy {
                target: VersionTarget::Latest,
                reinstall_requesting_user: false,
            },
        };
        let mut value: ciborium::Value =
            ciborium::from_reader(encode(&envelope).unwrap().as_slice()).unwrap();
        let ciborium::Value::Map(fields) = &mut value else {
            panic!("request envelope must encode as a map");
        };
        fields.push((
            ciborium::Value::Text("future-envelope-field".to_string()),
            ciborium::Value::Bool(true),
        ));
        let request = fields
            .iter_mut()
            .find_map(|(key, value)| {
                (key == &ciborium::Value::Text("request".to_string())).then_some(value)
            })
            .unwrap();
        let ciborium::Value::Map(request_fields) = request else {
            panic!("request method must encode as a map");
        };
        request_fields.push((
            ciborium::Value::Text("future-method-field".to_string()),
            ciborium::Value::Integer(0xDEADBEEF_u64.into()),
        ));
        let mut encoded = Vec::new();
        ciborium::into_writer(&value, &mut encoded).unwrap();

        let decoded: RequestEnvelope = decode(&encoded).unwrap();
        assert_eq!(decoded.request, envelope.request);
    }

    fn hex(bytes: &[u8]) -> String {
        use std::fmt::Write as _;

        bytes
            .iter()
            .fold(String::with_capacity(bytes.len() * 2), |mut value, byte| {
                write!(value, "{byte:02x}").unwrap();
                value
            })
    }

    #[test]
    fn job_ids_are_fixed_lowercase_hex() {
        let id = JobId::parse("deadbeefdeadbeefdeadbeefdeadbeef").unwrap();
        assert_eq!(id.to_string(), "deadbeefdeadbeefdeadbeefdeadbeef");
        assert!(JobId::parse("DEADBEEFDEADBEEFDEADBEEFDEADBEEF").is_err());
        assert!(JobId::parse("deadbeef").is_err());
    }

    #[test]
    fn oversized_frames_are_rejected_before_decode() {
        assert!(matches!(
            decode::<RequestEnvelope>(&vec![0; MAX_FRAME_BYTES + 1]),
            Err(ManagementError::FrameTooLarge)
        ));
    }

    #[test]
    fn unknown_method_decodes_to_a_structured_request() {
        let envelope = RequestEnvelope {
            request_id: RequestId([0; 16]),
            minimum_protocol_major: PROTOCOL_MAJOR,
            maximum_protocol_major: PROTOCOL_MAJOR,
            request: ManagementRequest::Redeploy {
                target: VersionTarget::Latest,
                reinstall_requesting_user: false,
            },
        };
        let mut encoded = encode(&envelope).unwrap();
        let offset = encoded
            .windows(b"redeploy".len())
            .position(|window| window == b"redeploy")
            .unwrap();
        encoded[offset..offset + b"whatever".len()].copy_from_slice(b"whatever");

        let decoded: RequestEnvelope = decode(&encoded).unwrap();
        assert_eq!(decoded.request, ManagementRequest::Unknown);
    }
}
