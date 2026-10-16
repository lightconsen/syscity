//! Google Chat channel.
//!
//! Inbound arrives as a webhook carrying a Google-issued bearer token (the
//! route and the shared inbound tail live in `gateway/webhooks.rs`); this module
//! owns the outbound half — posting into a space through the Chat API.
//!
//! Three things make this channel unlike the signed ones:
//!
//! - **The sender is not the destination.** A message arrives from a *user*
//!   (`users/123`) inside a *space* (`spaces/S`), and a reply goes to the space.
//!   The session is keyed on the sender and the reply is addressed to the
//!   space, which is why the shared tail has a variant that takes both.
//! - **The bearer token names an identity, not just an audience.** Verifying
//!   the signature is not enough on its own: Google issues these tokens to
//!   *its own* service accounts, so the `email` claim is checked against a
//!   configured list. Without that, any Google-signed token for the right
//!   audience would be accepted.
//! - **Outbound is minted, not configured.** The Chat API takes an OAuth access
//!   token, obtained by signing an assertion with a service-account key.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::{debug, info};

use crate::channels::oauth_token::{Grant, TokenSource};
use crate::channels::{
    split_message, Channel, ChannelCapabilities, ChatType, ConversationId, OutgoingMessage,
    PlainTextFormatter,
};
use crate::core::models::Id;
use crate::error::SyscityError;
use crate::gateway::jwt_verify::JwtVerifier;

/// Google's OAuth token endpoint.
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The Chat API's base URL.
const GOOGLE_CHAT_API_BASE: &str = "https://chat.googleapis.com/v1";

/// The scope a Chat app posts with.
const GOOGLE_CHAT_SCOPE: &str = "https://www.googleapis.com/auth/chat.bot";

/// The most characters the Chat API accepts in one message.
pub const MAX_GOOGLE_CHAT_CHARS: usize = 4096;

/// Google Chat channel configuration.
#[derive(Debug, Clone)]
pub struct GoogleChatConfig {
    /// The issuer the inbound token must name.
    ///
    /// Not defaulted, because Google issues Chat tokens under more than one
    /// identity depending on the app's **Authentication Audience** setting, and
    /// a wrong guess here rejects every delivery. See `docs/channels.md`.
    pub issuer: String,
    /// The audience the inbound token must name — the app's endpoint URL or its
    /// GCP project number, matching the same console setting.
    pub audience: String,
    /// Where the issuer publishes its signing keys.
    pub jwks_url: String,
    /// The service accounts allowed to call the webhook.
    ///
    /// Google signs these tokens for its own service accounts, so a valid
    /// signature only proves Google issued it — not that it was issued for this
    /// app. An empty list refuses everything, which is the safe reading of
    /// "nobody is allowed".
    pub allowed_service_accounts: Vec<String>,
    /// The service account that posts replies.
    pub client_email: String,
    /// Its private key, PEM-encoded.
    pub private_key_pem: String,
}

impl GoogleChatConfig {
    /// The verifier for inbound deliveries.
    ///
    /// Built once at init and kept: a verifier per request would refetch the
    /// key set on every delivery, which is both slow and a good way to get rate
    /// limited by the issuer.
    pub fn verifier(&self) -> JwtVerifier {
        JwtVerifier::new(&self.issuer, &self.audience, &self.jwks_url)
    }

    /// Whether an inbound token's identity is one this app accepts.
    pub fn identity_is_allowed(&self, email: &str) -> bool {
        identity_is_allowed(&self.allowed_service_accounts, email)
    }
}

/// Whether `email` is one of the allowed identities.
///
/// Free-standing because the webhook handler holds the raw configuration value,
/// not a [`GoogleChatConfig`] — building one there would mean carrying a
/// service-account private key the handler has no use for.
///
/// An empty list allows nobody, which is the safe reading of "nobody is
/// allowed" and the one that fails closed when the setting is forgotten.
pub fn identity_is_allowed(allowed_service_accounts: &[String], email: &str) -> bool {
    let email = email.trim().to_ascii_lowercase();
    !email.is_empty()
        && allowed_service_accounts
            .iter()
            .any(|allowed| allowed.trim().to_ascii_lowercase() == email)
}

/// A message extracted from a Chat event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoogleChatIncoming {
    /// The sender, as a Chat resource name (`users/123`).
    pub user_id: String,
    /// The space to reply into (`spaces/S`).
    pub space: String,
    /// The message text.
    pub text: String,
}

