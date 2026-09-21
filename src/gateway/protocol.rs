//! Syscity WebSocket Protocol
//!
//! Implements the WebSocket-native RPC protocol defined in docs/protocol.md.
//! Uses req/res/event framing aligned with

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::gateway::GatewayEvent;
use crate::security::UserId;
// ── Protocol Version
// ──────────────────────────────────────────────────────────

/// Current protocol version
pub const PROTOCOL_VERSION: u32 = 1;

/// Minimum protocol version supported by this server
pub const PROTOCOL_VERSION_MIN: u32 = 1;

// ── Frame Types
// ───────────────────────────────────────────────────────────────

/// A frame received from the client (always a request)
#[derive(Debug, Clone, Deserialize)]
pub struct WsRequest {
    /// Frame type discriminator — always "req" for client messages
    #[serde(rename = "type")]
    pub frame_type: String,
    /// Client-generated request ID (mirrored in response)
    pub id: String,
    /// Method name, dot-namespaced (e.g. "chat.send")
    pub method: String,
    /// Method-specific parameters
    #[serde(default)]
    pub params: Option<serde_json::Value>,
}

/// A response frame sent to the client
#[derive(Debug, Clone, Serialize)]
pub struct WsResponse {
    /// Frame type discriminator — always "res"
    #[serde(rename = "type")]
    pub frame_type: &'static str,
    /// Mirrors the request ID
    pub id: String,
    /// Success flag
    pub ok: bool,
    /// Response payload (when ok = true)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Error details (when ok = false)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WsError>,
}

/// Serialize a frame's payload, reporting the failure rather than dropping it.
///
/// The payload field is optional on the wire, so a failure yields a frame with
/// no payload instead of no frame — but it must not be *silent*: `ok: true`
/// with nothing in it is indistinguishable, from the caller's side, from a
/// handler that returned nothing, and this project's rule is that failures are
/// observable.
fn payload_or_warn(what: &str, id: &str, payload: &impl Serialize) -> Option<serde_json::Value> {
    match serde_json::to_value(payload) {
        Ok(value) => Some(value),
        Err(e) => {
            warn!("Failed to serialize payload for {what} '{id}': {e}");
            None
        }
    }
}

impl WsResponse {
    /// Build a successful response
    pub fn ok(id: impl Into<String>, payload: impl Serialize) -> Self {
        let id = id.into();
        Self {
            frame_type: "res",
            payload: payload_or_warn("response", &id, &payload),
            id,
            ok: true,
            error: None,
        }
    }

