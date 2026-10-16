//! Microsoft Teams channel.
//!
//! Inbound arrives as a Bot Framework *activity* posted to a public HTTPS
//! webhook (the route and the shared inbound tail live in `gateway/webhooks.rs`);
//! this module owns the outbound half — posting activities back through the Bot
//! Framework connector.
//!
//! Four things make this channel unlike the signed ones:
//!
//! - **The delivery is authenticated by a JWT**, verified against the issuer's
//!   published keys (`gateway::jwt_verify`). Past that, the token's `serviceurl`
//!   claim has to match the activity's `serviceUrl` field: Microsoft signs one
//!   token per bot, and the claim is what binds it to the conversation surface
//!   the activity came from. The two spellings differ, and both Bot Framework
//!   SDKs look up the claim's.
//! - **The reply address is two values, not one.** A reply goes to
//!   `{service_url}v3/conversations/{conversation_id}/activities`, and the
//!   service URL is per-tenant rather than a constant, so both travel together
//!   in the conversation id — the one string [`Channel::send`] receives.
//! - **The outbound token is minted, not configured**, by a client-credentials
//!   grant against Azure AD (`channels::oauth_token`).
//! - **The service URL arrives in the request and is then handed a freshly
//!   minted bearer token**, so it is held to an allowlist of Bot Framework hosts
//!   before any token is attached. The claim check above is what authenticates
//!   it; the allowlist is the second layer, and it is what makes a tampered
//!   activity a failed reply rather than a way to post this bot's credentials to
//!   somebody else's host.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::channels::oauth_token::{Grant, TokenSource};
use crate::channels::{
    split_message, Channel, ChannelCapabilities, ChatType, ConversationId, OutgoingMessage,
    PlainTextFormatter,
};
use crate::core::models::Id;
use crate::error::SyscityError;

/// Where Azure AD mints the outbound token, per tenant.
const AZURE_AD_TOKEN_URL: &str = "https://login.microsoftonline.com";

/// The scope a Bot Framework connector call is authorised with.
const BOT_FRAMEWORK_SCOPE: &str = "https://api.botframework.com/.default";

/// The most characters Teams accepts in one message.
pub const MAX_TEAMS_CHARS: usize = 28_000;

/// The longest conversation id that will be put in a connector URL.
///
/// Teams' own ids are a fraction of this; the bound is here so a hostile payload
/// cannot make the gateway build an absurd URL.
const MAX_CONVERSATION_ID_LEN: usize = 512;

/// The hosts a reply may be sent to — and therefore the only hosts that may ever
/// be handed a freshly minted Bot Framework token.
///
/// Matched exactly rather than by suffix, and deliberately not configurable: any
/// Azure customer can register a `*.trafficmanager.net` name, so a suffix rule
/// would let a tampered activity point this bot's credentials at a host somebody
/// else controls, and a configurable list would put that one typo away. The
/// second entry is the government cloud; the commercial regions differ by
/// *path* (`/amer/`, `/emea/`, `/apac/`) on the same host, which is why the path
/// travels per activity rather than being pinned here.
const ALLOWED_SERVICE_HOSTS: &[&str] = &[
    "smba.trafficmanager.net",
    "smba.infra.gov.teams.microsoft.us",
];

/// The separator between the two halves of a reply address.
///
/// Neither half can contain one — the service URL is an `https` URL and the
/// conversation id is held to the charset in [`is_valid_conversation_id`] — so
/// the split needs no escaping.
const ADDRESS_SEPARATOR: char = '|';

/// Where a Teams reply goes.
///
/// Two values, because that is what the connector takes: the service URL of the
/// tenant the activity came from, and the conversation inside it.
/// [`Channel::send`] is handed one string, so they travel joined by
/// [`ADDRESS_SEPARATOR`] — the same way the reply is addressed to a *space* for
/// Google Chat, and for the same reason: the value is a destination, not an
/// identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamsAddress {
    /// The tenant's connector host, with a trailing slash.
    pub service_url: String,
    /// The conversation to post into.
    pub conversation_id: String,
}

impl TeamsAddress {
    /// The address as it travels in a conversation id.
    pub fn encode(&self) -> String {
        format!("{}{}{}", self.service_url, ADDRESS_SEPARATOR, self.conversation_id)
    }

