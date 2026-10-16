//! Bearer-token (JWT) verification for the webhook channels that use one.
//!
//! Most channels authenticate a delivery with a shared secret: the platform
//! signs the body and we recompute the signature. Teams and Google Chat do not
//! — they issue a **JWT** and publish the signing keys — so verifying a delivery
//! means checking a JOSE signature against a key set fetched over the network.
//!
//! That is a different kind of work from the HMAC helpers in `webhooks.rs`, and
//! it is why it lives here: both channels need exactly this and nothing else,
//! and the parts that are easy to get wrong are worth having in one place.
//!
//! Four properties this deliberately holds:
//!
//! - **Fail closed.** Every error path returns `Err`. There is no "could not
//!   fetch keys, let it through" branch.
//! - **One algorithm.** Only RS256 is accepted, and it is chosen by *this*
//!   code rather than read from the token. Negotiating the algorithm the token
//!   names is the confusion attack this shape of code is known for: an `HS256`
//!   token verified with the published RSA key as an HMAC secret would let
//!   anyone who can read the key set mint a valid token.
//! - **Issuer and audience are always checked.** A signature that verifies
//!   against the right key but names the wrong audience is a token minted for
//!   somebody else.
//! - **Rotation does not drop deliveries.** An unknown key id refetches the set
//!   once before refusing.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::error::SyscityError;

/// How long a fetched key set is reused before it is refetched.
///
/// Rotation is handled by refetching on an unknown key id as well, so this is a
/// bound on staleness rather than the only path to a new key.
const JWKS_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// How much clock skew to tolerate when checking `exp` and `nbf`.
///
/// Microsoft documents ±5 minutes for the Bot Framework. The same tolerance is
/// harmless for Google's tokens and is what a gateway whose clock is a few
/// seconds off needs in order to keep working at all.
const LEEWAY_SECS: u64 = 300;

/// A JSON Web Key Set, as published by the issuer.
#[derive(Debug, Deserialize)]
struct JwkSet {
    keys: Vec<Jwk>,
}

/// One published key. Only the fields a verifier needs are read.
#[derive(Debug, Deserialize)]
struct Jwk {
    /// Key id, matched against the token header.
    kid: Option<String>,
    /// Key type. Only `RSA` is usable here.
    kty: String,
    /// RSA modulus, base64url.
    n: Option<String>,
    /// RSA exponent, base64url.
    e: Option<String>,
    /// Intended use. `sig` is the only one that belongs in a verifier.
    #[serde(rename = "use")]
    use_: Option<String>,
}

/// A key set and the moment it was fetched.
struct CachedKeys {
    keys: HashMap<String, jsonwebtoken::DecodingKey>,
    fetched_at: Instant,
}

/// Verifies bearer tokens against an issuer's published keys.
///
/// Cheap to clone-share behind an `Arc`; the cache is shared, the configuration
/// is not.
pub struct JwtVerifier {
    issuer: String,
    audience: String,
    jwks_url: String,
    http: reqwest::Client,
    cache: RwLock<Option<CachedKeys>>,
}

impl std::fmt::Debug for JwtVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JwtVerifier")
            .field("issuer", &self.issuer)
            .field("audience", &self.audience)
            .field("jwks_url", &self.jwks_url)
            .finish()
    }
}