/// Extract a user message from a Chat event, or `None` when it is not one.
///
/// Chat posts several event types to the same URL, and a bot's own replies come
/// back through it too — a bot that answered its own message would loop, so
/// `sender.type == "BOT"` is dropped. `argumentText` is not used: it is the
/// text with mentions stripped, and stripping a mention the agent needs to see
/// is worse than leaving it in.
pub fn parse_event(body: &[u8]) -> crate::Result<Option<GoogleChatIncoming>> {
    #[derive(Deserialize)]
    struct Event {
        #[serde(rename = "type")]
        kind: Option<String>,
        message: Option<Message>,
        space: Option<Space>,
    }

    #[derive(Deserialize)]
    struct Message {
        #[serde(default)]
        sender: Option<Sender>,
        #[serde(default)]
        text: Option<String>,
    }

    #[derive(Deserialize)]
    struct Sender {
        #[serde(default)]
        name: Option<String>,
        #[serde(rename = "type", default)]
        kind: Option<String>,
    }

    #[derive(Deserialize)]
    struct Space {
        #[serde(default)]
        name: Option<String>,
    }

    let event: Event = serde_json::from_slice(body)
        .map_err(|e| SyscityError::Validation(format!("Invalid Google Chat payload: {}", e)))?;

    if event.kind.as_deref() != Some("MESSAGE") {
        return Ok(None);
    }

    let Some(message) = event.message else {
        return Ok(None);
    };

    // A bot's own reply arrives through the same webhook. Answering it would
    // loop, so it is dropped before anything else looks at it.
    let sender = message.sender.unwrap_or(Sender { name: None, kind: None });
    if sender.kind.as_deref() == Some("BOT") {
        return Ok(None);
    }

    let Some(text) = message.text else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(None);
    }

    let Some(user_id) = sender.name.filter(|n| !n.is_empty()) else {
        return Ok(None);
    };
    let Some(space) = event.space.and_then(|s| s.name).filter(|n| !n.is_empty()) else {
        return Ok(None);
    };

    Ok(Some(GoogleChatIncoming { user_id, space, text }))
}

/// Google Chat channel implementation.
pub struct GoogleChatChannel {
    config: GoogleChatConfig,
    tokens: TokenSource,
    http_client: reqwest::Client,
    running: Arc<AtomicBool>,
}

impl std::fmt::Debug for GoogleChatChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The service-account key is deliberately not printed.
        f.debug_struct("GoogleChatChannel")
            .field("client_email", &self.config.client_email)
            .field("audience", &self.config.audience)
            .field("running", &self.running)
            .finish()
    }
}

impl GoogleChatChannel {
    /// Create a new Google Chat channel.
    pub fn new(config: GoogleChatConfig) -> Self {
        let tokens = TokenSource::new(
            GOOGLE_TOKEN_URL,
            Grant::JwtBearer {
                client_email: config.client_email.clone(),
                private_key_pem: config.private_key_pem.clone(),
                scope: GOOGLE_CHAT_SCOPE.to_string(),
            },
        );

        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            config,
            tokens,
            http_client,
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Post one message into a space.
    async fn post_message(&self, space: &str, text: &str) -> crate::Result<()> {
        let token = self.tokens.token().await?;

        let response = self
            .http_client
            .post(format!("{}/{}/messages", GOOGLE_CHAT_API_BASE, space))
            .bearer_auth(token)
            .json(&serde_json::json!({ "text": text }))
            .send()
            .await
            .map_err(|e| SyscityError::ExternalService {
                source: format!("Google Chat send failed: {}", e),
                cause: None,
            })?;

        if !response.status().is_success() {
            let status = response.status();
            // Google's error body names the cause (the bot is not in the space,
            // the space does not exist, the token lacks the scope).
            let detail = response.text().await.unwrap_or_default();
            return Err(SyscityError::ExternalService {
                source: format!("Google Chat send returned {}: {}", status, detail),
                cause: None,
            });
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for GoogleChatChannel {
    fn name(&self) -> &str {
        "googlechat"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct, ChatType::Group],
            // Chat renders a small markdown subset; the plain-text rendering is
            // what the agent's markdown reduces to safely.
            supports_formatting: false,
            supports_attachments: false,
            supports_images: false,
            supports_threads: true,
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
        info!("Starting Google Chat channel...");
        self.running.store(true, Ordering::SeqCst);
        info!(
            "Google Chat channel started (inbound via /webhooks/googlechat, \
             verifying issuer {})",
            self.config.issuer
        );

        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping Google Chat channel...");
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        // The conversation id is the *space*, not the sender: the shared
        // inbound tail is told both, and stores the space as the reply address.
        let space = &message.conversation_id.0;

        let text = match &message.formatted_content {
            Some(crate::channels::FormattedContent::Markdown(md)) => {
                PlainTextFormatter::strip_markdown(md)
            }
            _ => PlainTextFormatter::strip_markdown(&message.content),
        };

        // Chat caps a message at 4096 characters, so a longer answer arrives as
        // consecutive messages rather than being refused outright.
        for segment in split_message(&text, MAX_GOOGLE_CHAT_CHARS) {
            self.post_message(space, &segment).await?;
        }

        debug!("Google Chat message posted to {}", space);
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // The Chat API has no typing indicator.
        Ok(())
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        Err(SyscityError::Unsupported(
            "Google Chat messages cannot be edited from here".into(),
        ))
    }

    async fn delete_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(SyscityError::Unsupported(
            "Google Chat messages cannot be deleted from here".into(),
        ))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> GoogleChatConfig {
        GoogleChatConfig {
            issuer: "chat@system.gserviceaccount.com".to_string(),
            audience: "https://gateway.example/webhooks/googlechat".to_string(),
            jwks_url: "https://www.googleapis.com/example/jwk".to_string(),
            allowed_service_accounts: vec!["chat@system.gserviceaccount.com".to_string()],
            client_email: "bot@project.iam.gserviceaccount.com".to_string(),
            private_key_pem: "not used in these tests".to_string(),
        }
    }

    fn event(sender_type: &str, text: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "type": "MESSAGE",
            "message": {
                "name": "spaces/S/messages/M",
                "sender": { "name": "users/123", "type": sender_type },
                "text": text,
            },
            "space": { "name": "spaces/S", "spaceType": "DIRECT_MESSAGE" },
        }))
        .expect("serialize")
    }

