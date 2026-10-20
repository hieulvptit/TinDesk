use crate::{DeviceApi, Error, Result, api::decode};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use remotevnc_protocol::*;
impl DeviceApi {
    pub async fn begin_enrollment(&self, name: &str, public_key: &str) -> Result<Enrollment> {
        if public_key != URL_SAFE_NO_PAD.encode(self.key.verifying_key().to_bytes()) {
            return Err(Error::Invalid);
        }
        let enrollment: Enrollment = decode(
            self.http
                .post(self.url("/v1/enrollments")?)
                .json(&EnrollmentRequest {
                    name: name.into(),
                    public_key: public_key.into(),
                })
                .send()
                .await
                .map_err(|_| Error::Request)?,
        )
        .await?;
        if enrollment.challenge.purpose != ChallengePurpose::RegisterDevice {
            return Err(Error::Invalid);
        }
        Ok(enrollment)
    }
    pub async fn poll_enrollment(&self, enrollment: &Enrollment) -> Result<EnrollmentStatus> {
        if enrollment.challenge.purpose != ChallengePurpose::RegisterDevice
            || enrollment.id.len() > 64
            || !enrollment
                .id
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == '-')
        {
            return Err(Error::Invalid);
        }
        let poll = EnrollmentPoll {
            polling_verifier: enrollment.polling_verifier.clone(),
            signature: sign_challenge(&self.key, &enrollment.challenge)
                .map_err(|_| Error::Invalid)?,
        };
        let result: EnrollmentStatus = decode(
            self.http
                .post(self.url(&format!("/v1/enrollments/{}/poll", enrollment.id))?)
                .json(&poll)
                .send()
                .await
                .map_err(|_| Error::Request)?,
        )
        .await?;
        if let EnrollmentStatus::Enrolled { device_id } = result
            && device_id != enrollment.challenge.device_id
        {
            return Err(Error::Invalid);
        }
        Ok(result)
    }
}