impl JwtVerifier {
    /// Create a verifier for one issuer.
    ///
    /// All three values are configuration rather than constants: Teams and
    /// Google Chat both have more than one issuer in the wild (sovereign
    /// clouds; Google's two audience modes), and a hardcoded guess would either
    /// reject every delivery or — worse — accept tokens minted for a different
    /// audience.
    pub fn new(
        issuer: impl Into<String>,
        audience: impl Into<String>,
        jwks_url: impl Into<String>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            jwks_url: jwks_url.into(),
            http,
            cache: RwLock::new(None),
        }
    }

    /// The issuer this verifier accepts. Exposed for the startup log line, so
    /// an operator can see what the gateway believes it is checking.
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Verify a bearer token and return its claims.
    pub async fn verify(&self, token: &str) -> crate::Result<serde_json::Value> {
        let header = jsonwebtoken::decode_header(token)
            .map_err(|e| rejected(format!("malformed token: {}", e)))?;

        // The algorithm is decided here, not by the token.
        if header.alg != jsonwebtoken::Algorithm::RS256 {
            return Err(rejected(format!(
                "unsupported algorithm {:?}; only RS256 is accepted",
                header.alg
            )));
        }

        let kid = header
            .kid
            .ok_or_else(|| rejected("token carries no key id".to_string()))?;

        let key = self.key_for(&kid).await?;

        let mut validation = jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256);
        validation.set_issuer(&[self.issuer.as_str()]);
        validation.set_audience(&[self.audience.as_str()]);
        validation.leeway = LEEWAY_SECS;
        validation.validate_exp = true;

        let data = jsonwebtoken::decode::<serde_json::Value>(token, &key, &validation)
            .map_err(|e| rejected(format!("token rejected: {}", e)))?;

        Ok(data.claims)
    }

    /// The key for `kid`, refetching the set once if it is not already held.
    async fn key_for(&self, kid: &str) -> crate::Result<jsonwebtoken::DecodingKey> {
        if let Some(key) = self.cached_key(kid).await {
            return Ok(key);
        }

        // An unknown key id means either a rotation or a forgery. Refetching
        // once is what keeps a rotation from breaking deliveries; the lookup
        // still fails afterwards if the key genuinely is not published.
        self.refresh().await?;

        self.cached_key(kid)
            .await
            .ok_or_else(|| rejected(format!("no published signing key with id {}", kid)))
    }

    /// A cached key for `kid`, or `None` when it is absent or the cache is stale.
    async fn cached_key(&self, kid: &str) -> Option<jsonwebtoken::DecodingKey> {
        let cache = self.cache.read().await;
        let cached = cache.as_ref()?;
        if cached.fetched_at.elapsed() > JWKS_TTL {
            return None;
        }
        cached.keys.get(kid).cloned()
    }

    /// Fetch and cache the key set.
    async fn refresh(&self) -> crate::Result<()> {
        let response = self.http.get(&self.jwks_url).send().await.map_err(|e| {
            SyscityError::ExternalService {
                source: format!("key set fetch failed: {}", e),
                cause: None,
            }
        })?;

        if !response.status().is_success() {
            return Err(SyscityError::ExternalService {
                source: format!("key set fetch returned {}", response.status()),
                cause: None,
            });
        }

        let set: JwkSet = response
            .json()
            .await
            .map_err(|e| SyscityError::ExternalService {
                source: format!("key set is not a JWKS document: {}", e),
                cause: None,
            })?;

        let mut keys = HashMap::new();
        for jwk in set.keys {
            // A key with no id cannot be selected by a token, and a key that is
            // not for signing has no business in a verifier.
            let Some(kid) = jwk.kid else { continue };
            if jwk.kty != "RSA" || jwk.use_.as_deref().is_some_and(|u| u != "sig") {
                continue;
            }
            let (Some(n), Some(e)) = (jwk.n, jwk.e) else {
                continue;
            };
            match jsonwebtoken::DecodingKey::from_rsa_components(&n, &e) {
                Ok(key) => {
                    keys.insert(kid, key);
                }
                Err(err) => warn!("key set: skipping unusable RSA key {}: {}", kid, err),
            }
        }

        if keys.is_empty() {
            return Err(SyscityError::ExternalService {
                source: "key set contained no usable signing keys".to_string(),
                cause: None,
            });
        }

        debug!("key set: cached {} signing key(s)", keys.len());
        *self.cache.write().await = Some(CachedKeys {
            keys,
            fetched_at: Instant::now(),
        });

        Ok(())
    }
}

