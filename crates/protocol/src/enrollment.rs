//! HTTP identity DTOs. Secret-bearing types deliberately do not implement Debug.
use crate::{DeviceChallenge, DeviceId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct EnrollmentRequest {
    pub name: String,
    pub public_key: String,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Enrollment {
    pub id: String,
    pub display_code: String,
    pub browser_url: String,
    pub polling_verifier: String,
    pub expires_at_unix_ms: u64,
    pub challenge: DeviceChallenge,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct EnrollmentPoll {
    pub polling_verifier: String,
    pub signature: Vec<u8>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum EnrollmentStatus {
    Pending,
    Enrolled { device_id: DeviceId },
}
#[derive(Clone, Serialize, Deserialize)]
pub struct DeviceProof {
    pub challenge_id: String,
    pub signature: Vec<u8>,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct AuthenticationChallenge {
    pub id: String,
    pub challenge: DeviceChallenge,
}
#[derive(Clone, Serialize, Deserialize)]
pub struct AccessToken {
    pub token: String,
    pub expires_at_unix_ms: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceSummary {
    pub id: DeviceId,
    pub name: String,
    pub public_key: String,
    pub revoked: bool,
}
