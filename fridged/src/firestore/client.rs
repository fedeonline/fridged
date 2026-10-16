//! The HTTP side: OAuth2 token exchange and batched document writes.
//!
//! Blocking, called only from the uploader thread. Layout in the database:
//!
//! - `devices/{device_id}/samples/{bootId-uptimeMs}`: one document per sample
//! - `devices/{device_id}/status/current`: the latest sample, overwritten
//! - `devices/{device_id}/settings/current`: the band set by the user, read
//!   back every round (see `crate::settings`)
//!
//! Both are written by the same `:commit`, so the status never runs ahead of
//! the samples.

use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use ureq::Agent;

use super::auth::{AuthError, ServiceAccount, Signer};
use super::value;
use crate::control::Band;
use crate::sample::Sample;

const FIRESTORE: &str = "https://firestore.googleapis.com/v1";
/// Get a new token this long before the old one expires, so a request never
/// goes out with a token about to die in flight.
const REFRESH_MARGIN: Duration = Duration::from_secs(5 * 60);
/// Firestore accepts up to 500 writes per commit; one goes to the status.
pub const MAX_BATCH: usize = 200;
const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("network error")]
    Network(#[source] ureq::Error),
    #[error("HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("unexpected response: {0}")]
    Response(String),
    #[error(transparent)]
    Auth(#[from] AuthError),
}

impl UploadError {
    /// The access token was rejected: a new one is needed before retrying.
    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Status { status: 401, .. })
    }
}

struct Token {
    value: String,
    expires: Instant,
}

pub struct Client {
    agent: Agent,
    signer: Signer,
    /// `projects/{p}/databases/{d}/documents`
    root: String,
    device_id: String,
    token: Option<Token>,
}

impl Client {
    pub fn new(
        account: &ServiceAccount,
        project: &str,
        database: &str,
        device_id: &str,
    ) -> Result<Self, AuthError> {
        let config = Agent::config_builder()
            // Statuses are classified by the caller, with the body for the log.
            .http_status_as_error(false)
            .timeout_global(Some(TIMEOUT))
            .build();
        Ok(Self {
            agent: Agent::new_with_config(config),
            signer: Signer::new(account)?,
            root: format!("projects/{project}/databases/{database}/documents"),
            device_id: device_id.to_owned(),
            token: None,
        })
    }

    /// Writes `samples` and updates the status document, in one commit.
    /// `now_unix` signs the token request: the wall clock must be synchronized.
    pub fn commit(&mut self, samples: &[Sample], now_unix: i64) -> Result<(), UploadError> {
        if samples.is_empty() {
            return Ok(());
        }
        let body = commit_body(&self.root, &self.device_id, samples);
        let url = format!("{FIRESTORE}/{}:commit", self.root);
        self.authorized_post(&url, &body, now_unix).map(drop)
    }

    /// Reads a document by its path under the database root, e.g.
    /// `devices/fridge/status/current`.
    pub fn get(&mut self, path: &str, now_unix: i64) -> Result<Value, UploadError> {
        let token = self.token(now_unix)?;
        let url = format!("{FIRESTORE}/{}/{path}", self.root);
        let response = self
            .agent
            .get(&url)
            .header("Authorization", &format!("Bearer {token}"))
            .call()
            .map_err(UploadError::Network)?;
        json_response(response)
    }