/// A token that did not verify.
///
/// Every rejection is this one error. The caller turns it into a 401 and says
/// nothing about which check failed — "wrong audience" and "bad signature" are
/// both just "no" to the sender.
fn rejected(reason: String) -> SyscityError {
    SyscityError::Validation(format!("bearer token rejected: {}", reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use rsa::pkcs8::EncodePrivateKey as _;
    use rsa::traits::PublicKeyParts;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ISSUER: &str = "https://issuer.example";
    const AUDIENCE: &str = "the-audience";
    const KID: &str = "test-key-1";

    /// A freshly generated RSA keypair and the JWKS document publishing it.
    ///
    /// Generated per test rather than embedded: a private key committed to the
    /// repository would trip the commit gate's secret scan, and a real key
    /// would be worse still.
    struct TestKey {
        encoding: jsonwebtoken::EncodingKey,
        jwks: serde_json::Value,
    }

    fn base64_url(bytes: &[u8]) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    }

    fn test_key() -> TestKey {
        let private =
            rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).expect("generate a test key");
        let public = private.to_public_key();

        let pem = private
            .to_pkcs8_pem(rsa::pkcs8::LineEnding::LF)
            .expect("encode the test key");

        TestKey {
            encoding: jsonwebtoken::EncodingKey::from_rsa_pem(pem.as_bytes())
                .expect("build an encoding key"),
            jwks: serde_json::json!({
                "keys": [{
                    "kty": "RSA",
                    "use": "sig",
                    "alg": "RS256",
                    "kid": KID,
                    "n": base64_url(&public.n().to_bytes_be()),
                    "e": base64_url(&public.e().to_bytes_be()),
                }]
            }),
        }
    }

    /// Mint a token the way a platform would.
    fn mint(key: &TestKey, issuer: &str, audience: &str, lifetime_secs: i64) -> String {
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "iss": issuer,
            "aud": audience,
            "sub": "a-subject",
            "iat": now,
            "exp": now + lifetime_secs,
        });

        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some(KID.to_string());

        jsonwebtoken::encode(&header, &claims, &key.encoding).expect("mint a token")
    }

    async fn serving(key: &TestKey) -> (MockServer, JwtVerifier) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&key.jwks))
            .mount(&server)
            .await;

        let verifier = JwtVerifier::new(ISSUER, AUDIENCE, format!("{}/jwks", server.uri()));
        (server, verifier)
    }

    /// The case that must keep working — a verifier that rejected everything
    /// would pass every negative test below.
    #[tokio::test]
    async fn a_correctly_signed_token_verifies() {
        let key = test_key();
        let (_server, verifier) = serving(&key).await;

        let token = mint(&key, ISSUER, AUDIENCE, 300);
        let claims = verifier
            .verify(&token)
            .await
            .expect("a valid token verifies");

        assert_eq!(claims["sub"], "a-subject");
    }

    #[tokio::test]
    async fn a_token_for_another_audience_or_issuer_is_refused() {
        let key = test_key();
        let (_server, verifier) = serving(&key).await;

        let wrong_audience = mint(&key, ISSUER, "somebody-elses-audience", 300);
        assert!(
            verifier.verify(&wrong_audience).await.is_err(),
            "a token minted for a different audience must not be accepted"
        );

        let wrong_issuer = mint(&key, "https://elsewhere.example", AUDIENCE, 300);
        assert!(verifier.verify(&wrong_issuer).await.is_err());
    }

    #[tokio::test]
    async fn an_expired_token_is_refused() {
        let key = test_key();
        let (_server, verifier) = serving(&key).await;

        // Well past the leeway, so this is expiry rather than clock skew.
        let expired = mint(&key, ISSUER, AUDIENCE, -(LEEWAY_SECS as i64 + 600));
        assert!(verifier.verify(&expired).await.is_err());
    }

    /// The algorithm-confusion attack this module exists to be immune to: the
    /// key set is public, so if an `HS256` token were verified with the RSA
    /// public key as the HMAC secret, anyone who can read the JWKS could mint a
    /// token that verifies. The algorithm is chosen by the verifier, not read
    /// from the token.
    #[tokio::test]
    async fn an_hs256_token_is_refused() {
        let key = test_key();
        let (_server, verifier) = serving(&key).await;

        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({
            "iss": ISSUER, "aud": AUDIENCE, "exp": now + 300,
        });
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::HS256);
        header.kid = Some(KID.to_string());
        let forged = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"the-public-key"),
        )
        .expect("mint an HS256 token");

        assert!(
            verifier.verify(&forged).await.is_err(),
            "only RS256 is accepted, whatever the token claims"
        );
    }

    #[tokio::test]
    async fn an_unsigned_token_is_refused() {
        let key = test_key();
        let (_server, verifier) = serving(&key).await;

        // `alg: none`, built by hand — the JWT "none" algorithm is the other
        // half of the confusion family.
        let header = base64_url(br#"{"alg":"none","kid":"test-key-1"}"#);
        let payload = base64_url(br#"{"iss":"https://issuer.example","aud":"the-audience"}"#);
        let unsigned = format!("{}.{}.", header, payload);

        assert!(verifier.verify(&unsigned).await.is_err());
    }

    #[tokio::test]
    async fn a_malformed_token_or_one_without_a_key_id_is_refused() {
        let key = test_key();
        let (_server, verifier) = serving(&key).await;

        assert!(verifier.verify("not-a-jwt").await.is_err());

        // A well-formed token whose header carries no `kid` cannot select a key.
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({ "iss": ISSUER, "aud": AUDIENCE, "exp": now + 300 });
        let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        let no_kid = jsonwebtoken::encode(&header, &claims, &key.encoding).expect("mint a token");
        assert!(verifier.verify(&no_kid).await.is_err());
    }

    /// A rotation must not drop deliveries: an unknown key id refetches the set
    /// before refusing.
    #[tokio::test]
    async fn an_unknown_key_id_triggers_a_refetch() {
        let key = test_key();
        let (server, verifier) = serving(&key).await;

        // Warm the cache.
        verifier
            .verify(&mint(&key, ISSUER, AUDIENCE, 300))
            .await
            .expect("the first token verifies");

        // A token naming a key that is not published. It must be refused — but
        // only after asking the issuer again.
        let mut header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some("a-key-that-does-not-exist".to_string());
        let now = chrono::Utc::now().timestamp();
        let claims = serde_json::json!({ "iss": ISSUER, "aud": AUDIENCE, "exp": now + 300 });
        let unknown = jsonwebtoken::encode(&header, &claims, &key.encoding).expect("mint");

        assert!(verifier.verify(&unknown).await.is_err());

        let fetches = server
            .received_requests()
            .await
            .expect("request log")
            .iter()
            .filter(|r| r.url.path() == "/jwks")
            .count();
        assert_eq!(fetches, 2, "the key set must be refetched once when a key id is unknown");
    }

    #[tokio::test]
    async fn a_key_set_with_no_usable_keys_is_an_error_not_an_accept() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/jwks"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                // An encryption key, and a signing key with no id: neither can
                // verify a token, so the set is unusable rather than empty.
                "keys": [
                    { "kty": "RSA", "use": "enc", "kid": "enc-1", "n": "AQAB", "e": "AQAB" },
                    { "kty": "RSA", "use": "sig", "n": "AQAB", "e": "AQAB" }
                ]
            })))
            .mount(&server)
            .await;

        let verifier = JwtVerifier::new(ISSUER, AUDIENCE, format!("{}/jwks", server.uri()));
        let key = test_key();

        assert!(
            verifier
                .verify(&mint(&key, ISSUER, AUDIENCE, 300))
                .await
                .is_err(),
            "a set with nothing usable in it must fail closed"
        );
    }

    #[tokio::test]
    async fn an_unreachable_key_set_fails_closed() {
        // Nothing is mounted on this server, so the fetch 404s.
        let server = MockServer::start().await;
        let verifier = JwtVerifier::new(ISSUER, AUDIENCE, format!("{}/jwks", server.uri()));
        let key = test_key();

        assert!(
            verifier
                .verify(&mint(&key, ISSUER, AUDIENCE, 300))
                .await
                .is_err(),
            "a key set that cannot be fetched must not let a token through"
        );
    }
}
