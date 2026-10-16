//! LINE Messaging API channel.
//!
//! Inbound arrives as a webhook: LINE POSTs a batch of `events` to a single
//! URL and signs the *raw body* with the channel secret (the route and the
//! shared inbound tail live in `gateway/webhooks.rs`). This module owns the
//! outbound half — pushing a reply through the Messaging API — and the
//! signature check the handler calls.
//!
//! There are two credentials and they are not interchangeable. The **channel
//! secret** verifies what LINE sends us; the **channel access token**
//! authorises what we send to LINE. A config that swaps them fails in both
//! directions, which is why they are separate fields rather than one `token`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::channels::{
    Channel, ChannelCapabilities, ChatType, ConversationId, OutgoingMessage, PlainTextFormatter,
};
use crate::core::models::Id;
use crate::error::SyscityError;

/// LINE Messaging API base URL.
const LINE_API_BASE: &str = "https://api.line.me/v2/bot";

/// The header LINE signs a webhook delivery with.
pub const LINE_SIGNATURE_HEADER: &str = "x-line-signature";

/// LINE channel configuration.
#[derive(Debug, Clone)]
pub struct LineConfig {
    /// Channel access token — the credential for the outbound API.
    pub channel_access_token: String,
    /// Channel secret — the key the inbound webhook signature is computed
    /// with. Not the same credential as the access token.
    pub channel_secret: String,
}

impl LineConfig {
    /// Create a config from the two credentials.
    pub fn new(channel_access_token: impl Into<String>, channel_secret: impl Into<String>) -> Self {
        Self {
            channel_access_token: channel_access_token.into(),
            channel_secret: channel_secret.into(),
        }
    }
}

/// Verify a LINE webhook signature.
///
/// LINE signs the raw request body with the channel secret and carries the
/// result base64-encoded:
///
/// ```text
/// X-Line-Signature: base64(HMAC-SHA256(channel_secret, body))
/// ```
///
/// The signature covers the bytes *as sent*, so the caller must hand over the
/// body it received rather than a re-serialisation of the parsed JSON —
/// re-encoding changes whitespace and key order and the signature stops
/// matching. Comparison is constant-time.
pub fn verify_signature(channel_secret: &str, body: &[u8], provided: &str) -> bool {
    use base64::Engine as _;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    use subtle::ConstantTimeEq;

    let mut mac = match Hmac::<Sha256>::new_from_slice(channel_secret.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            warn!("LINE webhook: could not build an HMAC from the channel secret");
            return false;
        }
    };
    mac.update(body);
    let computed = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    computed.as_bytes().ct_eq(provided.as_bytes()).into()
}

/// One text message extracted from a LINE webhook delivery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineIncoming {
    /// The LINE user id the message came from.
    pub user_id: String,
    /// The message text.
    pub text: String,
}

/// Extract the text messages from a webhook payload.
///
/// A delivery carries an `events` array and most of it is not a message: LINE
/// reports follows, unfollows, joins, leaves and postbacks through the same
/// endpoint. Only `message` events carrying `text` are returned, and an empty
/// text is dropped rather than passed on — an empty string would otherwise
/// reach the agent as a turn with nothing in it.
pub fn parse_events(body: &[u8]) -> crate::Result<Vec<LineIncoming>> {
    #[derive(Deserialize)]
    struct Envelope {
        #[serde(default)]
        events: Vec<Event>,
    }

    #[derive(Deserialize)]
    struct Event {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        message: Option<Message>,
        #[serde(default)]
        source: Option<Source>,
    }

    #[derive(Deserialize)]
    struct Message {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        text: Option<String>,
    }

    #[derive(Deserialize)]
    struct Source {
        #[serde(rename = "userId")]
        user_id: Option<String>,
    }

    let envelope: Envelope = serde_json::from_slice(body)
        .map_err(|e| SyscityError::Validation(format!("Invalid LINE webhook payload: {}", e)))?;

    Ok(envelope
        .events
        .into_iter()
        .filter(|event| event.kind == "message")
        .filter_map(|event| {
            let message = event.message?;
            if message.kind != "text" {
                return None;
            }
            let text = message.text?;
            let user_id = event.source.and_then(|s| s.user_id)?;
            if text.trim().is_empty() {
                return None;
            }
            Some(LineIncoming { user_id, text })
        })
        .collect())
}

/// LINE channel implementation.
pub struct LineChannel {
    config: LineConfig,
    http_client: reqwest::Client,
    running: Arc<AtomicBool>,
}

impl std::fmt::Debug for LineChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The credentials are deliberately not printed.
        f.debug_struct("LineChannel")
            .field("running", &self.running)
            .finish()
    }
}