    /// The settings document, or `None` if the user has not created one.
    pub fn read_settings(&mut self, now_unix: i64) -> Result<Option<Value>, UploadError> {
        match self.get(&self.settings_path(), now_unix) {
            Ok(doc) => Ok(Some(doc)),
            Err(UploadError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Writes the band to the settings document (what the console does by
    /// hand), for tools and tests.
    pub fn write_settings(&mut self, band: Band, now_unix: i64) -> Result<(), UploadError> {
        let write = json!({ "update": {
            "name": format!("{}/{}", self.root, self.settings_path()),
            "fields": {
                "targetMin": { "doubleValue": band.min },
                "targetMax": { "doubleValue": band.max },
            },
        }});
        self.commit_writes(vec![write], now_unix)
    }

    /// Deletes the settings document: the device goes back to its config file.
    pub fn clear_settings(&mut self, now_unix: i64) -> Result<(), UploadError> {
        let write = json!({ "delete": format!("{}/{}", self.root, self.settings_path()) });
        self.commit_writes(vec![write], now_unix)
    }

    fn settings_path(&self) -> String {
        format!("devices/{}/settings/current", self.device_id)
    }

    fn commit_writes(&mut self, writes: Vec<Value>, now_unix: i64) -> Result<(), UploadError> {
        let url = format!("{FIRESTORE}/{}:commit", self.root);
        let body = json!({ "writes": writes });
        self.authorized_post(&url, &body, now_unix).map(drop)
    }

    /// Forgets the access token, e.g. after a 401.
    pub fn invalidate_token(&mut self) {
        self.token = None;
    }

    fn authorized_post(
        &mut self,
        url: &str,
        body: &Value,
        now_unix: i64,
    ) -> Result<Value, UploadError> {
        let token = self.token(now_unix)?;
        let response = self
            .agent
            .post(url)
            .header("Authorization", &format!("Bearer {token}"))
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .map_err(UploadError::Network)?;
        json_response(response)
    }

    fn token(&mut self, now_unix: i64) -> Result<String, UploadError> {
        if let Some(t) = &self.token {
            if Instant::now() + REFRESH_MARGIN < t.expires {
                return Ok(t.value.clone());
            }
        }
        let assertion = self.signer.jwt(now_unix)?;
        let response = self
            .agent
            .post(self.signer.token_uri())
            .send_form([
                ("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"),
                ("assertion", assertion.as_str()),
            ])
            .map_err(UploadError::Network)?;
        let token: TokenResponse = serde_json::from_value(json_response(response)?)
            .map_err(|e| UploadError::Response(e.to_string()))?;
        log::debug!("new access token, valid {} s", token.expires_in);
        self.token = Some(Token {
            value: token.access_token.clone(),
            expires: Instant::now() + Duration::from_secs(token.expires_in),
        });
        Ok(token.access_token)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
}

fn json_response(mut response: ureq::http::Response<ureq::Body>) -> Result<Value, UploadError> {
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .read_to_string()
        .map_err(UploadError::Network)?;
    if !(200..300).contains(&status) {
        return Err(UploadError::Status {
            status,
            body: truncate(&body, 500),
        });
    }
    serde_json::from_str(&body).map_err(|e| UploadError::Response(e.to_string()))
}

fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

/// The `:commit` request: one upsert per sample, plus the status document
/// with the latest one.
pub fn commit_body(root: &str, device_id: &str, samples: &[Sample]) -> Value {
    let device = format!("{root}/devices/{device_id}");
    let mut writes: Vec<Value> = samples
        .iter()
        .map(|s| {
            json!({ "update": {
                "name": format!("{device}/samples/{}", value::document_id(s)),
                "fields": value::fields(s),
            }})
        })
        .collect();
    if let Some(latest) = samples.iter().max_by_key(|s| (s.time_ms, s.uptime_ms)) {
        writes.push(json!({ "update": {
            "name": format!("{device}/status/current"),
            "fields": value::fields(latest),
        }}));
    }
    json!({ "writes": writes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spool::tests::sample;

    const ROOT: &str = "projects/my-project/databases/(default)/documents";

    #[test]
    fn commit_body_has_one_write_per_sample_plus_status() {
        let mut a = sample(1_000);
        a.uptime_ms = 1;
        let mut b = sample(2_000);
        b.uptime_ms = 2;
        let body = commit_body(ROOT, "fridge", &[a, b]);
        let writes = body["writes"].as_array().unwrap();
        assert_eq!(writes.len(), 3);
        assert_eq!(
            writes[0]["update"]["name"],
            format!("{ROOT}/devices/fridge/samples/boot-000000000001")
        );
        assert_eq!(
            writes[2]["update"]["name"],
            format!("{ROOT}/devices/fridge/status/current")
        );
        // The status is the latest sample.
        assert_eq!(
            writes[2]["update"]["fields"]["timeMs"]["integerValue"],
            "2000"
        );
    }

    #[test]
    fn only_401_asks_for_a_new_token() {
        let status = |status| UploadError::Status {
            status,
            body: String::new(),
        };
        assert!(status(401).is_unauthorized());
        assert!(!status(403).is_unauthorized());
        assert!(!status(503).is_unauthorized());
        assert!(!UploadError::Response("x".into()).is_unauthorized());
    }

    #[test]
    fn long_error_bodies_are_truncated() {
        let long = "é".repeat(600);
        let t = truncate(&long, 500);
        assert_eq!(t.chars().count(), 501);
        assert!(t.ends_with('…'));
        assert_eq!(truncate("short", 500), "short");
    }
}
