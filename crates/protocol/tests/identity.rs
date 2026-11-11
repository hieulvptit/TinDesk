use binachicken_protocol::*;
use ed25519_dalek::SigningKey;

fn challenge() -> DeviceChallenge {
    DeviceChallenge {
        purpose: ChallengePurpose::AuthenticateDevice,
        nonce: [7; 32],
        device_id: DeviceId::from_uuid(uuid::Uuid::from_u128(42)),
        expires_at_unix_ms: 2000,
    }
}

#[test]
fn signed_device_challenge_is_single_use() {
    let key = SigningKey::from_bytes(&[3; 32]);
    let challenge = challenge();
    let signature = sign_challenge(&key, &challenge).unwrap();
    let mut verifier = ChallengeVerifier::new(challenge);
    assert!(
        verifier
            .verify(&key.verifying_key(), &signature, 1999)
            .is_ok()
    );
    assert_eq!(
        verifier.verify(&key.verifying_key(), &signature, 1999),
        Err(ProtocolError::ChallengeReplayed)
    );
}

#[test]
fn all_challenge_fields_are_signed() {
    let key = SigningKey::from_bytes(&[3; 32]);
    let original = challenge();
    let signature = sign_challenge(&key, &original).unwrap();
    let mut variants = [
        original.clone(),
        original.clone(),
        original.clone(),
        original.clone(),
    ];
    variants[0].purpose = ChallengePurpose::RegisterDevice;
    variants[1].nonce[0] ^= 1;
    variants[2].device_id = DeviceId::from_uuid(uuid::Uuid::from_u128(43));
    variants[3].expires_at_unix_ms = 2001;
    for changed in variants {
        assert_eq!(
            verify_challenge(&key.verifying_key(), &changed, &signature, 0),
            Err(ProtocolError::InvalidSignature)
        );
    }
}

#[test]
fn expired_wrong_key_and_malformed_proofs_are_rejected() {
    let key = SigningKey::from_bytes(&[3; 32]);
    let challenge = challenge();
    let signature = sign_challenge(&key, &challenge).unwrap();
    assert_eq!(
        verify_challenge(&key.verifying_key(), &challenge, &signature, 2000),
        Err(ProtocolError::ChallengeExpired)
    );
    let other = SigningKey::from_bytes(&[4; 32]);
    let mut verifier = ChallengeVerifier::new(challenge);
    assert_eq!(
        verifier.verify(&other.verifying_key(), &signature, 0),
        Err(ProtocolError::InvalidSignature)
    );
    assert_eq!(
        verifier.verify(&key.verifying_key(), &[1; 63], 0),
        Err(ProtocolError::InvalidSignature)
    );
    assert!(verifier.verify(&key.verifying_key(), &signature, 0).is_ok());
}