    /// Read an address back, or `None` when the value is not one.
    pub fn parse(raw: &str) -> Option<Self> {
        let (service_url, conversation_id) = raw.split_once(ADDRESS_SEPARATOR)?;
        if service_url.is_empty() || conversation_id.is_empty() {
            return None;
        }
        Some(Self {
            service_url: service_url.to_string(),
            conversation_id: conversation_id.to_string(),
        })
    }
}

/// A Bot Framework service URL, rebuilt in a canonical form, or `None` when it
/// is not one this channel will send a token to.
///
/// Canonicalizing matters because the connector path is appended to it *and*
/// because the activity's `serviceUrl` is compared against the token's claim:
/// rebuilding from the parsed URL is what lets a tenant that spells it with and
/// without the trailing slash — or with the host in different case, which is the
/// same host — still match.
pub fn validate_service_url(raw: &str) -> Option<String> {
    let parsed = url::Url::parse(raw).ok()?;
    if parsed.scheme() != "https" {
        return None;
    }

    let host = parsed.host_str()?;
    if !ALLOWED_SERVICE_HOSTS
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(host))
    {
        return None;
    }

    // A connector URL is scheme, host and a tenant routing path, and nothing
    // else. A query, a fragment or embedded credentials are refused rather than
    // trimmed, so nothing unexpected survives into the URL the token is sent to.
    if parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }

    // Rebuilt rather than trimmed, so what is returned is only ever the parts
    // that were just checked. The host is case-folded because hosts are
    // case-insensitive by definition; the path is not, because paths are not.
    // The port is not checked: it cannot move the request off the host that was
    // just allowlisted, and the host is the whole of the exfiltration guard.
    let mut normalized = format!("https://{}", host.to_ascii_lowercase());
    if let Some(port) = parsed.port() {
        normalized.push_str(&format!(":{}", port));
    }
    normalized.push_str(parsed.path());
    if !normalized.ends_with('/') {
        normalized.push('/');
    }

    Some(normalized)
}

/// Whether `id` is safe to append to the connector path.
///
/// Conservative on purpose: the id goes into
/// `/v3/conversations/<id>/activities`, so a value carrying `/` would escape it,
/// and one that is exactly `..` would let the URL parser walk up a level into a
/// different connector endpoint on the same host. Teams' own ids
/// (`19:…@thread.tacv2`, `a:…`) sit well inside this set.
fn is_valid_conversation_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_CONVERSATION_ID_LEN
        && !id.contains("..")
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '@' | '-' | '_' | '.'))
}

/// Remove the `<at>BotName</at>` markup Teams wraps a mention in.
///
/// Only the tags go; the name between them stays. That is the one place this
/// departs from the reference implementation, which drops the whole tag and with
/// it the name — and a channel that silently deletes "Alice" out of
/// "`<at>Alice</at>` can you check this" is losing something the agent needs,
/// which is the same reason Google Chat keeps its mentions intact.
fn strip_mentions(text: &str) -> String {
    text.replace("<at>", "")
        .replace("</at>", "")
        .trim()
        .to_string()
}

/// A message extracted from a Bot Framework activity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamsIncoming {
    /// The sender, preferring their Azure AD object id when Teams sends one —
    /// it is stable across the tenant, where `29:…` is per-conversation.
    pub user_id: String,
    /// Where a reply goes.
    pub address: TeamsAddress,
    /// The message text, with Teams' mention markup removed.
    pub text: String,
}