impl LineChannel {
    /// Create a new LINE channel.
    pub fn new(config: LineConfig) -> Self {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            config,
            http_client,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Push a text message to a LINE user.
    ///
    /// This is the *push* endpoint rather than *reply*: a reply is only valid
    /// with the `replyToken` from the delivery that prompted it and expires
    /// within a minute, which an agent turn routinely outlives.
    async fn push_text(&self, user_id: &str, text: &str) -> crate::Result<()> {
        let payload = serde_json::json!({
            "to": user_id,
            "messages": [{ "type": "text", "text": text }],
        });

        let response = self
            .http_client
            .post(format!("{}/message/push", LINE_API_BASE))
            .bearer_auth(&self.config.channel_access_token)
            .json(&payload)
            .send()
            .await
            .map_err(|e| SyscityError::ExternalService {
                source: format!("LINE push failed: {}", e),
                cause: None,
            })?;

        if !response.status().is_success() {
            let status = response.status();
            // The body carries LINE's own error detail (`{"message": ...}`),
            // which is the difference between "bad token" and "user blocked
            // the bot" — so it goes in the error rather than being dropped.
            let detail = response.text().await.unwrap_or_default();
            return Err(SyscityError::ExternalService {
                source: format!("LINE push returned {}: {}", status, detail),
                cause: None,
            });
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for LineChannel {
    fn name(&self) -> &str {
        "line"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct, ChatType::Group],
            // The text message type is plain: LINE renders no markdown, so a
            // formatted reply would arrive with its asterisks intact.
            supports_formatting: false,
            supports_attachments: false,
            supports_images: false,
            supports_threads: false,
            supports_typing: false,
            supports_buttons: false,
            supports_commands: true,
            supports_reactions: false,
            supports_edit: false,
            supports_unsend: false,
            supports_effects: false,
        }
    }

    async fn start(&self) -> crate::Result<()> {
        info!("Starting LINE channel...");
        self.running.store(true, Ordering::SeqCst);
        info!("LINE channel started (inbound via /webhooks/line)");

        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping LINE channel...");
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let recipient = &message.conversation_id.0;

        // LINE has no markdown, so markdown is stripped rather than sent
        // literally — the alternative is a reply full of `**`.
        let text = match &message.formatted_content {
            Some(crate::channels::FormattedContent::Markdown(md)) => {
                PlainTextFormatter::strip_markdown(md)
            }
            _ => PlainTextFormatter::strip_markdown(&message.content),
        };

        self.push_text(recipient, &text).await?;
        debug!("LINE message pushed to {}", recipient);
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // The push API has no typing indicator.
        Ok(())
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        Err(SyscityError::Unsupported("LINE messages cannot be edited".into()))
    }

    async fn delete_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(SyscityError::Unsupported("LINE messages cannot be unsent".into()))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use hmac::{Hmac, Mac};
    use sha2::Sha256;

    /// Sign the way LINE does, so the verifier is tested against the documented
    /// construction rather than against itself.
    fn line_signature(secret: &str, body: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("hmac key");
        mac.update(body);
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    #[test]
    fn signature_matches_the_documented_construction() {
        let body = br#"{"events":[]}"#;
        let sig = line_signature("channel-secret", body);
        assert!(verify_signature("channel-secret", body, &sig));
    }

    #[test]
    fn signature_is_refused_for_the_wrong_secret_or_a_tampered_body() {
        let body = br#"{"events":[]}"#;
        let sig = line_signature("channel-secret", body);

        assert!(!verify_signature("not-the-secret", body, &sig));
        assert!(!verify_signature("channel-secret", br#"{"events":[1]}"#, &sig));
        assert!(!verify_signature("channel-secret", body, ""));
        assert!(!verify_signature("channel-secret", body, "not-base64-at-all"));
    }

    /// The signature covers the bytes as sent, so whitespace matters. This is
    /// the reason the handler must verify before parsing, not after.
    #[test]
    fn signature_is_over_the_raw_bytes_not_the_parsed_json() {
        let compact = br#"{"events":[]}"#;
        let spaced = b"{ \"events\" : [] }";
        let sig = line_signature("channel-secret", compact);

        assert!(verify_signature("channel-secret", compact, &sig));
        assert!(
            !verify_signature("channel-secret", spaced, &sig),
            "re-serialised JSON must not verify against a signature over the original"
        );
    }

    #[test]
    fn parse_events_extracts_text_messages() {
        let body = br#"{
            "destination": "U_bot",
            "events": [
                {
                    "type": "message",
                    "replyToken": "rt-1",
                    "source": { "type": "user", "userId": "U_alice" },
                    "message": { "type": "text", "id": "m1", "text": "hello there" }
                }
            ]
        }"#;

        let events = parse_events(body).expect("payload parses");
        assert_eq!(
            events,
            vec![LineIncoming {
                user_id: "U_alice".to_string(),
                text: "hello there".to_string()
            }]
        );
    }

    #[test]
    fn parse_events_skips_everything_that_is_not_a_text_message() {
        let body = br#"{
            "events": [
                { "type": "follow", "source": { "type": "user", "userId": "U_1" } },
                {
                    "type": "message",
                    "source": { "type": "user", "userId": "U_2" },
                    "message": { "type": "image", "id": "m2" }
                },
                {
                    "type": "message",
                    "source": { "type": "user", "userId": "U_3" },
                    "message": { "type": "text", "id": "m3", "text": "   " }
                },
                {
                    "type": "postback",
                    "source": { "type": "user", "userId": "U_4" },
                    "postback": { "data": "x" }
                },
                {
                    "type": "message",
                    "source": { "type": "group", "groupId": "C_1" },
                    "message": { "type": "text", "id": "m5", "text": "no user id" }
                }
            ]
        }"#;

        assert!(
            parse_events(body).expect("payload parses").is_empty(),
            "only text messages from a user source are messages"
        );
    }

    #[test]
    fn parse_events_rejects_a_body_that_is_not_json() {
        assert!(parse_events(b"not json").is_err());
    }

    #[test]
    fn config_keeps_the_two_credentials_apart() {
        let config = LineConfig::new("access-token", "channel-secret");
        assert_eq!(config.channel_access_token, "access-token");
        assert_eq!(config.channel_secret, "channel-secret");
    }
}
