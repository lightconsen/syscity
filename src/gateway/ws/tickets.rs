//! Single-use tickets for the WebSocket upgrade.
//!
//! A browser cannot set headers on a WebSocket upgrade, so a client that is not
//! already connected has only the URL to carry a credential. That is how the
//! gateway's own UI ended up putting the shared token in a query string — where
//! it lands in devtools, in proxy logs, in anything that copies a URL, and
//! where it stays valid for as long as the token does.
//!
//! A ticket is the same idea with a short life: the client exchanges its
//! credential for one over HTTP (where headers *do* work), and the URL carries
//! something that is useless to anyone else and dead in
//! [`TICKET_TTL`](WsTickets) seconds. Tickets are consumed on use, so even a
//! URL captured in flight is worth one connection attempt.
//!
//! What a ticket grants is decided when it is minted, from the credential that
//! minted it: a ticket is a *handover* of the caller's own entitlement, never a
//! way to acquire more.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;
use tracing::debug;

/// How long a minted ticket stays valid.
///
/// Long enough to cover the client's next request (the UI fetches one and
/// connects immediately), short enough that a leaked URL is nearly worthless.
pub const TICKET_TTL: Duration = Duration::from_secs(30);

/// Upper bound on outstanding tickets.
///
/// The endpoint is authenticated, so this bounds a *legitimate* client that
/// mints in a loop rather than an attacker — but an unbounded map keyed by
/// caller-supplied strings is not something to leave in the tree.
const MAX_TICKETS: usize = 1024;

/// What a ticket is worth: the identity and scopes of whoever minted it.
#[derive(Debug, Clone)]
pub struct TicketGrant {
    /// Who the minting credential belonged to.
    pub user_id: String,
    /// What that credential was entitled to.
    pub scopes: Vec<String>,
    /// When the ticket stops being usable.
    expires_at: Instant,
}

/// The process's outstanding tickets.
#[derive(Debug, Default)]
pub struct WsTickets {
    issued: Mutex<HashMap<String, TicketGrant>>,
}

impl WsTickets {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a ticket worth `scopes` for `user_id`, valid once.
    ///
    /// The expiry is the store's to decide, so callers cannot mint a long-lived
    /// one by mistake.
    pub async fn issue(&self, user_id: impl Into<String>, scopes: Vec<String>) -> String {
        let mut issued = self.issued.lock().await;
        issued.retain(|_, t| t.expires_at > Instant::now());
        if issued.len() >= MAX_TICKETS {
            // Drop the closest to expiring rather than refuse the mint: the
            // caller is authenticated, and the oldest ticket is the one most
            // likely to be abandoned.
            if let Some(oldest) = issued
                .iter()
                .min_by_key(|(_, t)| t.expires_at)
                .map(|(k, _)| k.clone())
            {
                issued.remove(&oldest);
            }
        }
        let ticket = format!("wst_{}", uuid::Uuid::new_v4().simple());
        issued.insert(
            ticket.clone(),
            TicketGrant {
                user_id: user_id.into(),
                scopes,
                expires_at: Instant::now() + TICKET_TTL,
            },
        );
        ticket
    }

    /// Consume a ticket, returning what it was worth — exactly once.
    pub async fn consume(&self, ticket: &str) -> Option<TicketGrant> {
        let grant = self.issued.lock().await.remove(ticket)?;
        if grant.expires_at <= Instant::now() {
            debug!("WebSocket ticket expired before use");
            return None;
        }
        Some(grant)
    }

    /// How many tickets are outstanding (tests and metrics).
    ///
    /// Not `len`: there is no useful `is_empty` companion here, and clippy is
    /// right that a `len` without one invites the wrong idiom.
    pub async fn outstanding(&self) -> usize {
        self.issued.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(scope_list: &[&str]) -> Vec<String> {
        scope_list.iter().map(|s| s.to_string()).collect()
    }

    #[tokio::test]
    async fn a_ticket_is_worth_one_upgrade() {
        let tickets = WsTickets::new();
        let ticket = tickets.issue("shared", scopes(&["chat", "read"])).await;

        let used = tickets.consume(&ticket).await.expect("first use");
        assert_eq!(used.user_id, "shared");
        assert_eq!(used.scopes, vec!["chat".to_string(), "read".to_string()]);
        assert!(
            tickets.consume(&ticket).await.is_none(),
            "a ticket is single-use — a replayed URL buys nothing"
        );
        assert_eq!(tickets.outstanding().await, 0);
    }

    #[tokio::test]
    async fn an_expired_ticket_is_refused() {
        let tickets = WsTickets::new();
        let ticket = tickets.issue("shared", scopes(&["chat"])).await;
        // Reach into the store rather than sleeping 30 s.
        {
            let mut issued = tickets.issued.lock().await;
            issued.get_mut(&ticket).unwrap().expires_at = Instant::now() - TICKET_TTL;
        }
        assert!(tickets.consume(&ticket).await.is_none());
    }

    /// A ticket carries what its minting credential was entitled to, so
    /// minting cannot be used to widen access.
    #[tokio::test]
    async fn a_ticket_keeps_the_minters_scopes() {
        let tickets = WsTickets::new();
        let ticket = tickets.issue("shared", scopes(&["read"])).await;
        let used = tickets.consume(&ticket).await.unwrap();
        assert_eq!(used.scopes, vec!["read".to_string()]);
    }

    #[tokio::test]
    async fn unknown_tickets_are_refused() {
        let tickets = WsTickets::new();
        assert!(tickets.consume("wst_nope").await.is_none());
    }

    /// The map cannot grow without bound.
    #[tokio::test]
    async fn issuing_beyond_capacity_drops_the_oldest() {
        let tickets = WsTickets::new();
        for _ in 0..MAX_TICKETS + 10 {
            tickets.issue("shared", scopes(&["chat"])).await;
        }
        assert!(tickets.outstanding().await <= MAX_TICKETS);
    }
}