/// Extract a user message from a Bot Framework activity, or `None` when it is
/// not one.
///
/// Teams posts conversation updates, reactions, invokes and typing to the same
/// URL, and the bot's own activity would come back around if it were routed —
/// answering it would make the bot talk to itself. Everything that is not a
/// message from somebody else is dropped here.
pub fn parse_activity(body: &[u8], app_id: &str) -> crate::Result<Option<TeamsIncoming>> {
    #[derive(Deserialize, Default)]
    struct Account {
        #[serde(default)]
        id: Option<String>,
        #[serde(rename = "aadObjectId", default)]
        aad_object_id: Option<String>,
    }

    #[derive(Deserialize)]
    struct Conversation {
        #[serde(default)]
        id: Option<String>,
    }

    #[derive(Deserialize)]
    struct Activity {
        #[serde(rename = "type", default)]
        kind: Option<String>,
        #[serde(default)]
        text: Option<String>,
        #[serde(rename = "serviceUrl", default)]
        service_url: Option<String>,
        #[serde(default)]
        from: Option<Account>,
        #[serde(default)]
        recipient: Option<Account>,
        #[serde(default)]
        conversation: Option<Conversation>,
    }

    let activity: Activity = serde_json::from_slice(body)
        .map_err(|e| SyscityError::Validation(format!("Invalid Teams activity: {}", e)))?;

    if activity.kind.as_deref() != Some("message") {
        return Ok(None);
    }

    let from = activity.from.unwrap_or_default();
    let from_id = from.id.filter(|id| !id.is_empty()).unwrap_or_default();
    if from_id.is_empty() {
        return Ok(None);
    }

    // Three spellings of "us": the app registration's bare id, the `28:<app id>`
    // Teams writes as the bot's conversation identity, and the activity's own
    // recipient — an activity the bot sent has `from == recipient`.
    let recipient_id = activity
        .recipient
        .and_then(|r| r.id)
        .filter(|id| !id.is_empty())
        .unwrap_or_default();
    let is_the_bot = from_id == app_id
        || (!app_id.is_empty() && from_id == format!("28:{}", app_id))
        || (!recipient_id.is_empty() && from_id == recipient_id);
    if is_the_bot {
        return Ok(None);
    }

    let Some(text) = activity.text.map(|t| strip_mentions(&t)) else {
        return Ok(None);
    };
    if text.is_empty() {
        return Ok(None);
    }

    let Some(conversation_id) = activity.conversation.and_then(|c| c.id) else {
        return Ok(None);
    };
    if !is_valid_conversation_id(&conversation_id) {
        warn!("Teams activity: conversation id is outside the Bot Framework charset — not routed");
        return Ok(None);
    }

    // A delivery we cannot reply to is not a turn: without a service URL there
    // is nowhere to send the answer, and one that is not a Bot Framework host is
    // somewhere we will not send a token.
    let Some(service_url) = activity
        .service_url
        .as_deref()
        .and_then(validate_service_url)
    else {
        warn!("Teams activity: serviceUrl is missing or not a Bot Framework host — not routed");
        return Ok(None);
    };

    let user_id = from
        .aad_object_id
        .filter(|id| !id.is_empty())
        .unwrap_or(from_id);

    Ok(Some(TeamsIncoming {
        user_id,
        address: TeamsAddress { service_url, conversation_id },
        text,
    }))
}

/// Microsoft Teams channel configuration.
#[derive(Debug, Clone)]
pub struct TeamsConfig {
    /// The issuer the inbound token must name.
    ///
    /// `https://api.botframework.com` in the public cloud; the sovereign clouds
    /// publish under their own, which is why this is configuration rather than a
    /// constant.
    pub issuer: String,
    /// The bot's Microsoft App ID.
    ///
    /// One value doing two jobs, which is what it is: the audience the inbound
    /// token must name, and the app registration replies are sent as. Splitting
    /// it into an `audience` and a `client_id` would be two names for one GUID,
    /// and the failure mode of setting only one of them is a channel that half
    /// works.
    pub app_id: String,
    /// Where the issuer publishes its signing keys.
    pub jwks_url: String,
    /// The Azure AD tenant replies are minted in.
    pub tenant_id: String,
    /// The app registration's secret.
    pub client_secret: String,
}

/// Microsoft Teams channel implementation.
pub struct TeamsChannel {
    config: TeamsConfig,
    tokens: TokenSource,
    http_client: reqwest::Client,
    running: Arc<AtomicBool>,
}

impl std::fmt::Debug for TeamsChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The app registration's secret is deliberately not printed.
        f.debug_struct("TeamsChannel")
            .field("app_id", &self.config.app_id)
            .field("tenant_id", &self.config.tenant_id)
            .field("running", &self.running)
            .finish()
    }
}