    /// Build an error response
    pub fn err(id: impl Into<String>, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            frame_type: "res",
            id: id.into(),
            ok: false,
            payload: None,
            error: Some(WsError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

/// An event frame pushed from server to client
#[derive(Debug, Clone, Serialize)]
pub struct WsEvent {
    /// Frame type discriminator — always "event"
    #[serde(rename = "type")]
    pub frame_type: &'static str,
    /// Event name (e.g. "chat.delta")
    pub event: String,
    /// Event-specific payload
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    /// Monotonic sequence number for ordering/dedup
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seq: Option<u64>,
}

impl WsEvent {
    /// Build an event frame
    pub fn new(event: impl Into<String>, payload: impl Serialize, seq: u64) -> Self {
        let event = event.into();
        Self {
            frame_type: "event",
            payload: payload_or_warn("event", &event, &payload),
            event,
            seq: Some(seq),
        }
    }
}

/// Error shape in a response frame
#[derive(Debug, Clone, Serialize)]
pub struct WsError {
    /// Error code (e.g. "UNAUTHORIZED", "SESSION_NOT_FOUND")
    pub code: String,
    /// Human-readable error message
    pub message: String,
}

// ── Connect Handshake
// ─────────────────────────────────────────────────────────

/// Parameters sent by client in the first `connect` request
#[derive(Debug, Clone, Deserialize)]
pub struct ConnectParams {
    /// Protocol version requested by client
    pub protocol_version: u32,
    /// Client identification
    pub client: Option<ClientInfo>,
    /// Authentication credentials
    pub auth: Option<AuthParams>,
    /// Device identity (for device pairing mode)
    pub device: Option<DeviceIdentity>,
    /// Requested scopes
    #[serde(default)]
    pub scopes: Vec<String>,
}

/// Client identification
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ClientInfo {
    /// Client type: "web", "ios", "android", "cli"
    pub id: String,
    /// Client software version
    #[serde(default)]
    pub version: String,
}

/// Authentication parameters within connect
#[derive(Debug, Clone, Deserialize)]
pub struct AuthParams {
    /// Shared token or device token
    #[serde(default)]
    pub token: Option<String>,
    /// Password (for password auth mode)
    #[serde(default)]
    pub password: Option<String>,
}

/// Device identity for pairing
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DeviceIdentity {
    /// Device unique ID
    pub id: String,
    /// Ed25519 public key (base64)
    #[serde(default)]
    pub public_key: Option<String>,
    /// Signature over nonce + timestamp
    #[serde(default)]
    pub signature: Option<String>,
    /// Nonce from the connect challenge
    #[serde(default)]
    pub nonce: Option<String>,
}

/// Payload of the hello-ok response
#[derive(Debug, Clone, Serialize)]
pub struct HelloOkPayload {
    /// Protocol version accepted by server
    pub protocol_version: u32,
    /// Session key derived for this connection
    pub session_key: String,
    /// Available features / methods
    pub features: Vec<String>,
    /// Scopes granted to this connection
    pub scopes_granted: Vec<String>,
    /// Server info
    pub server: ServerInfo,
}

/// Server info in hello-ok
#[derive(Debug, Clone, Serialize)]
pub struct ServerInfo {
    /// Server version string
    pub version: String,
    /// Connection ID
    pub conn_id: String,
}

// ── Scopes (Operator Scope system)
// ────────────────────────────────────────────
//
// Syscity uses an Operator Scope model. Each WebSocket method declares a
// required scope; the gateway verifies the connection's granted scopes before
// dispatch.
//
// Scope hierarchy (least to most privileged):
// read < chat < write < acp < pairing < admin

// Scope: chat operations (send, history, abort)
pub const SCOPE_CHAT: &str = "chat";
/// Scope: read-only queries
pub const SCOPE_READ: &str = "read";
/// Scope: write/modify operations
pub const SCOPE_WRITE: &str = "write";
/// Scope: admin (full access)
pub const SCOPE_ADMIN: &str = "admin";
/// Scope: device pairing management
pub const SCOPE_PAIRING: &str = "pairing";
/// Scope: ACP (Agent Control Plane) operations
pub const SCOPE_ACP: &str = "acp";

/// All available scopes
pub const ALL_SCOPES: &[&str] = &[
    SCOPE_CHAT,
    SCOPE_READ,
    SCOPE_WRITE,
    SCOPE_ADMIN,
    SCOPE_PAIRING,
    SCOPE_ACP,
];

/// Default scopes granted when none are explicitly requested
pub const DEFAULT_SCOPES: &[&str] = &[SCOPE_CHAT, SCOPE_READ];

/// Resolve the scopes a connection is actually granted.
///
/// `entitled` is what the *credential* confers — derived from configuration or
/// from the stored record, never from the request. `requested` is the client's
/// wish list, and it may only ever **narrow** the result: a client can ask for
/// less than it is entitled to, never more.
///
/// This is the single place authorization is decided. Before it existed, three
/// of the four auth paths assigned the client's requested scopes verbatim
/// (`params.scopes.clone()`), so any client that could reach `/ws` could ask
/// for `admin` and receive it — under `auth_mode = "none"` that needed no
/// credential at all.
///
/// An empty request means "give me what this credential is worth", which is
/// what a client that does not care about scopes should send.
pub fn resolve_scopes(entitled: &[String], requested: &[String]) -> Vec<String> {
    if requested.is_empty() {
        return entitled.to_vec();
    }
    entitled
        .iter()
        .filter(|scope| requested.contains(scope))
        .cloned()
        .collect()
}

/// Every method the dispatcher handles, with the scope it requires.
///
/// The single source of truth for authorization: [`method_scope`] looks up
/// here, `methods.list` publishes it, and a test holds it against the
/// dispatcher's arms. `None` means the method needs no scope at all (the
/// pre-auth handshake pair).
///
/// Data rather than a `match` so it can be published and diffed: the table
/// and the dispatcher used to drift silently, and `subscribe` and friends
/// spent that time demanding `admin` by falling through the default.
/// [`method_scope`] scans it linearly — a couple of hundred short compares,
/// once per request, against work that includes a WebSocket read.
pub const METHOD_SCOPES: &[(&str, Option<&str>)] = &[
    ("chat.send", Some(SCOPE_CHAT)),
    ("chat.abort", Some(SCOPE_CHAT)),
    ("ask.respond", Some(SCOPE_CHAT)),
    ("feedback.vote", Some(SCOPE_CHAT)),
    // `models.default` genuinely is a read; the rest of the `models.*`
    // family writes provider config (and API keys) to disk — see the write
    // group.
    ("chat.history", Some(SCOPE_READ)),
    ("sessions.list", Some(SCOPE_READ)),
    ("agents.list", Some(SCOPE_READ)),
    ("agents.get", Some(SCOPE_READ)),
    ("agents.get_config", Some(SCOPE_READ)),
    ("agents.memory.get", Some(SCOPE_READ)),
    ("agents.export", Some(SCOPE_READ)),
    ("agents.registry", Some(SCOPE_READ)),
    ("health", Some(SCOPE_READ)),
    ("system.presence", Some(SCOPE_READ)),
    ("cost.get", Some(SCOPE_READ)),
    ("commands.list", Some(SCOPE_READ)),
    ("config.get", Some(SCOPE_READ)),
    ("methods.list", Some(SCOPE_READ)),
    ("models.list", Some(SCOPE_READ)),
    ("models.presets", Some(SCOPE_READ)),
    ("models.default", Some(SCOPE_READ)),
    ("cron.list", Some(SCOPE_READ)),
    ("skills.list", Some(SCOPE_READ)),
    ("logs.subscribe", Some(SCOPE_READ)),
    ("logs.unsubscribe", Some(SCOPE_READ)),
    ("workspace.list", Some(SCOPE_READ)),
    ("workspace.read", Some(SCOPE_READ)),
    ("tasks.list", Some(SCOPE_READ)),
    ("mcp.list", Some(SCOPE_READ)),
    ("mcp.presets", Some(SCOPE_READ)),
    ("mcp.tools", Some(SCOPE_READ)),
    ("mcp.resources", Some(SCOPE_READ)),
    ("mcp.auth_status", Some(SCOPE_READ)),
    ("device.capabilities", Some(SCOPE_READ)),
    ("device.permission.status", Some(SCOPE_READ)),
    ("device.adb.status", Some(SCOPE_READ)),
    ("device.shortcut.results", Some(SCOPE_READ)),
    ("device.shortcut.inbox", Some(SCOPE_READ)),
    ("eval.trace.list", Some(SCOPE_READ)),
    ("eval.dashboard", Some(SCOPE_READ)),
    ("eval.optimizer.status", Some(SCOPE_READ)),
    ("feedback.ops", Some(SCOPE_READ)),
    ("connectors.list", Some(SCOPE_READ)),
    ("connectors.auth_status", Some(SCOPE_READ)),
    ("connectors.updates", Some(SCOPE_READ)),
    ("connectors.catalog", Some(SCOPE_READ)),
    ("onboarding.status", Some(SCOPE_READ)),
    ("cloud.status", Some(SCOPE_READ)),
    ("cloud.subscription", Some(SCOPE_READ)),
    ("cloud.usage", Some(SCOPE_READ)),
    ("cloud.credits.claims", Some(SCOPE_READ)),
    ("cloud.credits.packs", Some(SCOPE_READ)),
    ("cloud.credits.ledger", Some(SCOPE_READ)),
    ("cloud.credits.invite", Some(SCOPE_READ)),
    ("update.status", Some(SCOPE_READ)),
    ("update.progress", Some(SCOPE_READ)),
    ("plugins.list", Some(SCOPE_READ)),
    ("plugins.search", Some(SCOPE_READ)),
    ("providers.list", Some(SCOPE_READ)),
    ("providers.usage", Some(SCOPE_READ)),
    ("providers.health", Some(SCOPE_READ)),
    ("providers.fallback", Some(SCOPE_READ)),
    ("traces.get", Some(SCOPE_READ)),
    ("cron.get", Some(SCOPE_READ)),
    ("cron.logs", Some(SCOPE_READ)),
    ("skills.get", Some(SCOPE_READ)),
    ("channels.list", Some(SCOPE_READ)),
    ("approvals.list", Some(SCOPE_READ)),
    ("approvals.get", Some(SCOPE_READ)),
    ("audit.recent", Some(SCOPE_READ)),
    ("audit.all", Some(SCOPE_READ)),
    ("memory.search", Some(SCOPE_READ)),
    ("memory.collections", Some(SCOPE_READ)),
    ("mention.policy", Some(SCOPE_READ)),
    ("mention.allowlist", Some(SCOPE_READ)),
    ("mention.blocklist", Some(SCOPE_READ)),
    ("auth_profiles.list", Some(SCOPE_READ)),
    ("auth_profiles.get", Some(SCOPE_READ)),
    ("security.gate.list", Some(SCOPE_READ)),
    ("security.allowlist.list", Some(SCOPE_READ)),
    ("security.status", Some(SCOPE_READ)),
    ("status.get", Some(SCOPE_READ)),
    ("kb.collections", Some(SCOPE_READ)),
    ("kb.docs", Some(SCOPE_READ)),
    ("kb.doc_content", Some(SCOPE_READ)),
    ("cloud.kb.list", Some(SCOPE_READ)),
    ("cloud.kb.docs", Some(SCOPE_READ)),
    ("cloud.kb.query", Some(SCOPE_READ)),
    // Pairing-request inspection hands out the pairing code and the device
    // inventory, so it sits with approve/reject/revoke rather than with the
    // read-only queries: a `read` client must not be able to mint a code.
    // These mutate durable state or execute code: models.* writes provider
    // config (including API keys) to `config.toml`, `models.fetch_remote`
    // takes the endpoint from the caller, `skills.install` extracts an
    // archive, and `mcp.call_tool` invokes an arbitrary tool.
    ("sessions.create", Some(SCOPE_WRITE)),
    ("sessions.delete", Some(SCOPE_WRITE)),
    ("agents.create", Some(SCOPE_WRITE)),
    ("agents.delete", Some(SCOPE_WRITE)),
    ("agents.purge", Some(SCOPE_WRITE)),
    ("agents.rename", Some(SCOPE_WRITE)),
    ("cost.reset", Some(SCOPE_WRITE)),
    ("sessions.rename", Some(SCOPE_WRITE)),
    ("sessions.set_pinned", Some(SCOPE_WRITE)),
    ("sessions.set_model", Some(SCOPE_WRITE)),
    ("sessions.set_mode", Some(SCOPE_WRITE)),
    ("sessions.reset", Some(SCOPE_WRITE)),
    ("sessions.subscribe", Some(SCOPE_WRITE)),
    ("sessions.unsubscribe", Some(SCOPE_WRITE)),
    // Legacy aliases of `sessions.subscribe`/`unsubscribe` and the
    // connection-local subscribe-all. All three are dispatched but were
    // absent from this table, so they fell through to the `admin` default —
    // the drift the table/dispatcher check exists to catch.
    ("subscribe", Some(SCOPE_WRITE)),
    ("unsubscribe", Some(SCOPE_WRITE)),
    ("subscribe_all", Some(SCOPE_WRITE)),
    // Pops the macOS accessibility prompt: a side effect on the user's
    // machine, so it asks for the same scope as any other write.
    ("permissions.request_macos_accessibility", Some(SCOPE_WRITE)),
    ("commands.execute", Some(SCOPE_WRITE)),
    ("config.set", Some(SCOPE_WRITE)),
    ("tasks.schedule", Some(SCOPE_WRITE)),
    ("tasks.delete", Some(SCOPE_WRITE)),
    ("tasks.enable", Some(SCOPE_WRITE)),
    ("tasks.disable", Some(SCOPE_WRITE)),
    ("mcp.add", Some(SCOPE_WRITE)),
    ("mcp.remove", Some(SCOPE_WRITE)),
    ("mcp.connect", Some(SCOPE_WRITE)),
    ("mcp.disconnect", Some(SCOPE_WRITE)),
    ("mcp.auth_cancel", Some(SCOPE_WRITE)),
    ("device.permission.request", Some(SCOPE_WRITE)),
    ("device.adb.pair", Some(SCOPE_WRITE)),
    ("device.shortcut.run", Some(SCOPE_WRITE)),
    ("device.pairing.pending", Some(SCOPE_WRITE)),
    ("device.pairing.authorized", Some(SCOPE_WRITE)),
    ("device.pairing.qr", Some(SCOPE_WRITE)),
    ("device.pairing.setup", Some(SCOPE_WRITE)),
    ("device.pairing.approve", Some(SCOPE_WRITE)),
    ("device.pairing.reject", Some(SCOPE_WRITE)),
    ("device.pairing.revoke", Some(SCOPE_WRITE)),
    ("models.fetch_remote", Some(SCOPE_WRITE)),
    ("models.add", Some(SCOPE_WRITE)),
    ("models.remove", Some(SCOPE_WRITE)),
    ("models.set_default", Some(SCOPE_WRITE)),
    ("skills.install", Some(SCOPE_WRITE)),
    ("mcp.call_tool", Some(SCOPE_WRITE)),
    ("system.reload", Some(SCOPE_WRITE)),
    ("channels.enable", Some(SCOPE_WRITE)),
    ("channels.disable", Some(SCOPE_WRITE)),
    ("agents.update", Some(SCOPE_WRITE)),
    ("agents.default", Some(SCOPE_WRITE)),
    ("agents.memory.clear", Some(SCOPE_WRITE)),
    ("agents.import", Some(SCOPE_WRITE)),
    ("security.gate.set", Some(SCOPE_WRITE)),
    ("security.gate.clear", Some(SCOPE_WRITE)),
    ("security.allowlist.add", Some(SCOPE_WRITE)),
    ("security.allowlist.remove", Some(SCOPE_WRITE)),
    ("approvals.approve", Some(SCOPE_WRITE)),
    ("approvals.deny", Some(SCOPE_WRITE)),
    ("memory.add", Some(SCOPE_WRITE)),
    ("mention.policy.set", Some(SCOPE_WRITE)),
    ("mention.allowlist.add", Some(SCOPE_WRITE)),
    ("mention.allowlist.remove", Some(SCOPE_WRITE)),
    ("mention.blocklist.add", Some(SCOPE_WRITE)),
    ("mention.blocklist.remove", Some(SCOPE_WRITE)),
    ("auth_profiles.rotate", Some(SCOPE_WRITE)),
    ("eval.optimizer.run", Some(SCOPE_WRITE)),
    ("eval.optimizer.resume", Some(SCOPE_WRITE)),
    ("eval.optimizer.rollback", Some(SCOPE_WRITE)),
    ("eval.propose", Some(SCOPE_WRITE)),
    ("connectors.install", Some(SCOPE_WRITE)),
    ("connectors.enable", Some(SCOPE_WRITE)),
    ("connectors.disable", Some(SCOPE_WRITE)),
    ("connectors.uninstall", Some(SCOPE_WRITE)),
    ("connectors.catalog_install", Some(SCOPE_WRITE)),
    ("onboarding.apply", Some(SCOPE_WRITE)),
    ("cloud.token", Some(SCOPE_WRITE)),
    ("cloud.logout", Some(SCOPE_WRITE)),
    ("update.trigger", Some(SCOPE_WRITE)),
    ("plugins.enable", Some(SCOPE_WRITE)),
    ("plugins.disable", Some(SCOPE_WRITE)),
    ("plugins.install", Some(SCOPE_WRITE)),
    ("plugins.sign", Some(SCOPE_WRITE)),
    ("plugins.unload", Some(SCOPE_WRITE)),
    ("plugins.reload", Some(SCOPE_WRITE)),
    ("plugins.reload_all", Some(SCOPE_WRITE)),
    ("plugins.uninstall", Some(SCOPE_WRITE)),
    ("providers.enable", Some(SCOPE_WRITE)),
    ("providers.disable", Some(SCOPE_WRITE)),
    ("providers.check", Some(SCOPE_WRITE)),
    ("providers.switch", Some(SCOPE_WRITE)),
    ("cron.enable", Some(SCOPE_WRITE)),
    ("cron.disable", Some(SCOPE_WRITE)),
    ("cron.run", Some(SCOPE_WRITE)),
    ("cron.add", Some(SCOPE_WRITE)),
    ("cron.remove", Some(SCOPE_WRITE)),
    ("skills.enable", Some(SCOPE_WRITE)),
    ("skills.disable", Some(SCOPE_WRITE)),
    ("skills.uninstall", Some(SCOPE_WRITE)),
    ("skills.run", Some(SCOPE_WRITE)),
    ("kb.ingest", Some(SCOPE_WRITE)),
    ("kb.delete_doc", Some(SCOPE_WRITE)),
    ("cloud.kb.create", Some(SCOPE_WRITE)),
    ("cloud.kb.delete", Some(SCOPE_WRITE)),
    ("cloud.kb.upload", Some(SCOPE_WRITE)),
    ("cloud.kb.push", Some(SCOPE_WRITE)),
    ("cloud.kb.pull", Some(SCOPE_WRITE)),
    ("cloud.credits.daily_claim", Some(SCOPE_WRITE)),
    ("cloud.credits.signup_claim", Some(SCOPE_WRITE)),
    ("cloud.credits.invite_redeem", Some(SCOPE_WRITE)),
    ("acp.spawn", Some(SCOPE_ACP)),
    ("acp.terminate", Some(SCOPE_ACP)),
    ("acp.message", Some(SCOPE_ACP)),
    ("acp.pause", Some(SCOPE_ACP)),
    ("acp.resume", Some(SCOPE_ACP)),
    ("acp.step", Some(SCOPE_ACP)),
    ("acp.cancel", Some(SCOPE_ACP)),
    ("acp.execute.session", Some(SCOPE_ACP)),
    ("acp.execute.run", Some(SCOPE_ACP)),
    ("acp.list", Some(SCOPE_READ)),
    ("acp.status", Some(SCOPE_READ)),
    ("acp.tree", Some(SCOPE_READ)),
    ("connect", None),
    ("ping", None),
    // Admin scope required for unknown methods (default-deny)
];

/// Check if a method requires a specific scope.
///
/// An unknown method requires `admin`: an unrecognized name must not be a way
/// to reach anything at all, let alone something cheaper than `admin`.
pub fn method_scope(method: &str) -> Option<&'static str> {
    match METHOD_SCOPES.iter().find(|(name, _)| *name == method) {
        Some((_, scope)) => *scope,
        None => Some(SCOPE_ADMIN),
    }
}

/// Check if granted scopes allow a method
pub fn scopes_allow(granted: &[String], method: &str) -> bool {
    if granted.contains(&SCOPE_ADMIN.to_string()) {
        return true;
    }

    let required = match method_scope(method) {
        Some(s) => s,
        None => return true, // No scope required
    };

    granted.contains(&required.to_string())
}

// ── Auth Mode
// ─────────────────────────────────────────────────────────────────

/// Gateway authentication mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AuthMode {
    /// No authentication (development only)
    #[serde(rename = "none")]
    #[default]
    None,
    /// Shared secret token
    #[serde(rename = "token")]
    Token,
    /// Device pairing required
    #[serde(rename = "device")]
    Device,
    /// Tailscale automatic auth
    #[serde(rename = "tailscale")]
    Tailscale,
}

// ── Connection State
// ──────────────────────────────────────────────────────────

/// Per-connection state maintained during the WebSocket session
#[derive(Debug)]
pub struct ProtocolConnection {
    /// Whether the handshake is complete
    pub handshaked: bool,
    /// Granted scopes
    pub scopes: Vec<String>,
    /// User ID if authenticated
    pub user_id: Option<UserId>,
    /// Client info
    pub client: Option<ClientInfo>,
    /// Subscribed session IDs (empty = all)
    pub subscriptions: Vec<String>,
    /// Monotonic sequence counter for events
    pub seq: u64,
    /// Connection ID
    pub conn_id: String,
    /// Log subscription cancel sender
    pub log_cancel_tx: Option<tokio::sync::mpsc::Sender<()>>,
}

impl ProtocolConnection {
    pub fn new(conn_id: impl Into<String>) -> Self {
        Self {
            handshaked: false,
            scopes: DEFAULT_SCOPES.iter().map(|s| s.to_string()).collect(),
            user_id: None,
            client: None,
            subscriptions: Vec::new(),
            seq: 0,
            conn_id: conn_id.into(),
            log_cancel_tx: None,
        }
    }