    #[test]
    fn a_user_message_parses_with_both_the_sender_and_the_space() {
        let parsed = parse_event(&event("HUMAN", "hello there"))
            .expect("payload parses")
            .expect("it is a message");

        assert_eq!(
            parsed,
            GoogleChatIncoming {
                user_id: "users/123".to_string(),
                space: "spaces/S".to_string(),
                text: "hello there".to_string(),
            }
        );
    }

    /// A bot's own reply comes back through the same webhook. Answering it
    /// would make the bot talk to itself forever.
    #[test]
    fn a_bot_message_is_dropped() {
        assert_eq!(parse_event(&event("BOT", "my own reply")).expect("parses"), None);
    }

    #[test]
    fn everything_that_is_not_a_user_message_is_dropped() {
        // A different event type on the same URL.
        let added = serde_json::to_vec(&serde_json::json!({
            "type": "ADDED_TO_SPACE",
            "space": { "name": "spaces/S" },
        }))
        .expect("serialize");
        assert_eq!(parse_event(&added).expect("parses"), None);

        // Empty text, and a message with no sender.
        assert_eq!(parse_event(&event("HUMAN", "   ")).expect("parses"), None);

        let no_sender = serde_json::to_vec(&serde_json::json!({
            "type": "MESSAGE",
            "message": { "text": "hi" },
            "space": { "name": "spaces/S" },
        }))
        .expect("serialize");
        assert_eq!(parse_event(&no_sender).expect("parses"), None);

        // A message with no space cannot be replied to.
        let no_space = serde_json::to_vec(&serde_json::json!({
            "type": "MESSAGE",
            "message": { "sender": { "name": "users/1", "type": "HUMAN" }, "text": "hi" },
        }))
        .expect("serialize");
        assert_eq!(parse_event(&no_space).expect("parses"), None);
    }

    #[test]
    fn a_payload_that_is_not_json_is_an_error() {
        assert!(parse_event(b"not json").is_err());
    }

    /// The signature only proves Google issued the token. Which identity it was
    /// issued *to* is what decides whether this app should act on it.
    #[test]
    fn the_identity_check_is_a_closed_allowlist() {
        let config = config();

        assert!(config.identity_is_allowed("chat@system.gserviceaccount.com"));
        assert!(
            config.identity_is_allowed("  CHAT@System.GServiceAccount.COM "),
            "the comparison ignores case and surrounding space"
        );

        assert!(!config.identity_is_allowed("someone-else@example.com"));
        assert!(!config.identity_is_allowed(""));

        let empty = GoogleChatConfig {
            allowed_service_accounts: vec![],
            ..config
        };
        assert!(
            !empty.identity_is_allowed("chat@system.gserviceaccount.com"),
            "an empty allowlist allows nobody"
        );
    }
}
