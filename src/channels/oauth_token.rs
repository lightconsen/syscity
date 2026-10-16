//! Outbound OAuth access tokens, for the channels that have to mint one.
//!
//! LINE and Twilio send with a long-lived credential — a channel access token,
//! an auth token — and never think about it again. Teams and Google Chat do
//! not: their APIs take an OAuth access token that expires within the hour, and
//! it has to be obtained from a token endpoint by presenting either a client
//! secret or a signed assertion.
//!
//! The two grants differ only in what goes in the request body, so they share
//! everything else: one cache, one refresh rule, one error path. A channel asks
//! for [`TokenSource::token`] and gets something it can put in an
//! `Authorization` header; when it was fetched, and whether it is about to
//! expire, is not the channel's business.

use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::debug;

use crate::error::SyscityError;

/// How long before real expiry a token is treated as expired.
///
/// A token that expires mid-request is a failed delivery, and the difference
/// between this process's clock and the issuer's is not knowable, so the margin
/// is generous rather than exact.
const REFRESH_MARGIN: Duration = Duration::from_secs(120);

/// The lifetime asked for in a signed assertion.
///
/// The issuer decides the token's real lifetime and reports it back; this is
/// only what the assertion claims, and is the conventional hour.
const ASSERTION_LIFETIME_SECS: i64 = 3600;

/// How an access token is obtained.
pub enum Grant {
    /// `client_credentials` — the client id and secret *are* the credential.
    ///
    /// This is the Bot Framework's shape: an Azure AD app registration.
    ClientCredentials {
        /// The registered application's id.
        client_id: String,
        /// Its secret.
        client_secret: String,
        /// The scope to ask for, e.g. `https://api.botframework.com/.default`.
        scope: String,
    },
    /// `jwt-bearer` — a service account signs an assertion with its private key.
    ///
    /// This is Google's shape for server-to-server calls: the assertion is the
    /// credential, and the private key never leaves this process.
    JwtBearer {
        /// The service account's address, which is the assertion's issuer.
        client_email: String,
        /// Its private key, PEM-encoded.
        private_key_pem: String,
        /// The scope to ask for, e.g. `https://www.googleapis.com/auth/chat.bot`.
        scope: String,
    },
}

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The credentials are deliberately not printed.
        match self {
            Grant::ClientCredentials { scope, .. } => f
                .debug_struct("ClientCredentials")
                .field("scope", scope)
                .finish_non_exhaustive(),
            Grant::JwtBearer { client_email, scope, .. } => f
                .debug_struct("JwtBearer")
                .field("client_email", client_email)
                .field("scope", scope)
                .finish_non_exhaustive(),
        }
    }
}

/// The token endpoint's answer, in the subset this uses.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    /// Seconds until expiry. Optional because not every issuer sends it, and a
    /// missing one is not a reason to refuse a working token.
    #[serde(default)]
    expires_in: Option<u64>,
}

/// A token and the moment it stops being usable.
struct CachedToken {
    value: String,
    expires_at: Instant,
}

/// Mints and caches access tokens for one issuer.
pub struct TokenSource {
    token_url: String,
    grant: Grant,
    http: reqwest::Client,
    cache: RwLock<Option<CachedToken>>,
}

impl std::fmt::Debug for TokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSource")
            .field("token_url", &self.token_url)
            .field("grant", &self.grant)
            .finish()
    }
}

