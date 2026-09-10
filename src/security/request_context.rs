//! Per-request identity context threaded through the request lifecycle.
//!
//! Before this module existed, a request's authenticated identity was resolved
//! once at the WebSocket handshake and then discarded: audit entries fell back
//! to the literals `"shared"` / `"ws-user"` / `"anonymous"`, and the rate
//! limiter could only be keyed by whatever ad-hoc string a handler happened to
//! build. [`RequestContext`] captures that identity (at minimum a user id, plus
//! cheaply-available device id, session id, and auth source) at the request
//! entry point so it can be threaded inward to audit writers and rate limiters.
//!
//! The type is deliberately small and cheap to clone: every field is either an
//! [`Arc<str>`] or a `Copy` enum, so cloning is a handful of pointer bumps. It
//! carries no request payload and performs no I/O.
//!
//! The context is a pure carrier — constructing one never changes behaviour.
//! The default/single-user path still resolves to the same actor strings and
//! rate-limit keys as before; only the *plumbing* of that identity changes.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::security::UserId;

/// The transport-level source that established a request's identity.
///
/// This mirrors the gateway's `AuthMode` without depending on it (the security
/// module must not depend on the gateway module), so it is a self-contained
/// classification of *how* the caller proved who they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthSource {
    /// No authentication configured; the caller is anonymous.
    None,
    /// Authenticated by the configured shared token.
    SharedToken,
    /// Authenticated by a validated user session (cookie / Bearer / query).
    Session,
    /// Authenticated by a paired device token.
    Device,
    /// Authenticated by the Tailscale identity header.
    Tailscale,
    /// Authenticated by a trusted reverse proxy header.
    TrustedProxy,
    /// Internal origin with no transport request (background task, scheduler).
    System,
}

impl AuthSource {
    /// Stable lowercase label, suitable for logs and audit details.
    pub fn as_str(self) -> &'static str {
        match self {
            AuthSource::None => "none",
            AuthSource::SharedToken => "shared_token",
            AuthSource::Session => "session",
            AuthSource::Device => "device",
            AuthSource::Tailscale => "tailscale",
            AuthSource::TrustedProxy => "trusted_proxy",
            AuthSource::System => "system",
        }
    }
}

impl std::fmt::Display for AuthSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The identity of a single in-flight request.
///
/// Constructed once at the request entry point (the WebSocket dispatcher) and
/// passed by reference down to the handlers that perform auditing, rate
/// limiting, or identity-sensitive work.
///
/// All fields are cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestContext {
    user_id: Arc<str>,
    device_id: Option<Arc<str>>,
    session_id: Option<Arc<str>>,
    auth_source: AuthSource,
}

impl RequestContext {
    /// Build a context for `user_id` authenticated via `auth_source`.
    pub fn new(user_id: impl Into<Arc<str>>, auth_source: AuthSource) -> Self {
        Self {
            user_id: user_id.into(),
            device_id: None,
            session_id: None,
            auth_source,
        }
    }

    /// The context for an unauthenticated/anonymous caller.
    ///
    /// Preserves the historical `"anonymous"` actor string.
    pub fn anonymous() -> Self {
        Self::new("anonymous", AuthSource::None)
    }

    /// The context for an internal (non-request) origin such as a background
    /// task or the scheduler.
    pub fn system() -> Self {
        Self::new("system", AuthSource::System)
    }

    /// Derive a context from an already-resolved identity.
    ///
    /// `None` maps to [`RequestContext::anonymous`], matching the behaviour of
    /// handlers that previously did `.unwrap_or("anonymous")`.
    pub fn from_identity(user_id: Option<&UserId>, auth_source: AuthSource) -> Self {
        match user_id {
            Some(u) => Self::new(u.0.clone(), auth_source),
            None => Self::new("anonymous", auth_source),
        }
    }

    /// Attach the device id, if known.
    pub fn with_device_id(mut self, device_id: impl Into<Arc<str>>) -> Self {
        self.device_id = Some(device_id.into());
        self
    }