    /// Increment and return the next sequence number
    pub fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Check if this connection is subscribed to a session
    pub fn is_subscribed(&self, session_id: &str) -> bool {
        self.subscriptions.is_empty() || self.subscriptions.contains(&session_id.to_string())
    }
}

// ── GatewayEvent → WsEvent Mapping
// ────────────────────────────────────────────

/// Convert a GatewayEvent to a WsEvent name + payload
pub fn gateway_event_to_ws(event: &GatewayEvent) -> Option<(String, serde_json::Value)> {
    match event {
        GatewayEvent::AgentResponse { .. } => {
            // Suppressed: non-streaming responses emit chat.final via Completed,
            // so emitting chat.delta here would duplicate the full content.
            None
        }
        GatewayEvent::Thinking { session_id, agent_id, content } => Some((
            "agent.thinking".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "content": content,
            }),
        )),
        GatewayEvent::ContentDelta { session_id, agent_id, delta } => Some((
            "chat.delta".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "content": delta,
            }),
        )),
        GatewayEvent::ToolCalling {
            session_id,
            agent_id,
            tool_name,
            arguments,
        } => Some((
            "tool.calling".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "tool_name": tool_name,
                "arguments": arguments,
            }),
        )),
        GatewayEvent::ToolResult {
            session_id,
            agent_id,
            tool_name,
            result,
            data,
        } => Some((
            "tool.result".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "tool_name": tool_name,
                "result": result,
                "data": data,
            }),
        )),
        GatewayEvent::Completed {
            session_id,
            agent_id,
            response,
            turn_id,
            usage,
        } => {
            let mut payload = serde_json::json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "response": response,
                "turn_id": turn_id,
            });
            if let Some(u) = usage {
                // Cloud relay credit metering + token totals for the turn.
                payload["usage"] = serde_json::json!({
                    "credits_used": u.x_credits_used,
                    "credit_balance": u.x_credit_balance,
                    "total_tokens": u.total_tokens,
                });
            }
            Some(("chat.final".to_string(), payload))
        }
        GatewayEvent::ProcessingError {
            session_id,
            agent_id,
            message,
            code,
        } => {
            let mut payload = serde_json::json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "message": message,
            });
            if let Some(code) = code {
                payload["code"] = serde_json::json!(code);
            }
            Some(("chat.error".to_string(), payload))
        }
        GatewayEvent::MessageReceived {
            channel,
            user_id,
            content,
            timestamp,
        } => Some((
            "message.received".to_string(),
            serde_json::json!({
                "channel": channel,
                "user_id": user_id,
                "content": content,
                "timestamp": timestamp,
            }),
        )),
        GatewayEvent::AgentStatus { agent_id, status } => Some((
            "agent.status".to_string(),
            serde_json::json!({
                "agent_id": agent_id,
                "status": format!("{:?}", status),
            }),
        )),
        GatewayEvent::ChannelStatus { channel, connected } => Some((
            "channel.status".to_string(),
            serde_json::json!({
                "channel": channel,
                "connected": connected,
            }),
        )),
        GatewayEvent::ApprovalRequired {
            approval_id,
            tool_name,
            requested_by,
            risk_level,
            message,
            session_id,
        } => {
            let mut payload = serde_json::json!({
                "approval_id": approval_id,
                "tool_name": tool_name,
                "requested_by": requested_by,
                "risk_level": format!("{:?}", risk_level),
                "message": message,
            });
            if let Some(sid) = session_id {
                payload["session_id"] = serde_json::Value::String(sid.clone());
            }
            Some(("approval.required".to_string(), payload))
        }
        GatewayEvent::CronAnnounce { channel: _, to: _, message } => {
            // message is a JSON string produced by CronScheduler; try to parse it
            let payload = serde_json::from_str(message)
                .unwrap_or_else(|_| serde_json::json!({ "message": message }));
            Some(("cron.completed".to_string(), payload))
        }
        GatewayEvent::RepairAction {
            kind,
            target_id,
            description,
            restart_count,
        } => Some((
            "repair.action".to_string(),
            serde_json::json!({
                "kind": kind,
                "target_id": target_id,
                "description": description,
                "restart_count": restart_count,
            }),
        )),
        GatewayEvent::DevicePairRequested { device_id, code, display_name } => Some((
            "device.pair.requested".to_string(),
            serde_json::json!({
                "device_id": device_id,
                "code": code,
                "display_name": display_name,
            }),
        )),
        GatewayEvent::SessionCreated { session_id, agent_id, user_id } => Some((
            "session.created".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "user_id": user_id,
            }),
        )),
        GatewayEvent::SessionRenamed { session_id, name } => Some((
            "session.renamed".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "name": name,
            }),
        )),
        GatewayEvent::SessionPinned { session_id, pinned } => Some((
            "session.pinned".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "pinned": pinned,
            }),
        )),
        GatewayEvent::SessionModelChanged { session_id, model } => Some((
            "session.model_changed".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "model": model,
            }),
        )),
        GatewayEvent::SessionModeChanged { session_id, mode } => Some((
            "session.mode_changed".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "mode": mode,
            }),
        )),
        GatewayEvent::AcpSpawned {
            session_id,
            subagent_id,
            parent_id,
            mode,
            thread_id,
        } => Some((
            "acp.spawned".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "subagent_id": subagent_id,
                "parent_id": parent_id,
                "mode": mode,
                "thread_id": thread_id,
            }),
        )),
        GatewayEvent::AcpCompleted {
            session_id,
            subagent_id,
            status,
        } => Some((
            "acp.completed".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "subagent_id": subagent_id,
                "status": status,
            }),
        )),
        GatewayEvent::AcpStatusChanged { session_id, runtime_state } => Some((
            "acp.status_changed".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "runtime_state": runtime_state,
            }),
        )),
        GatewayEvent::AcpRecovered {
            session_id,
            old_subagent_id,
            new_subagent_id,
            crash_count,
        } => Some((
            "acp.recovered".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "old_subagent_id": old_subagent_id,
                "new_subagent_id": new_subagent_id,
                "crash_count": crash_count,
            }),
        )),
        GatewayEvent::AcpThreadSwitched { thread_id, active_subagent } => Some((
            "acp.thread_switched".to_string(),
            serde_json::json!({
                "thread_id": thread_id,
                "active_subagent": active_subagent,
            }),
        )),
        GatewayEvent::McpConnected {
            server_id,
            tools,
            prompts,
            resources,
        } => Some((
            "mcp.connected".to_string(),
            serde_json::json!({
                "server_id": server_id,
                "tools": tools,
                "prompts": prompts,
                "resources": resources,
            }),
        )),
        GatewayEvent::McpDisconnected { server_id, reason } => Some((
            "mcp.disconnected".to_string(),
            serde_json::json!({
                "server_id": server_id,
                "reason": reason,
            }),
        )),
        GatewayEvent::McpRecovered { server_id, attempt } => Some((
            "mcp.recovered".to_string(),
            serde_json::json!({
                "server_id": server_id,
                "attempt": attempt,
            }),
        )),
        GatewayEvent::McpResourceChanged { server_id, uri } => Some((
            "mcp.resource_changed".to_string(),
            serde_json::json!({
                "server_id": server_id,
                "uri": uri,
            }),
        )),
        GatewayEvent::McpAuthRequired { server_id, auth_url } => Some((
            "mcp.auth_required".to_string(),
            serde_json::json!({
                "server_id": server_id,
                "auth_url": auth_url,
            }),
        )),
        GatewayEvent::McpAuthComplete { server_id } => Some((
            "mcp.auth_complete".to_string(),
            serde_json::json!({
                "server_id": server_id,
            }),
        )),
        GatewayEvent::McpAuthFailed { server_id, reason } => Some((
            "mcp.auth_failed".to_string(),
            serde_json::json!({
                "server_id": server_id,
                "reason": reason,
            }),
        )),
        GatewayEvent::McpTokenRefreshed { server_id } => Some((
            "mcp.token_refreshed".to_string(),
            serde_json::json!({
                "server_id": server_id,
            }),
        )),
        GatewayEvent::ConnectorChanged { id, state, summary } => Some((
            match state.as_str() {
                "installed" => "connector.installed",
                "enabled" => "connector.enabled",
                "disabled" => "connector.disabled",
                "uninstalled" => "connector.uninstalled",
                "error" => "connector.error",
                _ => "connector.updated",
            }
            .to_string(),
            serde_json::json!({
                "id": id,
                "state": state,
                "summary": summary,
            }),
        )),
        GatewayEvent::DeviceStatusChanged { device_id, status, message } => Some((
            "device.status_changed".to_string(),
            serde_json::json!({
                "device_id": device_id,
                "status": status,
                "message": message,
            }),
        )),
        GatewayEvent::GoalProgress { goal_id, session_id, event } => Some((
            "goal.progress".to_string(),
            serde_json::json!({
                "goal_id": goal_id,
                "session_id": session_id,
                "event": event,
            }),
        )),
        GatewayEvent::AskRequired(e) => Some((
            "ask.required".to_string(),
            serde_json::json!({
                "ask_id": e.ask_id,
                "session_id": e.session_id,
                "question": e.question,
                "options": e.options,
                "required": e.required,
                "default": e.default,
            }),
        )),
        GatewayEvent::AskResolved(e) => Some((
            "ask.resolved".to_string(),
            serde_json::json!({
                "ask_id": e.ask_id,
                "cancelled": e.cancelled,
            }),
        )),
        GatewayEvent::AgentUsage { session_id, agent_id, usage } => Some((
            "agent.usage".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "agent_id": agent_id,
                "usage": {
                    "prompt_tokens": usage.prompt_tokens,
                    "completion_tokens": usage.completion_tokens,
                    "total_tokens": usage.total_tokens,
                    "cache_read_tokens": usage.cache_read_tokens,
                    "cache_creation_tokens": usage.cache_creation_tokens,
                },
            }),
        )),
        GatewayEvent::DelegationTaskUpdated { session_id, task } => Some((
            "delegation.updated".to_string(),
            serde_json::json!({
                "session_id": session_id,
                "task_id": task.task_id,
                "root_id": task.root_id,
                "parent_id": task.parent_id,
                "depth": task.depth,
                "agent_id": task.agent_id,
                "title": task.title,
                "status": task.status,
                "created_at": task.created_at,
                "updated_at": task.updated_at,
                "completed_at": task.completed_at,
                "usage_tokens": task.usage_tokens,
                "duration_ms": task.duration_ms,
            }),
        )),
    }
}