impl TokenSource {
    /// Create a source for one token endpoint.
    pub fn new(token_url: impl Into<String>, grant: Grant) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            token_url: token_url.into(),
            grant,
            http,
            cache: RwLock::new(None),
        }
    }

    /// A usable access token, fetching or refreshing it as needed.
    ///
    /// Two callers that miss the cache at the same moment will both fetch. That
    /// costs one extra request and yields two valid tokens, which is a better
    /// trade than serialising every send behind a refresh.
    pub async fn token(&self) -> crate::Result<String> {
        {
            let cache = self.cache.read().await;
            if let Some(cached) = cache.as_ref() {
                if Instant::now() < cached.expires_at {
                    return Ok(cached.value.clone());
                }
            }
        }

        let fetched = self.fetch().await?;
        let value = fetched.value.clone();
        *self.cache.write().await = Some(fetched);
        debug!("oauth: acquired an access token");

        Ok(value)
    }

    /// Ask the issuer for a token.
    async fn fetch(&self) -> crate::Result<CachedToken> {
        let form = match &self.grant {
            Grant::ClientCredentials {
                client_id,
                client_secret,
                scope,
            } => vec![
                ("grant_type".to_string(), "client_credentials".to_string()),
                ("client_id".to_string(), client_id.clone()),
                ("client_secret".to_string(), client_secret.clone()),
                ("scope".to_string(), scope.clone()),
            ],
            Grant::JwtBearer {
                client_email,
                private_key_pem,
                scope,
            } => vec![
                (
                    "grant_type".to_string(),
                    "urn:ietf:params:oauth:grant-type:jwt-bearer".to_string(),
                ),
                ("assertion".to_string(), self.assertion(client_email, private_key_pem, scope)?),
            ],
        };

        let response = self
            .http
            .post(&self.token_url)
            .form(&form)
            .send()
            .await
            .map_err(|e| SyscityError::ExternalService {
                source: format!("token request failed: {}", e),
                cause: None,
            })?;

        if !response.status().is_success() {
            let status = response.status();
            // The body names the cause (`invalid_client`, `invalid_scope`),
            // which is the difference between a typo and a revoked credential.
            let detail = response.text().await.unwrap_or_default();
            return Err(SyscityError::ExternalService {
                source: format!("token request returned {}: {}", status, detail),
                cause: None,
            });
        }

        let body: TokenResponse =
            response
                .json()
                .await
                .map_err(|e| SyscityError::ExternalService {
                    source: format!("token response is not a token: {}", e),
                    cause: None,
                })?;

        if body.access_token.is_empty() {
            return Err(SyscityError::ExternalService {
                source: "token response carried an empty access token".to_string(),
                cause: None,
            });
        }

        let lifetime =
            Duration::from_secs(body.expires_in.unwrap_or(ASSERTION_LIFETIME_SECS as u64));

        Ok(CachedToken {
            value: body.access_token,
            // `saturating_sub` matters: a very short-lived token would otherwise
            // underflow into a huge duration and be cached long past its life.
            expires_at: Instant::now() + lifetime.saturating_sub(REFRESH_MARGIN),
        })
    }

    /// Sign the assertion a `jwt-bearer` grant presents.
    ///
    /// The assertion is a short-lived RS256 JWT whose signature *is* the
    /// credential: it proves possession of the service account's private key,
    /// which is why the key must never be logged or echoed anywhere.
    fn assertion(
        &self,
        client_email: &str,
        private_key_pem: &str,
        scope: &str,
    ) -> crate::Result<String> {
        let key =
            jsonwebtoken::EncodingKey::from_rsa_pem(private_key_pem.as_bytes()).map_err(|e| {
                SyscityError::Validation(format!(
                    "service-account key is not a usable RSA key: {}",
                    e
                ))
            })?;

        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "iss": client_email,
            "scope": scope,
            // The assertion is addressed to the endpoint that will accept it.
            "aud": self.token_url,
            "iat": now,
            "exp": now + ASSERTION_LIFETIME_SECS,
        });

        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        jsonwebtoken::encode(&header, &claims, &key)
            .map_err(|e| SyscityError::Internal(format!("could not sign the assertion: {}", e)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::EncodePrivateKey as _;
    use rsa::traits::PublicKeyParts as _;
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    /// A throwaway RSA keypair, generated per test rather than embedded — a
    /// private key committed to the repository would trip the commit gate's
    /// secret scan, and a real one would be worse.
    fn test_key() -> (String, String) {
        let private =
            rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("generate a test key");

        let private_pem = private
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .expect("encode the private key")
            .to_string();

        let public = private.to_public_key();
        let public_pem = format!(
            "-----BEGIN PUBLIC KEY-----\n{}\n-----END PUBLIC KEY-----\n",
            base64_wrapped(&public_der(&public))
        );

        (private_pem, public_pem)
    }

    /// DER for an RSAPublicKey, via the `pkcs8`/`spki` traits.
    fn public_der(public: &rsa::RsaPublicKey) -> Vec<u8> {
        use rsa::pkcs8::EncodePublicKey as _;
        public
            .to_public_key_der()
            .expect("encode the public key")
            .as_bytes()
            .to_vec()
    }

    fn base64_wrapped(der: &[u8]) -> String {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(der);
        b64.as_bytes()
            .chunks(64)
            .map(|c| String::from_utf8_lossy(c).to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn token_response(token: &str, expires_in: u64) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": token,
            "token_type": "Bearer",
            "expires_in": expires_in,
        }))
    }

    #[tokio::test]
    async fn client_credentials_posts_the_grant_and_caches_the_token() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .and(body_string_contains("grant_type=client_credentials"))
            .and(body_string_contains("client_id=the-client"))
            .respond_with(token_response("token-one", 3600))
            .expect(1) // exactly once: the second call must come from the cache
            .mount(&server)
            .await;

        let source = TokenSource::new(
            format!("{}/token", server.uri()),
            Grant::ClientCredentials {
                client_id: "the-client".to_string(),
                client_secret: "the-secret".to_string(),
                scope: "the-scope".to_string(),
            },
        );

        assert_eq!(source.token().await.expect("a token"), "token-one");
        assert_eq!(
            source.token().await.expect("a token"),
            "token-one",
            "the second call is served from the cache"
        );
    }

    /// A token whose lifetime is inside the refresh margin is treated as
    /// already expired, so it is fetched again rather than used and failing
    /// mid-request.
    #[tokio::test]
    async fn a_token_expiring_within_the_margin_is_refetched() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            // Zero seconds, so the refresh margin swallows the whole lifetime.
            .respond_with(token_response("short-lived", 0))
            .expect(2)
            .mount(&server)
            .await;

        let source = TokenSource::new(
            format!("{}/token", server.uri()),
            Grant::ClientCredentials {
                client_id: "c".to_string(),
                client_secret: "s".to_string(),
                scope: "sc".to_string(),
            },
        );

        source.token().await.expect("a token");
        source.token().await.expect("a token");
    }

    /// The assertion is the credential, so what matters is that the issuer
    /// could verify it: a real RS256 signature over the documented claims,
    /// addressed to the token endpoint.
    #[tokio::test]
    async fn jwt_bearer_signs_a_verifiable_assertion() {
        let (private_pem, public_pem) = test_key();
        let server = MockServer::start().await;

        let captured: std::sync::Arc<std::sync::Mutex<Option<String>>> = Default::default();
        let sink = captured.clone();

        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(move |req: &Request| {
                // Pull the assertion back out of the form body, decoded the way
                // the token endpoint would decode it.
                let assertion = url::form_urlencoded::parse(&req.body)
                    .find(|(k, _)| k == "assertion")
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                *sink.lock().expect("capture lock") = Some(assertion);
                token_response("google-token", 3600)
            })
            .mount(&server)
            .await;

        let token_url = format!("{}/token", server.uri());
        let source = TokenSource::new(
            token_url.clone(),
            Grant::JwtBearer {
                client_email: "bot@project.iam.gserviceaccount.com".to_string(),
                private_key_pem: private_pem,
                scope: "https://www.googleapis.com/auth/chat.bot".to_string(),
            },
        );

        assert_eq!(source.token().await.expect("a token"), "google-token");

        let assertion = captured
            .lock()
            .expect("capture lock")
            .clone()
            .expect("the assertion was posted");

        // Verify it the way the issuer would.
        let key =
            jsonwebtoken::DecodingKey::from_rsa_pem(public_pem.as_bytes()).expect("decoding key");
        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_audience(&[token_url.as_str()]);
        validation.set_issuer(&["bot@project.iam.gserviceaccount.com"]);
        validation.validate_exp = false;

        let claims = jsonwebtoken::decode::<serde_json::Value>(&assertion, &key, &validation)
            .expect("the assertion verifies")
            .claims;

        assert_eq!(
            claims["scope"], "https://www.googleapis.com/auth/chat.bot",
            "the assertion asks for the configured scope"
        );
    }

    /// Nothing is cached from a failed fetch, so a later call tries again
    /// rather than serving a token that was never issued.
    #[tokio::test]
    async fn a_failed_fetch_is_an_error_and_caches_nothing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid_client"))
            .mount(&server)
            .await;

        let source = TokenSource::new(
            format!("{}/token", server.uri()),
            Grant::ClientCredentials {
                client_id: "c".to_string(),
                client_secret: "wrong".to_string(),
                scope: "sc".to_string(),
            },
        );

        let first = source.token().await;
        assert!(first.is_err(), "a refused token request must not yield a token");
        assert!(
            first.unwrap_err().to_string().contains("invalid_client"),
            "the issuer's own reason is carried through"
        );

        assert!(source.token().await.is_err(), "and it is not cached as success");
    }

    #[tokio::test]
    async fn an_empty_or_unparseable_response_is_an_error() {
        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "access_token": "",
                "expires_in": 3600
            })))
            .mount(&server)
            .await;

        let source = TokenSource::new(
            format!("{}/token", server.uri()),
            Grant::ClientCredentials {
                client_id: "c".to_string(),
                client_secret: "s".to_string(),
                scope: "sc".to_string(),
            },
        );
        assert!(source.token().await.is_err(), "an empty token is not a token");

        let other = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&other)
            .await;

        let source = TokenSource::new(
            format!("{}/token", other.uri()),
            Grant::ClientCredentials {
                client_id: "c".to_string(),
                client_secret: "s".to_string(),
                scope: "sc".to_string(),
            },
        );
        assert!(source.token().await.is_err());
    }

    #[tokio::test]
    async fn a_key_that_is_not_a_key_is_refused_before_any_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/token"))
            .respond_with(token_response("never", 3600))
            .expect(0) // a bad key must not reach the network
            .mount(&server)
            .await;

        let source = TokenSource::new(
            format!("{}/token", server.uri()),
            Grant::JwtBearer {
                client_email: "bot@example.iam.gserviceaccount.com".to_string(),
                private_key_pem: "not a pem".to_string(),
                scope: "scope".to_string(),
            },
        );

        assert!(source.token().await.is_err());
    }
}
