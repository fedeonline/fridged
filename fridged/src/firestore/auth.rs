//! Service-account authentication: a JWT signed with the account's RSA key,
//! later exchanged for an OAuth2 access token (RFC 7523).

use std::fmt;
use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde::Deserialize;
use serde_json::json;

/// Read and write access to Firestore documents.
pub const SCOPE: &str = "https://www.googleapis.com/auth/datastore";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
/// The longest lifetime Google accepts for the assertion.
pub const JWT_LIFETIME_SECS: i64 = 3600;

/// The fields of a service-account JSON key that matter here.
#[derive(Clone, Deserialize)]
pub struct ServiceAccount {
    pub project_id: String,
    pub private_key_id: String,
    /// PEM, PKCS#8. Never logged: see the `Debug` impl.
    pub private_key: String,
    pub client_email: String,
    #[serde(default = "default_token_uri")]
    pub token_uri: String,
}

fn default_token_uri() -> String {
    DEFAULT_TOKEN_URI.to_owned()
}

impl fmt::Debug for ServiceAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServiceAccount")
            .field("project_id", &self.project_id)
            .field("client_email", &self.client_email)
            .field("private_key_id", &self.private_key_id)
            .field("private_key", &"<redacted>")
            .finish()
    }
}

impl ServiceAccount {
    pub fn load(path: &Path) -> Result<Self, AuthError> {
        let text = std::fs::read_to_string(path).map_err(AuthError::Read)?;
        serde_json::from_str(&text).map_err(AuthError::Parse)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("cannot read the service-account key")]
    Read(#[source] std::io::Error),
    #[error("invalid service-account JSON")]
    Parse(#[source] serde_json::Error),
    #[error("invalid private key: {0}")]
    Key(String),
    #[error("signing failed")]
    Sign,
}

/// Signs the JWT assertions for one service account.
pub struct Signer {
    client_email: String,
    key_id: String,
    token_uri: String,
    key: RsaKeyPair,
    rng: SystemRandom,
}

impl fmt::Debug for Signer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Signer")
            .field("client_email", &self.client_email)
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

impl Signer {
    pub fn new(account: &ServiceAccount) -> Result<Self, AuthError> {
        let der = pem_to_der(&account.private_key)?;
        let key = RsaKeyPair::from_pkcs8(&der).map_err(|e| AuthError::Key(e.to_string()))?;
        Ok(Self {
            client_email: account.client_email.clone(),
            key_id: account.private_key_id.clone(),
            token_uri: account.token_uri.clone(),
            key,
            rng: SystemRandom::new(),
        })
    }

    pub fn token_uri(&self) -> &str {
        &self.token_uri
    }

    /// The signed assertion, valid from `now` (Unix seconds) for an hour.
    pub fn jwt(&self, now: i64) -> Result<String, AuthError> {
        let header = json!({ "alg": "RS256", "typ": "JWT", "kid": self.key_id });
        let claims = json!({
            "iss": self.client_email,
            "scope": SCOPE,
            "aud": self.token_uri,
            "iat": now,
            "exp": now + JWT_LIFETIME_SECS,
        });
        let mut token = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut signature = vec![0; self.key.public().modulus_len()];
        self.key
            .sign(
                &RSA_PKCS1_SHA256,
                &self.rng,
                token.as_bytes(),
                &mut signature,
            )
            .map_err(|_| AuthError::Sign)?;
        token.push('.');
        token.push_str(&URL_SAFE_NO_PAD.encode(signature));
        Ok(token)
    }
}

/// Decodes a PEM `PRIVATE KEY` block to DER.
fn pem_to_der(pem: &str) -> Result<Vec<u8>, AuthError> {
    const BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
    const END: &str = "-----END PRIVATE KEY-----";
    let start = pem
        .find(BEGIN)
        .ok_or_else(|| AuthError::Key("no PKCS#8 PEM block".to_owned()))?;
    let body = &pem[start + BEGIN.len()..];
    let end = body
        .find(END)
        .ok_or_else(|| AuthError::Key("unterminated PEM block".to_owned()))?;
    let b64: String = body[..end].chars().filter(|c| !c.is_whitespace()).collect();
    STANDARD
        .decode(b64)
        .map_err(|e| AuthError::Key(format!("bad base64: {e}")))
}

#[cfg(test)]
mod tests {
    use ring::signature::{RSA_PKCS1_2048_8192_SHA256, UnparsedPublicKey};
    use serde_json::Value;

    use super::*;

    /// A throwaway 2048-bit key generated for these tests. Not a credential:
    /// it was never attached to any account.
    const TEST_KEY: &str = "\
-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQCzlNqxpf82DJsR
+WTI56AQSDHPJy7zooQTQRNfEWFXyw9D3fwax2nT9e1z4yjWnLMJw6Nr3+IGd5EH
C5SiaQ9rSLtW3DCdJlR1Irsy1aCjcez+1BysE2MC42Oq4XzFfiKgZ4jEEz+bpONj
imF7+siJH/iNM/Hxwp2aTuHCni+aU4eeeCbd+qMcO4/fIQnN5sKgPR5YXhuWzCE1
FeRaayAYQxWskAkWhZJVdomWrijxcYu9uy8e/kXxpWBqhrolf+9qcadgUH0+1Q3W
aCEijhuToLg9wZ7kHU6fSzDeM5zIVgvNJdL6qE3cjjUlBn9GE4idwjwnk6PIMXGs
R0kfodxHAgMBAAECggEALg+EhmM+0fSqw5H1pUgvuG1kBS+WLdxlxvB+kPhKwPNu
32ftFUtHnBusg3hkzaMUkvEQlgcvGJyLYMS6I2wSDeDyWCWS/IPLzt2RKUk88h8y
++uYzRQN5bxAIKDEWX1wUTctD6mmO3wv7KSeO2gIDo/aNxF3QZy6wszvv/TF3e2X
iRZtqrRUFPjzUBgRenZkTUQEfD8+W/Q44mYbqD4eK1T70baP6LiPWD7L+EEjjUSn
0S3hJFfQTJg6GN4f+6nsrv/p/rNP77ZtHoQAswWmhCXVejzPdmyI3x7TOwUXC838
v8jYPUDbkbVHSkr3SurkIU/xTS4WAH5DV9bgmrqYJQKBgQDmcnfTynu/nRKLFw3/
DasoicGm+zlh5L2KswDvm4E5GhSxQDhkZRsPOc/qrmVQtAO9aNNJITzaB38pH06l
nPOgFNKDoVE8ny7HhqRdOEkEpgWfidpr9Bx1wELyMq/lN8o8pSY2iBiD/tQwQZdM
Ee3aKAMJXI8uA2SkAGTnOdQhowKBgQDHfn/7VkrQj9f4e6mJ5M7zZB/8jo9E66Nv
jJz4PkIolFV/0WwYPByHUHbtI/O2/tMx6YMcxqITvMHlfq0VPqnlPz2Ml3rA/CXa
xu79rCyIDFdalZt3olcxnmFLOqLl/koOGmmP8U69Dp+K7Aj+X9hr7UY8i6/024qM
zrfogmutDQKBgQC6AwO+LN8emAbzxrRBgCL8IdXyly6GWkC0yDClN/xivy6ctJ3Q
S9w/ugaeBA87yVKLfSWvDek0hICcDO1KuRndyXDzVy9fo436+C+MH3axDeHUGU35
fZ3LXlmrbkaU2yefNB9YPRqs0PEt0oPxnzdCtLFeyRu5pHuD9d181yznyQKBgEJY
y++nD1K1xDobxpLZygogbpLsRrHBPKXIuhvFePNOAfF0kOKuez51gFf18QQXU93x
r21OknDGAiZ1LtM1+AMKOUocxvKP6XcNWkZ7kIPett6liwMKtr0F043Vfn/HRpHS
LvF0a59s0Zq+TW9sKOocC5PvUSG6MiYKShfF6FGxAoGBAJ6vOJF+ruHlmJXUTfOO
gYKFgZmDaojbF78Pqb+qlW5wEcKN8GLPwqtFUEV25Ldv2q+yBPgM82KKpGMbQ628
8x3az4kLZzeS1Ju685kRGBxYX2Uu+UI8WuLMmjgub4BoEzusDZKZ5LIohIzL53ua
dBUtId4iftJXLWHnzli5G+gb
-----END PRIVATE KEY-----
";

    fn account() -> ServiceAccount {
        serde_json::from_value(json!({
            "type": "service_account",
            "project_id": "beer-cellar",
            "private_key_id": "kid123",
            "private_key": TEST_KEY,
            "client_email": "fridged@beer-cellar.iam.gserviceaccount.com",
            "token_uri": "https://oauth2.googleapis.com/token",
        }))
        .unwrap()
    }

    fn decode(part: &str) -> Value {
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(part).unwrap()).unwrap()
    }

    #[test]
    fn jwt_header_and_claims() {
        let signer = Signer::new(&account()).unwrap();
        let jwt = signer.jwt(1_760_000_000).unwrap();
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        assert_eq!(
            decode(parts[0]),
            json!({ "alg": "RS256", "typ": "JWT", "kid": "kid123" })
        );
        assert_eq!(
            decode(parts[1]),
            json!({
                "iss": "fridged@beer-cellar.iam.gserviceaccount.com",
                "scope": "https://www.googleapis.com/auth/datastore",
                "aud": "https://oauth2.googleapis.com/token",
                "iat": 1_760_000_000,
                "exp": 1_760_003_600,
            })
        );
    }

    #[test]
    fn jwt_signature_verifies_with_the_public_key() {
        let signer = Signer::new(&account()).unwrap();
        let jwt = signer.jwt(1_760_000_000).unwrap();
        let (signed, signature) = jwt.rsplit_once('.').unwrap();
        let public =
            UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, signer.key.public().as_ref());
        public
            .verify(
                signed.as_bytes(),
                &URL_SAFE_NO_PAD.decode(signature).unwrap(),
            )
            .unwrap();
        // A tampered payload does not.
        let tampered = signed.replace('.', ".e");
        assert!(
            public
                .verify(
                    tampered.as_bytes(),
                    &URL_SAFE_NO_PAD.decode(signature).unwrap()
                )
                .is_err()
        );
    }

    #[test]
    fn token_uri_defaults_to_google() {
        let mut v = serde_json::to_value(json!({
            "project_id": "p", "private_key_id": "k", "private_key": TEST_KEY,
            "client_email": "e",
        }))
        .unwrap();
        v.as_object_mut().unwrap().remove("token_uri");
        let sa: ServiceAccount = serde_json::from_value(v).unwrap();
        assert_eq!(sa.token_uri, "https://oauth2.googleapis.com/token");
    }

    #[test]
    fn debug_never_shows_the_private_key() {
        let shown = format!("{:?} {:?}", account(), Signer::new(&account()).unwrap());
        assert!(!shown.contains("PRIVATE KEY"), "{shown}");
        assert!(!shown.contains(&TEST_KEY[40..80]), "{shown}");
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn bad_keys_are_rejected() {
        let mut sa = account();
        sa.private_key = "not a key".to_owned();
        assert!(matches!(Signer::new(&sa), Err(AuthError::Key(_))));
        sa.private_key = "-----BEGIN PRIVATE KEY-----\n!!!!\n-----END PRIVATE KEY-----".to_owned();
        assert!(matches!(Signer::new(&sa), Err(AuthError::Key(_))));
    }
}
