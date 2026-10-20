use crate::{DeviceId, ProtocolError};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChallengePurpose {
    RegisterDevice,
    AuthenticateDevice,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceChallenge {
    pub purpose: ChallengePurpose,
    /// Generate 32 unpredictable bytes on the server for every new challenge.
    pub nonce: [u8; 32],
    pub device_id: DeviceId,
    /// Unix time is used only for network challenge expiry, not local leases.
    pub expires_at_unix_ms: u64,
}

fn signing_bytes(challenge: &DeviceChallenge) -> Vec<u8> {
    let mut bytes = b"RemoteVNC/device-challenge/v1\0".to_vec();
    bytes.push(match challenge.purpose {
        ChallengePurpose::RegisterDevice => 1,
        ChallengePurpose::AuthenticateDevice => 2,
    });
    bytes.extend_from_slice(&challenge.nonce);
    bytes.extend_from_slice(challenge.device_id.0.as_bytes());
    bytes.extend_from_slice(&challenge.expires_at_unix_ms.to_be_bytes());
    bytes
}

pub fn sign_challenge(
    key: &SigningKey,
    challenge: &DeviceChallenge,
) -> Result<Vec<u8>, ProtocolError> {
    Ok(key.sign(&signing_bytes(challenge)).to_bytes().to_vec())
}

/// Cryptographic verification only. Authentication endpoints must use
/// `ChallengeVerifier` with their stored server-issued challenge for one-use checks.
pub fn verify_challenge(
    key: &VerifyingKey,
    challenge: &DeviceChallenge,
    signature: &[u8],
    now_unix_ms: u64,
) -> Result<(), ProtocolError> {
    if now_unix_ms >= challenge.expires_at_unix_ms {
        return Err(ProtocolError::ChallengeExpired);
    }
    let signature =
        Signature::from_slice(signature).map_err(|_| ProtocolError::InvalidSignature)?;
    key.verify_strict(&signing_bytes(challenge), &signature)
        .map_err(|_| ProtocolError::InvalidSignature)
}

/// Store exactly one instance per server-issued nonce. Do not rebuild this from
/// an untrusted submitted challenge; persist/atomically consume at the backend
/// boundary if multiple processes can authenticate the same challenge.
pub struct ChallengeVerifier {
    challenge: DeviceChallenge,
    consumed: bool,
}

impl ChallengeVerifier {
    pub fn new(challenge: DeviceChallenge) -> Self {
        Self {
            challenge,
            consumed: false,
        }
    }

    pub fn verify(
        &mut self,
        key: &VerifyingKey,
        signature: &[u8],
        now_unix_ms: u64,
    ) -> Result<(), ProtocolError> {
        if self.consumed {
            return Err(ProtocolError::ChallengeReplayed);
        }
        verify_challenge(key, &self.challenge, signature, now_unix_ms)?;
        self.consumed = true;
        Ok(())
    }
}
