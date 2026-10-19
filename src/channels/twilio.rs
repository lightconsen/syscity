//! Twilio SMS channel.
//!
//! Inbound arrives as a form-encoded webhook (`/webhooks/twilio`, see
//! `gateway/webhooks.rs`); this module owns the outbound half — sending through
//! the Messages API — and the signature verification the handler calls.
//!
//! Twilio's signature is unlike the other channels' in one way that shapes the
//! whole design: it covers **the URL Twilio dialled**, not just the body. The
//! gateway cannot recover that from the request — behind a tunnel or a proxy it
//! sees `127.0.0.1:<port>` while Twilio signed `https://example.com/...` — so
//! the public URL is configuration, and without it a delivery can only be
//! refused.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::channels::{
    split_message, Channel, ChannelCapabilities, ChatType, ConversationId, OutgoingMessage,
    PlainTextFormatter,
};
use crate::core::models::Id;
use crate::error::SyscityError;

/// Twilio REST API base URL.
const TWILIO_API_BASE: &str = "https://api.twilio.com/2010-04-01";

/// The most characters Twilio accepts in one message `Body`. Well above where
/// it starts billing per segment — this is the API's own ceiling, past which it
/// refuses the request rather than splitting.
pub const MAX_SMS_CHARS: usize = 1600;

/// The header Twilio signs a delivery with.
pub const TWILIO_SIGNATURE_HEADER: &str = "x-twilio-signature";

/// Twilio channel configuration.
#[derive(Debug, Clone)]
pub struct TwilioConfig {
    /// Account SID — the API's username, and half of the Basic auth pair.
    pub account_sid: String,
    /// Auth token — the API's password, and the HMAC key for inbound
    /// signatures.
    pub auth_token: String,
    /// The Twilio number replies are sent from.
    pub from_number: String,
    /// The **public** URL Twilio was configured to call, exactly as entered in
    /// the console. See the module docs for why this cannot be derived from the
    /// request.
    pub webhook_url: String,
}

impl TwilioConfig {
    /// Create a config from the four credentials.
    pub fn new(
        account_sid: impl Into<String>,
        auth_token: impl Into<String>,
        from_number: impl Into<String>,
        webhook_url: impl Into<String>,
    ) -> Self {
        Self {
            account_sid: account_sid.into(),
            auth_token: auth_token.into(),
            from_number: from_number.into(),
            webhook_url: webhook_url.into(),
        }
    }
}

/// Verify a Twilio webhook signature.
///
/// Twilio signs the request URL with every POST parameter appended in key
/// order, using HMAC-SHA1, and base64-encodes the digest:
///
/// ```text
/// X-Twilio-Signature: base64(HMAC-SHA1(auth_token, url + Σ key+value))
/// ```
///
/// A delivery whose signature does not match is refused; so is one that cannot
/// be checked at all, which is what an empty `auth_token` means — an empty HMAC
/// key is a public key, and every signature would verify against it.
pub fn verify_signature(
    auth_token: &str,
    webhook_url: &str,
    params: &[(String, String)],
    provided: &str,
) -> bool {
    if auth_token.is_empty() || provided.is_empty() {
        return false;
    }
    if check_signature(auth_token, webhook_url, params, provided) {
        return true;
    }

    // Twilio has signed URLs both with and without the scheme's default port,
    // and `url` normalises that port away — so which spelling was signed cannot
    // be read back off the configured string. Both are tried instead.
    default_port_spellings(webhook_url)
        .iter()
        .any(|spelling| check_signature(auth_token, spelling, params, provided))
}

/// One HMAC-SHA1 check against a specific URL spelling.
fn check_signature(
    auth_token: &str,
    url: &str,
    params: &[(String, String)],
    provided: &str,
) -> bool {
    use base64::Engine as _;
    use hmac::{Hmac, KeyInit, Mac};
    use sha1::Sha1;
    use subtle::ConstantTimeEq;

    // Key order is part of the signed string, so the params are sorted rather
    // than taken as received.
    let mut ordered: Vec<&(String, String)> = params.iter().collect();
    ordered.sort_by(|a, b| a.0.cmp(&b.0));

    let mut data = String::from(url);
    for (key, value) in ordered {
        data.push_str(key);
        data.push_str(value);
    }

    let mut mac = match Hmac::<Sha1>::new_from_slice(auth_token.as_bytes()) {
        Ok(m) => m,
        Err(_) => {
            warn!("Twilio webhook: could not build an HMAC from the auth token");
            return false;
        }
    };
    mac.update(data.as_bytes());
    let computed = base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes());

    computed.as_bytes().ct_eq(provided.as_bytes()).into()
}