// ── Error Codes
// ───────────────────────────────────────────────────────────────

/// Build a standardized error response
pub fn error_unauthorized(id: impl Into<String>) -> WsResponse {
    WsResponse::err(id, "UNAUTHORIZED", "Invalid or missing authentication")
}

pub fn error_forbidden(id: impl Into<String>, missing: &str) -> WsResponse {
    WsResponse::err(id, "FORBIDDEN", format!("Missing required scope: {}", missing))
}

pub fn error_invalid_request(id: impl Into<String>, msg: impl Into<String>) -> WsResponse {
    WsResponse::err(id, "INVALID_REQUEST", msg)
}

pub fn error_method_not_found(id: impl Into<String>, method: &str) -> WsResponse {
    WsResponse::err(id, "METHOD_NOT_FOUND", format!("Unknown method: {}", method))
}

pub fn error_session_not_found(id: impl Into<String>) -> WsResponse {
    WsResponse::err(id, "SESSION_NOT_FOUND", "Session does not exist")
}

pub fn error_agent_not_found(id: impl Into<String>) -> WsResponse {
    WsResponse::err(id, "AGENT_NOT_FOUND", "Agent does not exist")
}

pub fn error_rate_limited(id: impl Into<String>) -> WsResponse {
    WsResponse::err(id, "RATE_LIMITED", "Too many requests")
}