    /// Attach the session id, if known.
    pub fn with_session_id(mut self, session_id: impl Into<Arc<str>>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// The authenticated user id.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// The device id, when the request came from a paired device.
    pub fn device_id(&self) -> Option<&str> {
        self.device_id.as_deref()
    }

    /// The session id, when the request is bound to a chat session.
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    /// How the request's identity was established.
    pub fn auth_source(&self) -> AuthSource {
        self.auth_source
    }

    /// The actor string to record in audit entries.
    ///
    /// Intentionally just the user id: audit consumers (and existing tests)
    /// compare against the raw user id, so qualifying it would be a behaviour
    /// change.
    pub fn actor(&self) -> &str {
        &self.user_id
    }

    /// The user id as an owned [`UserId`], for APIs that key by `UserId`
    /// (e.g. the token-bucket rate limiter).
    pub fn to_user_id(&self) -> UserId {
        UserId::new(self.user_id.to_string())
    }

    /// A scope-qualified user id of the form `"{scope}:{user_id}"`.
    ///
    /// This reproduces the ad-hoc key format previously built inline at call
    /// sites (e.g. `"acp:spawn:{actor}"`), now derived from the context.
    pub fn scoped_user_id(&self, scope: &str) -> UserId {
        UserId::new(format!("{}:{}", scope, self.user_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::persistent_audit::PersistentAuditLog;
    use crate::security::runtime_audit::AuditEventType;
    use crate::security::RateLimiter;

    /// (a) An audit entry written with an injected context records that
    /// context's user as the actor — not the historical `"shared"` literal.
    #[tokio::test]
    async fn audit_entry_uses_injected_context_actor() {
        let log = PersistentAuditLog::new();
        let ctx = RequestContext::new("alice", AuthSource::SharedToken);

        log.log_with_context(AuditEventType::AcpSpawn, &ctx, "subagent-1", true, "spawned", None)
            .await;

        let entries = log.recent(1).await;
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].actor, "alice");
        assert_ne!(entries[0].actor, "shared");
    }

    /// (b) The rate limiter keys independently per user: exhausting one user's
    /// bucket does not deny another user.
    #[tokio::test]
    async fn rate_limiter_keys_independently_per_user() {
        // Capacity 1, negligible refill so the first hit exhausts the bucket.
        let limiter = RateLimiter::new(1, 0.0001);
        let alice = RequestContext::new("alice", AuthSource::SharedToken);
        let bob = RequestContext::new("bob", AuthSource::SharedToken);

        assert!(limiter.check_context(&alice).await.is_allowed());
        assert!(!limiter.check_context(&alice).await.is_allowed());
        // Bob's bucket is untouched by Alice's exhaustion.
        assert!(limiter.check_context(&bob).await.is_allowed());

        // Scope-qualified keys are independent from the bare user key as well.
        assert!(limiter
            .check_scoped_context(&alice, "acp:spawn", 1.0)
            .await
            .is_allowed());
        assert!(!limiter
            .check_scoped_context(&alice, "acp:spawn", 1.0)
            .await
            .is_allowed());
        assert!(limiter
            .check_scoped_context(&alice, "acp:message", 1.0)
            .await
            .is_allowed());
    }

    /// (c) The default/single-user paths still resolve to the same actor
    /// strings and rate-limit keys as before this change.
    #[tokio::test]
    async fn default_paths_preserve_prior_behaviour() {
        // Anonymous (auth_mode = none, unauthenticated) path.
        let anon = RequestContext::from_identity(None, AuthSource::None);
        assert_eq!(anon.actor(), "anonymous");
        assert_eq!(anon.auth_source(), AuthSource::None);

        // Shared-token path: the literal `"shared"` user id is preserved.
        let shared =
            RequestContext::from_identity(Some(&UserId::new("shared")), AuthSource::SharedToken);
        assert_eq!(shared.actor(), "shared");
        assert_eq!(shared.to_user_id(), UserId::new("shared"));
        // The scoped key matches the format previously built at call sites.
        assert_eq!(shared.scoped_user_id("acp:spawn"), UserId::new("acp:spawn:shared"));

        // Tailscale path: literal `"tailscale"` preserved.
        let ts =
            RequestContext::from_identity(Some(&UserId::new("tailscale")), AuthSource::Tailscale);
        assert_eq!(ts.actor(), "tailscale");

        // Device path: the device id is the user id.
        let dev = RequestContext::from_identity(Some(&UserId::new("dev-42")), AuthSource::Device)
            .with_device_id("dev-42");
        assert_eq!(dev.actor(), "dev-42");
        assert_eq!(dev.device_id(), Some("dev-42"));
        assert_eq!(dev.auth_source(), AuthSource::Device);

        // Internal origin.
        assert_eq!(RequestContext::system().actor(), "system");
    }

    #[test]
    fn auth_source_labels_are_stable() {
        assert_eq!(AuthSource::None.as_str(), "none");
        assert_eq!(AuthSource::SharedToken.as_str(), "shared_token");
        assert_eq!(AuthSource::Session.as_str(), "session");
        assert_eq!(AuthSource::Device.as_str(), "device");
        assert_eq!(AuthSource::Tailscale.as_str(), "tailscale");
        assert_eq!(AuthSource::TrustedProxy.as_str(), "trusted_proxy");
        assert_eq!(AuthSource::System.as_str(), "system");
        assert_eq!(AuthSource::Session.to_string(), "session");
    }

    #[test]
    fn builder_attaches_optional_ids() {
        let ctx = RequestContext::new("u", AuthSource::Session)
            .with_session_id("s-1")
            .with_device_id("d-1");
        assert_eq!(ctx.session_id(), Some("s-1"));
        assert_eq!(ctx.device_id(), Some("d-1"));
        assert_eq!(ctx.user_id(), "u");

        let bare = RequestContext::anonymous();
        assert_eq!(bare.session_id(), None);
        assert_eq!(bare.device_id(), None);
    }
}