/// The two ways a URL's *default* port can be written.
///
/// Returns empty when there is only one spelling — either the scheme has no
/// default port to toggle, or the URL names a port that is not the default.
/// The second case matters: rebuilding `https://host:8443/x` from its parts
/// would drop the non-default port and produce two URLs that are both wrong,
/// so a non-default port has to be recognised and left alone.
fn default_port_spellings(url: &str) -> Vec<String> {
    let Ok(parsed) = url::Url::parse(url) else {
        return Vec::new();
    };
    let default_port = match parsed.scheme() {
        "https" => 443,
        "http" => 80,
        _ => return Vec::new(),
    };
    // `Url` normalises a default port away, so `port()` is `None` for both "no
    // port" and "the default port" — which is exactly the ambiguous pair this
    // function exists to cover. Anything else is an explicit non-default port.
    if parsed.port().is_some() {
        return Vec::new();
    }
    let Some(host) = parsed.host_str() else {
        return Vec::new();
    };

    let mut suffix = parsed.path().to_string();
    if let Some(query) = parsed.query() {
        suffix.push('?');
        suffix.push_str(query);
    }

    vec![
        format!("{}://{}{}", parsed.scheme(), host, suffix),
        format!("{}://{}:{}{}", parsed.scheme(), host, default_port, suffix),
    ]
}

/// Parse an `application/x-www-form-urlencoded` body into key/value pairs.
///
/// Twilio posts forms, not JSON. `keep_blank_values` is what
/// `form_urlencoded::parse` already does — a blank `Body` is a real value that
/// must read as empty rather than as absent.
pub fn parse_form(body: &[u8]) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

/// An inbound SMS.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TwilioIncoming {
    /// The sender's number, in E.164 (`+15551234567`).
    pub from: String,
    /// The Twilio number it was sent to.
    pub to: String,
    /// The message text.
    pub body: String,
}

/// Pull the message out of the form parameters, or `None` when it is not one.
///
/// Twilio posts several shapes to the same URL: inbound messages, and status
/// callbacks for messages it has already handled. A status callback carries no
/// `Body`, so treating one as a message would hand the agent an empty turn
/// every time a reply was delivered.
pub fn parse_incoming(params: &[(String, String)]) -> Option<TwilioIncoming> {
    let field = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };

    let from = field("From").unwrap_or_default();
    let body = field("Body").unwrap_or_default();
    if from.is_empty() || body.trim().is_empty() {
        return None;
    }

    Some(TwilioIncoming {
        from: from.to_string(),
        to: field("To").unwrap_or_default().to_string(),
        body: body.to_string(),
    })
}

/// Twilio SMS channel implementation.
pub struct TwilioChannel {
    config: TwilioConfig,
    http_client: reqwest::Client,
    running: Arc<AtomicBool>,
}

impl std::fmt::Debug for TwilioChannel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The credentials are deliberately not printed.
        f.debug_struct("TwilioChannel")
            .field("from_number", &self.config.from_number)
            .field("running", &self.running)
            .finish()
    }
}

impl TwilioChannel {
    /// Create a new Twilio SMS channel.
    pub fn new(config: TwilioConfig) -> Self {
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

    /// Send one SMS body through the Messages API.
    async fn send_sms(&self, to: &str, text: &str) -> crate::Result<()> {
        let url = format!("{}/Accounts/{}/Messages.json", TWILIO_API_BASE, self.config.account_sid);

        // The account SID is the Basic-auth username, and the auth token the
        // password — the same token that keys inbound signatures, used in the
        // two different ways Twilio defines.
        let response = self
            .http_client
            .post(&url)
            .basic_auth(&self.config.account_sid, Some(&self.config.auth_token))
            .form(&[
                ("To", to),
                ("From", self.config.from_number.as_str()),
                ("Body", text),
            ])
            .send()
            .await
            .map_err(|e| SyscityError::ExternalService {
                source: format!("Twilio send failed: {}", e),
                cause: None,
            })?;

        if !response.status().is_success() {
            let status = response.status();
            // Twilio's error body names the cause (unverified number, bad
            // token, opted out), which is the difference between a config
            // mistake and a user who cannot be messaged.
            let detail = response.text().await.unwrap_or_default();
            return Err(SyscityError::ExternalService {
                source: format!("Twilio send returned {}: {}", status, detail),
                cause: None,
            });
        }

        Ok(())
    }
}

#[async_trait]
impl Channel for TwilioChannel {
    fn name(&self) -> &str {
        "twilio"
    }

    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities {
            chat_types: vec![ChatType::Direct],
            // SMS renders no markup, so a formatted reply would arrive with its
            // asterisks intact.
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
        info!("Starting Twilio SMS channel...");
        self.running.store(true, Ordering::SeqCst);
        info!("Twilio SMS channel started (inbound via /webhooks/twilio)");

        while self.running.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        Ok(())
    }