impl TeamsChannel {
    /// Create a new Microsoft Teams channel.
    pub fn new(config: TeamsConfig) -> Self {
        // The tenant goes into the token URL's path. The host is fixed, so a
        // hostile value cannot send the request somewhere else — but it could
        // still end the path early and produce a URL that is not the token
        // endpoint at all, and an Azure AD tenant is a GUID or a verified
        // domain, so anything outside that charset is dropped.
        let tenant_id: String = config
            .tenant_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
            .collect();

        let tokens = TokenSource::new(
            format!("{}/{}/oauth2/v2.0/token", AZURE_AD_TOKEN_URL, tenant_id),
            Grant::ClientCredentials {
                client_id: config.app_id.clone(),
                client_secret: config.client_secret.clone(),
                scope: BOT_FRAMEWORK_SCOPE.to_string(),
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

    /// Post one activity into a conversation.
    async fn post_activity(
        &self,
        service_url: &str,
        conversation_id: &str,
        activity: serde_json::Value,
    ) -> crate::Result<()> {
        let token = self.tokens.token().await?;

        let response = self
            .http_client
            .post(format!("{}v3/conversations/{}/activities", service_url, conversation_id))
            .bearer_auth(token)
            .json(&activity)
            .send()
            .await
            .map_err(|e| SyscityError::ExternalService {
                source: format!("Teams send failed: {}", e),
                cause: None,
            })?;

        if !response.status().is_success() {
            let status = response.status();
            // The connector's error body names the cause (the bot is not
            // installed in the conversation, the token is for another tenant).
            let detail = response.text().await.unwrap_or_default();
            return Err(SyscityError::ExternalService {
                source: format!("Teams send returned {}: {}", status, detail),
                cause: None,
            });
        }

        Ok(())
    }

    /// Split a reply address into the pair the connector takes, refusing
    /// anything that is not one this channel would have produced.
    ///
    /// Both halves are re-checked on the way out rather than trusted from the
    /// way in: this is the point where a minted token is attached and the point
    /// where the values become a URL, so it is the point that must not be able
    /// to send the token anywhere else or address anything but a conversation.
    fn resolve(&self, conversation_id: &ConversationId) -> crate::Result<(String, String)> {
        let address = TeamsAddress::parse(&conversation_id.0).ok_or_else(|| {
            SyscityError::Validation(format!("not a Teams reply address: {}", conversation_id.0))
        })?;

        let Some(service_url) = validate_service_url(&address.service_url) else {
            return Err(SyscityError::Validation(format!(
                "refusing to send a Bot Framework token to {}",
                address.service_url
            )));
        };

        if !is_valid_conversation_id(&address.conversation_id) {
            return Err(SyscityError::Validation(format!(
                "not a Bot Framework conversation id: {}",
                address.conversation_id
            )));
        }

        Ok((service_url, address.conversation_id))
    }
}

#[async_trait]
impl Channel for TeamsChannel {
    fn name(&self) -> &str {
        "teams"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct, ChatType::Group],
            // Teams renders a subset of markdown that excludes headings and
            // tables, so markdown sent literally would half-render; the
            // plain-text reduction is what the agent's markdown becomes safely.
            supports_formatting: false,
            supports_attachments: false,
            supports_images: false,
            supports_threads: true,
            supports_typing: true,
            supports_buttons: false,
            supports_commands: true,
            supports_reactions: false,
            supports_edit: false,
            supports_unsend: false,
            supports_effects: false,
        }
    }

    async fn start(&self) -> crate::Result<()> {
        info!("Starting Microsoft Teams channel...");
        self.running.store(true, Ordering::SeqCst);
        info!(
            "Microsoft Teams channel started (inbound via /webhooks/teams, \
             verifying issuer {})",
            self.config.issuer
        );

        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping Microsoft Teams channel...");
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let (service_url, conversation_id) = self.resolve(&message.conversation_id)?;

        let text = match &message.formatted_content {
            Some(crate::channels::FormattedContent::Markdown(md)) => {
                PlainTextFormatter::strip_markdown(md)
            }
            _ => PlainTextFormatter::strip_markdown(&message.content),
        };

        // Teams caps a message at ~28 KB, so a longer answer arrives as
        // consecutive messages rather than being refused outright.
        for segment in split_message(&text, MAX_TEAMS_CHARS) {
            self.post_activity(
                &service_url,
                &conversation_id,
                serde_json::json!({ "type": "message", "text": segment }),
            )
            .await?;
        }

        debug!("Teams message posted to {}", conversation_id);
        Ok(Id::new())
    }

    async fn send_typing(&self, conversation_id: &ConversationId) -> crate::Result<()> {
        let (service_url, conversation_id) = self.resolve(conversation_id)?;
        self.post_activity(&service_url, &conversation_id, serde_json::json!({ "type": "typing" }))
            .await
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        Err(SyscityError::Unsupported("Teams messages cannot be edited from here".into()))
    }

    async fn delete_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(SyscityError::Unsupported("Teams messages cannot be deleted from here".into()))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP_ID: &str = "11111111-2222-3333-4444-555555555555";
    const SERVICE_URL: &str = "https://smba.trafficmanager.net/amer/";

    fn activity(kind: &str, from: &str, recipient: &str, text: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "type": kind,
            "id": "ACT_HTTP_E2E",
            "text": text,
            "serviceUrl": SERVICE_URL,
            "channelId": "msteams",
            "from": { "id": from, "aadObjectId": "aad-object-id" },
            "recipient": { "id": recipient },
            "conversation": { "id": "19:conv@thread.tacv2", "conversationType": "channel" },
        }))
        .expect("serialize")
    }

    #[test]
    fn a_user_message_parses_with_its_sender_and_reply_address() {
        let parsed =
            parse_activity(&activity("message", "29:user", "28:bot", "hello there"), APP_ID)
                .expect("payload parses")
                .expect("it is a message");

        assert_eq!(parsed.user_id, "aad-object-id", "the stable id is preferred");
        assert_eq!(parsed.text, "hello there");
        assert_eq!(
            parsed.address,
            TeamsAddress {
                service_url: SERVICE_URL.to_string(),
                conversation_id: "19:conv@thread.tacv2".to_string(),
            }
        );
        assert_eq!(parsed.address.encode(), format!("{}|19:conv@thread.tacv2", SERVICE_URL));
    }

    /// A sender Teams sends without an Azure AD object id still has one usable
    /// identity, and a reply address that is not the sender's.
    #[test]
    fn a_sender_without_an_object_id_falls_back_to_their_activity_id() {
        let body = serde_json::to_vec(&serde_json::json!({
            "type": "message",
            "text": "hi",
            "serviceUrl": SERVICE_URL,
            "from": { "id": "29:user" },
            "recipient": { "id": "28:bot" },
            "conversation": { "id": "a:1abc" },
        }))
        .expect("serialize");

        let parsed = parse_activity(&body, APP_ID)
            .expect("parses")
            .expect("it is a message");
        assert_eq!(parsed.user_id, "29:user");
    }

    /// The bot's own activity coming back around would make it answer itself
    /// forever. Teams spells the bot's identity three ways and all of them count.
    #[test]
    fn the_bots_own_activity_is_dropped() {
        let prefixed = format!("28:{}", APP_ID);
        for from in [APP_ID, prefixed.as_str()] {
            assert_eq!(
                parse_activity(&activity("message", from, "28:bot", "my own reply"), APP_ID)
                    .expect("parses"),
                None,
                "{from} is this bot"
            );
        }

        // An activity the bot sent to itself has `from == recipient`.
        assert_eq!(
            parse_activity(&activity("message", "28:bot", "28:bot", "talking to myself"), APP_ID)
                .expect("parses"),
            None
        );
    }

    /// Teams' mention markup is the platform's, not the user's — but the name
    /// inside it is not, so only the tags are removed.
    #[test]
    fn mention_markup_is_stripped_and_the_name_kept() {
        let parsed = parse_activity(
            &activity("message", "29:user", "28:bot", "<at>Alice</at> can you check this?"),
            APP_ID,
        )
        .expect("parses")
        .expect("it is a message");

        assert_eq!(parsed.text, "Alice can you check this?");
    }

    #[test]
    fn everything_that_is_not_a_user_message_is_dropped() {
        // The other activity types Teams posts to the same URL.
        for kind in ["conversationUpdate", "messageReaction", "invoke", "typing"] {
            assert_eq!(
                parse_activity(&activity(kind, "29:user", "28:bot", "hi"), APP_ID).expect("parses"),
                None,
                "{kind} is not a turn"
            );
        }

        // Empty text.
        assert_eq!(
            parse_activity(&activity("message", "29:user", "28:bot", "   "), APP_ID)
                .expect("parses"),
            None
        );

        // A mention of the bot with nothing after it is not empty: only the tags
        // go, so what is left is the name. That is the deliberate difference
        // from the reference implementation, which deletes the whole tag — and
        // with it the "Alice" out of "<at>Alice</at> can you check this".
        assert_eq!(
            parse_activity(&activity("message", "29:user", "28:bot", "<at>Bot</at>"), APP_ID)
                .expect("parses")
                .expect("a mention is still a message")
                .text,
            "Bot"
        );

        // No sender, and no conversation to reply into.
        let no_from = serde_json::to_vec(&serde_json::json!({
            "type": "message", "text": "hi", "serviceUrl": SERVICE_URL,
            "conversation": { "id": "a:1abc" },
        }))
        .expect("serialize");
        assert_eq!(parse_activity(&no_from, APP_ID).expect("parses"), None);

        let no_conversation = serde_json::to_vec(&serde_json::json!({
            "type": "message", "text": "hi", "serviceUrl": SERVICE_URL,
            "from": { "id": "29:user" },
        }))
        .expect("serialize");
        assert_eq!(parse_activity(&no_conversation, APP_ID).expect("parses"), None);
    }

    /// A delivery we cannot reply to is not a turn. Without a service URL there
    /// is nowhere to send the answer, and one that is not a Bot Framework host
    /// is somewhere a token must never go.
    #[test]
    fn an_activity_with_no_usable_service_url_is_dropped() {
        for service_url in [
            serde_json::Value::Null,
            serde_json::json!(""),
            serde_json::json!("http://smba.trafficmanager.net/amer/"),
            serde_json::json!("https://smba.trafficmanager.net.evil.example/"),
            serde_json::json!("https://somebody-elses-trafficmanager.net/"),
        ] {
            let body = serde_json::to_vec(&serde_json::json!({
                "type": "message", "text": "hi", "serviceUrl": service_url,
                "from": { "id": "29:user" },
                "recipient": { "id": "28:bot" },
                "conversation": { "id": "a:1abc" },
            }))
            .expect("serialize");

            assert_eq!(
                parse_activity(&body, APP_ID).expect("parses"),
                None,
                "{service_url} is not a host a token may be sent to"
            );
        }
    }

    /// The id is appended to the connector path, so anything that could escape
    /// it is refused rather than encoded.
    #[test]
    fn a_conversation_id_that_could_escape_the_connector_path_is_dropped() {
        for conversation_id in [
            "19:conv/../../other",
            "..",
            "a:1abc?x=1",
            "a:1abc#fragment",
            "",
        ] {
            let body = serde_json::to_vec(&serde_json::json!({
                "type": "message", "text": "hi", "serviceUrl": SERVICE_URL,
                "from": { "id": "29:user" },
                "recipient": { "id": "28:bot" },
                "conversation": { "id": conversation_id },
            }))
            .expect("serialize");

            assert_eq!(
                parse_activity(&body, APP_ID).expect("parses"),
                None,
                "{conversation_id:?} is not a conversation id"
            );
        }

        // The ids Teams actually sends are all inside the charset.
        for conversation_id in [
            "19:abc@thread.tacv2",
            "a:1a2b3c",
            "19:meeting_xyz@thread.v2",
        ] {
            assert!(
                is_valid_conversation_id(conversation_id),
                "{conversation_id} is a real Teams conversation id"
            );
        }
    }

    #[test]
    fn a_payload_that_is_not_json_is_an_error() {
        assert!(parse_activity(b"not json", APP_ID).is_err());
    }

    /// The service URL is canonicalized so the activity's spelling and the
    /// token's claim can be compared directly.
    #[test]
    fn a_service_url_is_canonicalized_and_held_to_the_allowlist() {
        assert_eq!(
            validate_service_url("https://smba.trafficmanager.net/amer").as_deref(),
            Some("https://smba.trafficmanager.net/amer/")
        );
        assert_eq!(
            validate_service_url("https://smba.trafficmanager.net/amer/").as_deref(),
            Some("https://smba.trafficmanager.net/amer/")
        );
        assert_eq!(
            validate_service_url("https://SMBA.TrafficManager.NET/teams/").as_deref(),
            Some("https://smba.trafficmanager.net/teams/"),
            "the host is case-folded, so the two spellings compare equal"
        );
        assert_eq!(
            validate_service_url("https://smba.infra.gov.teams.microsoft.us/").as_deref(),
            Some("https://smba.infra.gov.teams.microsoft.us/")
        );

        // A suffix rule would accept every one of these; exact matching does not.
        for refused in [
            "https://evil.example/",
            "https://smba.trafficmanager.net.evil.example/",
            "https://evil-smba.trafficmanager.net/",
            "http://smba.trafficmanager.net/",
            "https://smba.trafficmanager.net/amer/?redirect=elsewhere",
            "https://user:pass@smba.trafficmanager.net/",
            "not a url",
            "",
        ] {
            assert_eq!(validate_service_url(refused), None, "{refused} must be refused");
        }
    }

    /// The reply address survives the trip out through the conversation id, and
    /// a value that is not one is refused rather than guessed at.
    #[test]
    fn a_reply_address_round_trips_and_anything_else_is_refused() {
        let address = TeamsAddress {
            service_url: SERVICE_URL.to_string(),
            conversation_id: "19:conv@thread.tacv2".to_string(),
        };

        assert_eq!(TeamsAddress::parse(&address.encode()), Some(address));

        for not_an_address in [
            "",
            "19:conv@thread.tacv2",
            "|",
            "https://smba.trafficmanager.net/|",
        ] {
            assert_eq!(TeamsAddress::parse(not_an_address), None, "{not_an_address:?}");
        }
    }

    /// This is the point a minted token is attached, so it is the point that has
    /// to refuse a destination the inbound side would never have produced.
    #[test]
    fn sending_refuses_a_reply_address_that_is_not_a_bot_framework_host() {
        let channel = TeamsChannel::new(TeamsConfig {
            issuer: "https://api.botframework.com".to_string(),
            app_id: APP_ID.to_string(),
            jwks_url: "https://login.botframework.com/v1/.well-known/keys".to_string(),
            tenant_id: "a-tenant".to_string(),
            client_secret: "not used in these tests".to_string(),
        });

        for refused in [
            "https://evil.example/|19:conv",
            "https://smba.trafficmanager.net.evil.example/|19:conv",
            // A host that passes, but an id that would escape the connector path.
            "https://smba.trafficmanager.net/amer/|19:conv/../../other",
            "https://smba.trafficmanager.net/amer/|..",
            "19:conv",
            "",
        ] {
            assert!(
                channel
                    .resolve(&ConversationId(refused.to_string()))
                    .is_err(),
                "{refused:?} must not be sent to"
            );
        }

        let (service_url, conversation_id) = channel
            .resolve(&ConversationId("https://smba.trafficmanager.net/amer|19:conv".to_string()))
            .expect("a connector address resolves");
        assert_eq!(service_url, SERVICE_URL);
        assert_eq!(conversation_id, "19:conv");
    }

    /// The tenant goes into the token URL's path, so it cannot carry one.
    #[test]
    fn a_tenant_id_cannot_break_the_token_url() {
        let channel = TeamsChannel::new(TeamsConfig {
            issuer: "https://api.botframework.com".to_string(),
            app_id: APP_ID.to_string(),
            jwks_url: "https://login.botframework.com/v1/.well-known/keys".to_string(),
            tenant_id: "../../../evil.example/?x=1#f".to_string(),
            client_secret: "not used in these tests".to_string(),
        });

        let url = channel.tokens.token_url();
        assert!(url.starts_with("https://login.microsoftonline.com/"), "{url}");
        assert!(url.ends_with("/oauth2/v2.0/token"), "{url}");

        // The tenant is one path segment and nothing else, so it cannot end the
        // path early or add a query of its own.
        let segment = url
            .trim_start_matches("https://login.microsoftonline.com/")
            .trim_end_matches("/oauth2/v2.0/token");
        assert!(!segment.is_empty() && !segment.contains('/'), "{segment}");

        // A real tenant — a GUID, or a verified domain — is passed through as it
        // was configured.
        let clean = TeamsChannel::new(TeamsConfig {
            issuer: "https://api.botframework.com".to_string(),
            app_id: APP_ID.to_string(),
            jwks_url: "https://login.botframework.com/v1/.well-known/keys".to_string(),
            tenant_id: "contoso.onmicrosoft.com".to_string(),
            client_secret: "not used in these tests".to_string(),
        });
        assert_eq!(
            clean.tokens.token_url(),
            "https://login.microsoftonline.com/contoso.onmicrosoft.com/oauth2/v2.0/token"
        );
    }

    /// The channel's own `Debug` must not carry the app registration's secret.
    #[test]
    fn the_debug_rendering_does_not_leak_the_secret() {
        let channel = TeamsChannel::new(TeamsConfig {
            issuer: "https://api.botframework.com".to_string(),
            app_id: APP_ID.to_string(),
            jwks_url: "https://login.botframework.com/v1/.well-known/keys".to_string(),
            tenant_id: "a-tenant".to_string(),
            client_secret: "super-secret-value".to_string(),
        });

        let rendered = format!("{:?}", channel);
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
    }
}