pub fn error_internal(id: impl Into<String>, msg: impl Into<String>) -> WsResponse {
    WsResponse::err(id, "INTERNAL_ERROR", msg)
}

pub fn error_version_mismatch(id: impl Into<String>) -> WsResponse {
    WsResponse::err(id, "VERSION_MISMATCH", "Protocol version not supported")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scope table and the dispatcher's arms must describe one surface.
    ///
    /// They live in different files and nothing but this test connects them —
    /// which is how `subscribe`, `unsubscribe`, `subscribe_all` and
    /// `permissions.request_macos_accessibility` came to be dispatched while
    /// missing from the table, each silently demanding `admin` by falling
    /// through the default.
    #[test]
    fn method_table_matches_the_dispatcher() {
        use std::collections::BTreeSet;

        let source = include_str!("ws/core.rs");
        let start = source
            .find("match req.method.as_str() {")
            .expect("the dispatcher's method match");
        let end = start
            + source[start..]
                .find("_ => error_method_not_found")
                .expect("the dispatcher's default arm");
        let region = &source[start..end];

        // Same shape as the table's own source: pattern lines accumulate
        // until the arm's `=>`, so a multi-line alternative list is read
        // whole. Only lines that begin with a pattern are considered, so a
        // JSON string value inside a handler body cannot be mistaken for a
        // method.
        let mut dispatched: BTreeSet<String> = BTreeSet::new();
        let mut pending: Vec<String> = Vec::new();
        for raw in region.lines() {
            let line = raw.split("//").next().unwrap_or("").trim();
            if line.is_empty() || !(line.starts_with('"') || line.starts_with('|')) {
                continue;
            }
            let (head, has_arrow) = match line.split_once("=>") {
                Some((head, _)) => (head, true),
                None => (line, false),
            };
            for part in head.split('|') {
                let part = part.trim().trim_end_matches(',').trim();
                if part.len() > 2 && part.starts_with('"') && part.ends_with('"') {
                    pending.push(part.trim_matches('"').to_string());
                }
            }
            if has_arrow {
                dispatched.extend(pending.drain(..));
            }
        }
        assert!(
            !dispatched.is_empty(),
            "the dispatcher's arms were not parsed — this test would pass vacuously"
        );

        let listed: BTreeSet<String> = METHOD_SCOPES
            .iter()
            .map(|(method, _)| method.to_string())
            .collect();
        let unlisted: Vec<&String> = dispatched.difference(&listed).collect();
        let undispatched: Vec<&String> = listed.difference(&dispatched).collect();
        assert!(
            unlisted.is_empty(),
            "dispatched but missing from METHOD_SCOPES (they fall through to admin): {unlisted:?}"
        );
        assert!(
            undispatched.is_empty(),
            "in METHOD_SCOPES but never dispatched: {undispatched:?}"
        );
    }

    /// Every entry names a scope the protocol knows, and the pre-auth pair is
    /// the only scope-free one.
    #[test]
    fn method_table_entries_are_well_formed() {
        let known = [
            SCOPE_CHAT,
            SCOPE_READ,
            SCOPE_WRITE,
            SCOPE_ADMIN,
            SCOPE_PAIRING,
            SCOPE_ACP,
        ];
        for (method, scope) in METHOD_SCOPES {
            assert!(!method.is_empty(), "an empty method name");
            assert!(!method.contains(char::is_whitespace), "`{method}` contains whitespace");
            match scope {
                Some(s) => assert!(known.contains(s), "`{method}` names unknown scope `{s}`"),
                None => assert!(
                    matches!(*method, "connect" | "ping"),
                    "`{method}` needs no scope, but only the pre-auth pair should"
                ),
            }
        }
        // The list is a set: a duplicate would make lookup order-dependent.
        let mut seen = std::collections::BTreeSet::new();
        for (method, _) in METHOD_SCOPES {
            assert!(seen.insert(*method), "`{method}` is listed twice");
        }
    }

    #[test]
    fn test_ws_response_ok() {
        let res = WsResponse::ok("req_1", serde_json::json!({"status": "ok"}));
        assert!(res.ok);
        assert_eq!(res.id, "req_1");
        assert!(res.error.is_none());
    }

    /// A payload that cannot be serialized is reported and dropped, not
    /// silently nulled: from the caller's side `ok: true` with no payload in
    /// it is indistinguishable from a handler that returned nothing.
    #[test]
    fn test_unserializable_payload_is_dropped_but_not_silently() {
        /// A value that refuses to serialize, standing in for whatever a
        /// handler might return that serde cannot turn into JSON.
        struct Unserializable;

        impl serde::Serialize for Unserializable {
            fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("cannot be represented as JSON"))
            }
        }

        let res = WsResponse::ok("req_1", Unserializable);
        assert!(res.ok);
        assert!(res.payload.is_none());
        assert!(res.error.is_none());

        let event = WsEvent::new("bad.event", Unserializable, 7);
        assert_eq!(event.event, "bad.event");
        assert!(event.payload.is_none());
        assert_eq!(event.seq, Some(7));
    }

    #[test]
    fn test_ws_response_err() {
        let res = WsResponse::err("req_1", "TEST", "something failed");
        assert!(!res.ok);
        assert_eq!(res.id, "req_1");
        assert_eq!(res.error.as_ref().unwrap().code, "TEST");
    }

    #[test]
    fn test_scope_check() {
        let scopes = vec!["chat".to_string(), "read".to_string()];
        assert!(scopes_allow(&scopes, "chat.send"));
        assert!(scopes_allow(&scopes, "chat.history"));
        assert!(!scopes_allow(&scopes, "sessions.create"));

        let admin = vec!["admin".to_string()];
        assert!(scopes_allow(&admin, "anything.unknown"));
    }

    #[test]
    fn test_method_scope_mapping() {
        // SCOPE_CHAT
        assert_eq!(method_scope("chat.send"), Some(SCOPE_CHAT));
        assert_eq!(method_scope("chat.abort"), Some(SCOPE_CHAT));
        assert_eq!(method_scope("ask.respond"), Some(SCOPE_CHAT));
        assert_eq!(method_scope("feedback.vote"), Some(SCOPE_CHAT));

        // SCOPE_READ
        assert_eq!(method_scope("chat.history"), Some(SCOPE_READ));
        assert_eq!(method_scope("sessions.list"), Some(SCOPE_READ));
        assert_eq!(method_scope("agents.list"), Some(SCOPE_READ));
        assert_eq!(method_scope("agents.get"), Some(SCOPE_READ));
        assert_eq!(method_scope("agents.registry"), Some(SCOPE_READ));
        assert_eq!(method_scope("health"), Some(SCOPE_READ));
        assert_eq!(method_scope("system.presence"), Some(SCOPE_READ));
        assert_eq!(method_scope("commands.list"), Some(SCOPE_READ));
        assert_eq!(method_scope("config.get"), Some(SCOPE_READ));
        assert_eq!(method_scope("models.list"), Some(SCOPE_READ));
        assert_eq!(method_scope("models.presets"), Some(SCOPE_READ));
        assert_eq!(method_scope("models.default"), Some(SCOPE_READ));
        assert_eq!(method_scope("cost.get"), Some(SCOPE_READ));
        assert_eq!(method_scope("cron.list"), Some(SCOPE_READ));
        assert_eq!(method_scope("skills.list"), Some(SCOPE_READ));
        assert_eq!(method_scope("logs.subscribe"), Some(SCOPE_READ));
        assert_eq!(method_scope("logs.unsubscribe"), Some(SCOPE_READ));
        assert_eq!(method_scope("tasks.list"), Some(SCOPE_READ));
        assert_eq!(method_scope("mcp.list"), Some(SCOPE_READ));
        assert_eq!(method_scope("mcp.presets"), Some(SCOPE_READ));
        assert_eq!(method_scope("device.capabilities"), Some(SCOPE_READ));
        assert_eq!(method_scope("device.permission.status"), Some(SCOPE_READ));
        assert_eq!(method_scope("device.adb.status"), Some(SCOPE_READ));
        assert_eq!(method_scope("device.shortcut.results"), Some(SCOPE_READ));
        assert_eq!(method_scope("device.shortcut.inbox"), Some(SCOPE_READ));
        assert_eq!(method_scope("eval.trace.list"), Some(SCOPE_READ));
        assert_eq!(method_scope("eval.dashboard"), Some(SCOPE_READ));
        assert_eq!(method_scope("eval.optimizer.status"), Some(SCOPE_READ));
        assert_eq!(method_scope("eval.optimizer.run"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("eval.optimizer.resume"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("eval.optimizer.rollback"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("eval.propose"), Some(SCOPE_WRITE));

        // SCOPE_WRITE
        // Writes that used to be mislabelled as reads.
        for method in [
            "models.fetch_remote",
            "models.add",
            "models.remove",
            "models.set_default",
            "skills.install",
            "mcp.call_tool",
            "device.pairing.qr",
            "device.pairing.setup",
            "device.pairing.pending",
            "device.pairing.authorized",
        ] {
            assert_eq!(
                method_scope(method),
                Some(SCOPE_WRITE),
                "{method} mutates state or hands out credentials"
            );
        }
        assert_eq!(method_scope("cost.reset"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("agents.purge"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("agents.rename"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("sessions.create"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("sessions.delete"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("sessions.rename"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("sessions.set_pinned"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("sessions.reset"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("sessions.subscribe"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("sessions.unsubscribe"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("commands.execute"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("config.set"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("tasks.schedule"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("tasks.delete"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("tasks.enable"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("tasks.disable"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("mcp.add"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("mcp.remove"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("mcp.connect"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("mcp.disconnect"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("device.permission.request"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("device.adb.pair"), Some(SCOPE_WRITE));
        assert_eq!(method_scope("device.shortcut.run"), Some(SCOPE_WRITE));

        // SCOPE_ACP
        assert_eq!(method_scope("acp.spawn"), Some(SCOPE_ACP));
        assert_eq!(method_scope("acp.terminate"), Some(SCOPE_ACP));
        assert_eq!(method_scope("acp.message"), Some(SCOPE_ACP));
        assert_eq!(method_scope("acp.pause"), Some(SCOPE_ACP));
        assert_eq!(method_scope("acp.resume"), Some(SCOPE_ACP));
        assert_eq!(method_scope("acp.step"), Some(SCOPE_ACP));
        assert_eq!(method_scope("acp.cancel"), Some(SCOPE_ACP));
        assert_eq!(method_scope("acp.execute.session"), Some(SCOPE_ACP));
        assert_eq!(method_scope("acp.execute.run"), Some(SCOPE_ACP));

        // SCOPE_READ (acp read-only)
        assert_eq!(method_scope("acp.list"), Some(SCOPE_READ));
        assert_eq!(method_scope("acp.status"), Some(SCOPE_READ));
        assert_eq!(method_scope("acp.tree"), Some(SCOPE_READ));

        // No scope required
        assert_eq!(method_scope("connect"), None);
        assert_eq!(method_scope("ping"), None);

        // Default: admin scope
        assert_eq!(method_scope("unknown"), Some(SCOPE_ADMIN));
        assert_eq!(method_scope("mcp.delete"), Some(SCOPE_ADMIN));
        assert_eq!(method_scope("config.delete"), Some(SCOPE_ADMIN));
    }

    #[test]
    fn test_protocol_connection() {
        let mut conn = ProtocolConnection::new("conn_1");
        assert!(!conn.handshaked);
        assert!(conn.is_subscribed("any")); // empty subscriptions = all

        conn.subscriptions.push("s1".to_string());
        assert!(conn.is_subscribed("s1"));
        assert!(!conn.is_subscribed("s2"));

        assert_eq!(conn.next_seq(), 1);
        assert_eq!(conn.next_seq(), 2);
    }

    #[test]
    fn test_acp_event_mapping() {
        let event = crate::gateway::GatewayEvent::AcpSpawned {
            session_id: "s1".to_string(),
            subagent_id: "sub-1".to_string(),
            parent_id: "parent".to_string(),
            mode: "run".to_string(),
            thread_id: "thread-1".to_string(),
        };
        let (name, payload) = gateway_event_to_ws(&event).expect("mapped event");
        assert_eq!(name, "acp.spawned");
        assert_eq!(payload["subagent_id"], "sub-1");

        let event = crate::gateway::GatewayEvent::AcpStatusChanged {
            session_id: "s1".to_string(),
            runtime_state: "paused".to_string(),
        };
        let (name, _) = gateway_event_to_ws(&event).expect("mapped event");
        assert_eq!(name, "acp.status_changed");
    }

    #[test]
    fn test_ask_event_mapping() {
        use crate::tools::ask_user::{AskRequiredEvent, AskResolvedEvent};

        let required = crate::gateway::GatewayEvent::AskRequired(AskRequiredEvent {
            ask_id: "ask-1".into(),
            session_id: "s1".into(),
            question: "Proceed?".into(),
            options: vec!["yes".into(), "no".into()],
            required: true,
            default: Some("yes".into()),
        });
        let (name, payload) = gateway_event_to_ws(&required).expect("mapped event");
        assert_eq!(name, "ask.required");
        assert_eq!(payload["ask_id"], "ask-1");
        assert_eq!(payload["session_id"], "s1");
        assert_eq!(payload["question"], "Proceed?");
        assert_eq!(payload["options"], serde_json::json!(["yes", "no"]));
        assert_eq!(payload["required"], true);
        assert_eq!(payload["default"], "yes");

        let resolved = crate::gateway::GatewayEvent::AskResolved(AskResolvedEvent {
            ask_id: "ask-1".into(),
            session_id: "s1".into(),
            cancelled: true,
        });
        let (name, payload) = gateway_event_to_ws(&resolved).expect("mapped event");
        assert_eq!(name, "ask.resolved");
        assert_eq!(payload["ask_id"], "ask-1");
        assert_eq!(payload["cancelled"], true);
    }

    #[test]
    fn test_agent_usage_mapping() {
        let event = crate::gateway::GatewayEvent::AgentUsage {
            session_id: "s1".to_string(),
            agent_id: "secretary".to_string(),
            usage: crate::providers::Usage {
                prompt_tokens: 100,
                completion_tokens: 50,
                total_tokens: 150,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                x_credits_used: Some(5),
                x_credit_balance: Some(100),
            },
        };
        let (name, payload) = gateway_event_to_ws(&event).expect("mapped event");
        assert_eq!(name, "agent.usage");
        assert_eq!(payload["session_id"], "s1");
        assert_eq!(payload["agent_id"], "secretary");
        assert_eq!(payload["usage"]["total_tokens"], 150);
        assert_eq!(payload["usage"]["prompt_tokens"], 100);
        // Credits are metered on chat.final, not per round.
        assert!(payload["usage"].get("credits_used").is_none());
    }

    #[test]
    fn test_delegation_updated_mapping() {
        let snapshot = crate::delegation::DelegationTaskSnapshot {
            task_id: "run-1".to_string(),
            root_id: "root-1".to_string(),
            parent_id: None,
            depth: 1,
            agent_id: "researcher".to_string(),
            title: "scan docs".to_string(),
            status: "running".to_string(),
            created_at: "2026-01-01T00:00:00+00:00".to_string(),
            updated_at: "2026-01-01T00:01:00+00:00".to_string(),
            completed_at: None,
            usage_tokens: 3400,
            duration_ms: None,
        };
        let event = crate::gateway::GatewayEvent::DelegationTaskUpdated {
            session_id: "s1".to_string(),
            task: snapshot,
        };
        let (name, payload) = gateway_event_to_ws(&event).expect("mapped event");
        assert_eq!(name, "delegation.updated");
        assert_eq!(payload["session_id"], "s1", "the gate reads this field");
        assert_eq!(payload["task_id"], "run-1");
        assert_eq!(payload["agent_id"], "researcher");
        assert_eq!(payload["status"], "running");
        assert_eq!(payload["usage_tokens"], 3400);
        // A running row has no duration; the field is present and null.
        assert_eq!(payload["duration_ms"], serde_json::Value::Null);
        assert_eq!(payload["completed_at"], serde_json::Value::Null);
    }

    #[test]
    fn test_chat_final_usage_payload() {
        let with_usage = crate::gateway::GatewayEvent::Completed {
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
            response: "done".to_string(),
            turn_id: "t1".to_string(),
            usage: Some(crate::providers::Usage {
                total_tokens: 60,
                x_credits_used: Some(2),
                x_credit_balance: Some(98),
                ..Default::default()
            }),
        };
        let (name, payload) = gateway_event_to_ws(&with_usage).expect("mapped event");
        assert_eq!(name, "chat.final");
        assert_eq!(payload["usage"]["credits_used"], 2);
        assert_eq!(payload["usage"]["credit_balance"], 98);
        assert_eq!(payload["usage"]["total_tokens"], 60);

        // No usage → no `usage` key at all (unchanged wire shape).
        let no_usage = crate::gateway::GatewayEvent::Completed {
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
            response: "done".to_string(),
            turn_id: "t1".to_string(),
            usage: None,
        };
        let (_, payload) = gateway_event_to_ws(&no_usage).expect("mapped event");
        assert!(payload.get("usage").is_none());
    }

    #[test]
    fn test_chat_error_code_payload() {
        let coded = crate::gateway::GatewayEvent::ProcessingError {
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
            message: "insufficient credits".to_string(),
            code: Some("insufficient_credits".to_string()),
        };
        let (name, payload) = gateway_event_to_ws(&coded).expect("mapped event");
        assert_eq!(name, "chat.error");
        assert_eq!(payload["code"], "insufficient_credits");

        let plain = crate::gateway::GatewayEvent::ProcessingError {
            session_id: "s1".to_string(),
            agent_id: "a1".to_string(),
            message: "boom".to_string(),
            code: None,
        };
        let (_, payload) = gateway_event_to_ws(&plain).expect("mapped event");
        assert!(payload.get("code").is_none());
    }

    /// `resolve_scopes` is the whole authorization model in one function, so it
    /// gets its own tests.
    #[test]
    fn resolve_scopes_narrows_but_never_widens() {
        let entitled = vec!["chat".to_string(), "read".to_string()];

        // A client asking for more than it has keeps only what it has.
        let asked = vec!["chat".to_string(), "read".to_string(), "admin".to_string()];
        assert_eq!(resolve_scopes(&entitled, &asked), entitled);

        // Asking for a subset narrows.
        let asked = vec!["read".to_string()];
        assert_eq!(resolve_scopes(&entitled, &asked), vec!["read".to_string()]);

        // Asking for nothing means "everything I am entitled to".
        assert_eq!(resolve_scopes(&entitled, &[]), entitled);

        // Asking only for what it cannot have grants nothing.
        let asked = vec!["admin".to_string(), "pairing".to_string()];
        assert!(resolve_scopes(&entitled, &asked).is_empty());

        // The order of the result follows the entitlement, not the request.
        let asked = vec!["read".to_string(), "chat".to_string()];
        assert_eq!(resolve_scopes(&entitled, &asked), vec!["chat".to_string(), "read".to_string()]);
    }

    /// The dispatcher and the scope table are two hand-maintained lists that
    /// have to agree. They drifted once (writes labelled as reads, a method
    /// missing entirely), so the agreement is a test rather than a convention.
    #[test]
    fn every_dispatched_method_declares_a_scope() {
        // Methods that genuinely need no scope, because they are part of
        // establishing the connection itself.
        const UN_SCOPED: &[&str] = &["connect", "ping"];

        let dispatcher = include_str!("ws/core.rs");
        let mut checked = 0;
        for line in dispatcher.lines() {
            let Some(rest) = line.strip_prefix("        ") else {
                continue;
            };
            let Some(rest) = rest.strip_prefix('"') else {
                continue;
            };
            let Some((method, tail)) = rest.split_once('"') else {
                continue;
            };
            // Only dispatch arms: `"name" => …`.
            if !tail.trim_start().starts_with("=>") {
                continue;
            }
            checked += 1;
            if UN_SCOPED.contains(&method) {
                assert_eq!(
                    method_scope(method),
                    None,
                    "{method} is listed as unscoped but the table gives it a scope"
                );
                continue;
            }
            assert!(
                method_scope(method).is_some(),
                "`{method}` is dispatched but has no entry in `method_scope` — every method must \
                 declare a scope, or say why it needs none in UN_SCOPED"
            );
        }
        assert!(
            checked > 150,
            "the dispatcher parse found only {checked} methods — the \
            extraction is probably broken rather than the table being complete"
        );
    }
}