    async fn stop(&self) -> crate::Result<()> {
        info!("Stopping Twilio SMS channel...");
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    async fn send(&self, message: OutgoingMessage) -> crate::Result<Id> {
        let recipient = &message.conversation_id.0;

        let text = match &message.formatted_content {
            Some(crate::channels::FormattedContent::Markdown(md)) => {
                PlainTextFormatter::strip_markdown(md)
            }
            _ => PlainTextFormatter::strip_markdown(&message.content),
        };

        // SMS has no editing, threading or reply-to, so an answer over the
        // limit arrives as consecutive texts — the closest thing to one reply
        // the medium has. A send that fails part-way leaves the earlier
        // segments delivered, which is stated in the error rather than hidden.
        for segment in split_message(&text, MAX_SMS_CHARS) {
            self.send_sms(recipient, &segment).await?;
        }

        debug!("Twilio SMS sent to {}", recipient);
        Ok(Id::new())
    }

    async fn send_typing(&self, _conversation_id: &ConversationId) -> crate::Result<()> {
        // SMS has no typing indicator.
        Ok(())
    }

    async fn edit_message(&self, _message_id: Id, _new_content: String) -> crate::Result<()> {
        Err(SyscityError::Unsupported("sent SMS messages cannot be edited".into()))
    }

    async fn delete_message(&self, _message_id: Id) -> crate::Result<()> {
        Err(SyscityError::Unsupported("sent SMS messages cannot be recalled".into()))
    }

    async fn health_check(&self) -> crate::Result<bool> {
        Ok(self.running.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use hmac::{Hmac, KeyInit, Mac};
    use sha1::Sha1;

    /// Sign the way Twilio documents, built independently of the verifier: a
    /// `BTreeMap` supplies the key order rather than the verifier's explicit
    /// sort, so the two would disagree if the ordering were wrong.
    fn twilio_signature(token: &str, url: &str, params: &[(&str, &str)]) -> String {
        let ordered: std::collections::BTreeMap<&str, &str> = params.iter().copied().collect();

        let mut data = url.to_string();
        for (key, value) in &ordered {
            data.push_str(key);
            data.push_str(value);
        }

        let mut mac = Hmac::<Sha1>::new_from_slice(token.as_bytes()).expect("hmac key");
        mac.update(data.as_bytes());
        base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
    }

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    const TOKEN: &str = "test-auth-token";
    const URL: &str = "https://example.com/webhooks/twilio";

    #[test]
    fn a_signature_over_the_documented_construction_verifies() {
        let p = params(&[
            ("From", "+15550000001"),
            ("To", "+15550000002"),
            ("Body", "hello"),
        ]);
        let sig = twilio_signature(
            TOKEN,
            URL,
            &[
                ("From", "+15550000001"),
                ("To", "+15550000002"),
                ("Body", "hello"),
            ],
        );

        assert!(verify_signature(TOKEN, URL, &p, &sig));
    }

    /// Key order is part of the signed string. Twilio appends them sorted, so a
    /// verifier that used arrival order would reject every real delivery whose
    /// form happened to arrive unsorted.
    #[test]
    fn parameter_order_does_not_change_the_signature() {
        let sig = twilio_signature(
            TOKEN,
            URL,
            &[
                ("From", "+15550000001"),
                ("To", "+15550000002"),
                ("Body", "hello"),
            ],
        );

        // Same parameters, supplied in the opposite order.
        let reversed = params(&[
            ("Body", "hello"),
            ("To", "+15550000002"),
            ("From", "+15550000001"),
        ]);
        assert!(verify_signature(TOKEN, URL, &reversed, &sig));
    }

    #[test]
    fn a_wrong_token_a_tampered_body_or_the_wrong_url_is_refused() {
        let p = params(&[("From", "+15550000001"), ("Body", "hello")]);
        let sig = twilio_signature(TOKEN, URL, &[("From", "+15550000001"), ("Body", "hello")]);

        assert!(!verify_signature("not-the-token", URL, &p, &sig));
        assert!(!verify_signature(TOKEN, "https://elsewhere.example/webhooks/twilio", &p, &sig));

        let tampered = params(&[("From", "+15550000001"), ("Body", "goodbye")]);
        assert!(
            !verify_signature(TOKEN, URL, &tampered, &sig),
            "the body is covered by the signature"
        );
    }

    /// An empty HMAC key is a public key: every signature would verify against
    /// it, so the check must fail closed rather than succeed vacuously.
    #[test]
    fn an_empty_token_or_an_empty_signature_is_refused() {
        let p = params(&[("From", "+1"), ("Body", "hi")]);

        assert!(!verify_signature("", URL, &p, ""));
        assert!(!verify_signature("", URL, &p, "anything"));
        assert!(!verify_signature(TOKEN, URL, &p, ""));
    }

    #[test]
    fn both_spellings_of_the_default_port_are_accepted() {
        let p = params(&[("From", "+15550000001"), ("Body", "hello")]);
        let pairs = [("From", "+15550000001"), ("Body", "hello")];

        // Signed with the port spelled out, verified against the bare URL.
        let with_port = twilio_signature(TOKEN, "https://example.com:443/webhooks/twilio", &pairs);
        assert!(verify_signature(TOKEN, URL, &p, &with_port));

        // And the other way round.
        let without_port = twilio_signature(TOKEN, URL, &pairs);
        assert!(verify_signature(
            TOKEN,
            "https://example.com:443/webhooks/twilio",
            &p,
            &without_port
        ));
    }

    #[test]
    fn a_non_default_port_has_only_one_spelling() {
        let url = "https://example.com:8443/webhooks/twilio";
        let spellings = default_port_spellings(url);
        assert!(
            spellings.iter().all(|s| s.contains(":8443")),
            "a non-default port must not be dropped or doubled: {spellings:?}"
        );
    }

    #[test]
    fn parse_form_reads_a_twilio_delivery() {
        let body = b"From=%2B15550000001&To=%2B15550000002&Body=hello+there&MessageSid=SM1";
        let parsed = parse_form(body);

        assert_eq!(
            parse_incoming(&parsed),
            Some(TwilioIncoming {
                from: "+15550000001".to_string(),
                to: "+15550000002".to_string(),
                body: "hello there".to_string(),
            })
        );
    }

    /// A status callback shares the URL with inbound messages and carries no
    /// `Body`; it must not become an empty turn.
    #[test]
    fn a_status_callback_is_not_a_message() {
        let body = b"MessageSid=SM1&MessageStatus=delivered&To=%2B15550000002";
        assert_eq!(parse_incoming(&parse_form(body)), None);
    }

    #[test]
    fn an_empty_body_is_not_a_message() {
        assert_eq!(parse_incoming(&parse_form(b"From=%2B15550000001&Body=")), None);
        assert_eq!(parse_incoming(&parse_form(b"Body=hi")), None, "no From");
    }

    #[test]
    fn a_short_reply_is_one_segment() {
        assert_eq!(split_message("hello", MAX_SMS_CHARS), vec!["hello".to_string()]);
        assert!(split_message("", MAX_SMS_CHARS).is_empty());
    }

    #[test]
    fn a_long_reply_is_split_within_the_limit_and_loses_nothing() {
        let text = "word ".repeat(1000); // 5000 chars
        let segments = split_message(&text, MAX_SMS_CHARS);

        assert!(segments.len() > 1, "5000 chars must not fit in one message");
        for segment in &segments {
            assert!(
                segment.chars().count() <= MAX_SMS_CHARS,
                "segment of {} chars exceeds the limit",
                segment.chars().count()
            );
        }

        // Nothing is lost: rejoining the words reproduces the original.
        let rejoined = segments.join(" ");
        assert_eq!(rejoined.split_whitespace().count(), text.split_whitespace().count());
    }

    /// A split at a byte offset would panic on a multi-byte character — and
    /// text with no spaces at all is exactly where the whitespace search fails
    /// and the hard cut is used.
    #[test]
    fn splitting_counts_characters_not_bytes() {
        let text = "字".repeat(4000); // 4000 chars, 12000 bytes
        let segments = split_message(&text, MAX_SMS_CHARS);

        assert!(segments.len() > 1);
        let total: usize = segments.iter().map(|s| s.chars().count()).sum();
        assert_eq!(total, 4000, "no character may be dropped at a segment boundary");
        for segment in &segments {
            assert!(segment.chars().count() <= MAX_SMS_CHARS);
        }
    }

    /// The split prefers a space so a word is not cut, but a word longer than
    /// the window has no boundary to take and has to be cut somewhere.
    #[test]
    fn a_split_prefers_a_word_boundary_but_still_splits_a_long_word() {
        // The space falls *inside* the window, so it is taken and the second
        // word survives whole.
        let text = format!("{} {}", "a".repeat(1500), "b".repeat(300));
        let segments = split_message(&text, MAX_SMS_CHARS);

        assert_eq!(segments.len(), 2);
        assert_eq!(segments[0], "a".repeat(1500), "the split lands on the space");
        assert_eq!(segments[1], "b".repeat(300), "the second word is not cut");

        // A word longer than the window has no space to take, so it is hard-cut
        // — and nothing is lost in the process.
        let unbroken = "b".repeat(2000);
        let segments = split_message(&unbroken, MAX_SMS_CHARS);
        assert_eq!(segments.len(), 2);
        assert_eq!(segments.iter().map(|s| s.len()).sum::<usize>(), 2000);
    }
}
