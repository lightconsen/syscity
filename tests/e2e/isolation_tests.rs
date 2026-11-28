//! Per-principal isolation, driven over a real WebSocket against a real
//! gateway.
//!
//! The unit tests around `agents.list`, `sessions.list` and `agents.create`
//! call the handlers directly with a fabricated `RequestContext`. That proves
//! each handler's filter, but not that the *dispatcher* builds the context a
//! real client would get — a regression in `dispatch_method`'s identity
//! resolution would pass every one of those and still leak in production.
//!
//! Every ingredient here is real: identities come from stored auth sessions
//! (a session token names a principal and carries no agent binding, so the
//! connection's identity — not a per-agent credential — is what the listings
//! are filtered by), the sessions go through `sessions.create`, and the
//! listings are read back over `FrontendSimulator`. Only the model is a mock.

use super::*;
use syscity::gateway::protocol::AuthMode;
use syscity::security::UserId;

/// The agent token `alice` is also given, bound to `alice-bot`. Held alongside
/// her session token so a test can connect as the *bound* shape of client.
const ALICE_AGENT_TOKEN: &str = "journey-agent-token-alice";

/// Start a token-mode gateway with two registered people, one disk-backed
/// agent each plus a shared one, and return a session token per person plus
/// alice's agent access token.
///
/// The session tokens name a principal and carry no agent binding, so the
/// connection's identity — not a per-agent credential — is what the listings
/// are filtered by: that is the case the principal filter exists for. The
/// agent token is the other shape: it binds the connection to one agent, and
/// what it may reach beyond that (the shared agents) is what the bound
/// journeys check.
async fn start_isolation_gateway(port: u16) -> (String, String, String) {
    let mut config = test_config(port, false);
    config.model_provider = "mock".to_string();
    config.model = "mock-model".to_string();
    config.security.auth_mode = AuthMode::Token;
    config.security.shared_token = Some("journey-shared-token".to_string());
    config.security.shared_token_scopes = vec![
        "chat".to_string(),
        "read".to_string(),
        "write".to_string(),
        "admin".to_string(),
    ];
    // `bob-bot` is restricted to bob at the router; `alice-bot` to alice. The
    // shared agent has no restriction, so both see it.
    config.agent_overrides.insert(
        "alice-bot".to_string(),
        syscity::gateway::config::AgentOverrides {
            allowed_users: Some(vec!["alice".to_string()]),
            ..Default::default()
        },
    );
    config.agent_overrides.insert(
        "bob-bot".to_string(),
        syscity::gateway::config::AgentOverrides {
            allowed_users: Some(vec!["bob".to_string()]),
            ..Default::default()
        },
    );

    // Alice also holds an agent token for `alice-bot`. It is the same
    // `(principal, agent)` pair her `allowed_users` entry names, so the two
    // credentials reach the same agent; the difference is the binding.
    config.security.agent_tokens.insert(
        "alice".to_string(),
        syscity::gateway::config::AgentTokenEntry {
            token: ALICE_AGENT_TOKEN.to_string(),
            principal: String::new(),
            agent_id: "alice-bot".to_string(),
            scopes: syscity::gateway::config::default_agent_token_scopes(),
        },
    );

    let gateway = new_test_gateway(config).await;
    let router = gateway.model_router();
    register_mock_provider_with_model(&router, llm_mock_provider_for_streaming(), "mock-model")
        .await;

    let state = gateway.state();

    // Two people, each with a session token worth the scopes the journeys need.
    let scopes = vec![
        "chat".to_string(),
        "read".to_string(),
        "write".to_string(),
        "admin".to_string(),
    ];
    let mut tokens = Vec::new();
    for who in ["alice", "bob"] {
        let user_id = UserId::new(who);
        let mut user = syscity::security::User::new(who, who);
        user.scopes = scopes.clone();
        state
            .auth
            .manager
            .register_user(user)
            .await
            .expect("register user");
        let session = state
            .auth
            .manager
            .create_session(user_id, 1, Some(scopes.clone()))
            .await
            .expect("create session");
        tokens.push(session.token);
    }

    // Lay the three agents on disk, discover them, and spawn them *before*
    // start: `agents.list` reads the runtime map, so a registry-only agent
    // would be invisible and the filtering would look like a pass.
    for (id, name) in [
        ("alice-bot", "Alice Bot"),
        ("bob-bot", "Bob Bot"),
        ("team", "Team Bot"),
    ] {
        let dir = state.paths.agents_dir().join(id);
        if let Err(e) = tokio::fs::create_dir_all(&dir).await {
            panic!("create agent dir {dir:?}: {e}");
        }
        tokio::fs::write(dir.join("IDENTITY.md"), format!("# {name}\n\n## name\n{name}\n"))
            .await
            .expect("write IDENTITY.md");
        tokio::fs::write(dir.join("SOUL.md"), "---\nemoji: \"🤖\"\n---\n")
            .await
            .expect("write SOUL.md");
    }
    state
        .agents
        .registry
        .write()
        .await
        .discover(&state.paths)
        .await
        .expect("registry discovery");
    let spawned = gateway
        .spawn_all_discovered_agents()
        .await
        .expect("spawn discovered agents");
    assert_eq!(spawned, 3, "all three agents must be running for the list to filter");

    tokio::spawn(async move {
        let _ = gateway.start().await;
    });

    // `start_gateway_and_wait` probes readiness by opening a WebSocket, which
    // under token mode is a 401 forever. The unauthenticated `/live` probe is
    // the one that answers here.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            tokio::time::Instant::now() < deadline,
            "gateway never answered /live on port {port}"
        );
        if let Ok(resp) = reqwest::get(format!("http://127.0.0.1:{port}/live")).await {
            if resp.status().is_success() {
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    (tokens[0].clone(), tokens[1].clone(), ALICE_AGENT_TOKEN.to_string())
}

/// Connect as the holder of `session_token`: mint an upgrade ticket over HTTP
/// with it, then upgrade with the ticket.
async fn connect_as(port: u16, session_token: &str) -> FrontendSimulator {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v1/ws-ticket"))
        .bearer_auth(session_token)
        .send()
        .await
        .expect("ticket request");
    assert!(resp.status().is_success(), "ticket mint failed: {}", resp.status());
    let body: serde_json::Value = resp.json().await.expect("ticket body");
    let ticket = body["ticket"].as_str().expect("ticket in body");
    FrontendSimulator::connect_with_ticket(port, ticket).await
}

/// Connect with an agent access token: mint a ticket over HTTP with it, then
/// upgrade with the ticket. The resulting connection is bound to that agent
/// (the ticket carries the binding, so the upgrade cannot widen it).
async fn connect_as_agent(port: u16, agent_token: &str) -> FrontendSimulator {
    let client = reqwest::Client::new();
    let resp = client
        .post(format!("http://127.0.0.1:{port}/api/v1/ws-ticket"))
        .bearer_auth(agent_token)
        .send()
        .await
        .expect("ticket request");
    assert!(resp.status().is_success(), "ticket mint failed: {}", resp.status());
    let body: serde_json::Value = resp.json().await.expect("ticket body");
    let ticket = body["ticket"].as_str().expect("ticket in body");
    FrontendSimulator::connect_with_ticket(port, ticket).await
}

/// Case 0 — an agent token's connection is bound to its one agent for private
/// agents, but the deployment's **shared** agents stay reachable: a team
/// member's Desktop must be able to talk to the team secretary. A *personal*
/// agent belonging to somebody else stays out.
#[tokio::test]
#[serial]
async fn a_bound_connection_sees_its_own_agent_and_the_shared_ones() {
    let port = free_port();
    let (_, _, alice_agent_token) = start_isolation_gateway(port).await;

    let mut bound = connect_as_agent(port, &alice_agent_token).await;

    // The handshake says which agent this is.
    let info = bound.request("ping", json!({})).await;
    assert_eq!(info["ok"], true, "{info}");

    let listed = bound.request("agents.list", json!({})).await;
    assert_eq!(listed["ok"], true, "{listed}");
    let mut ids: Vec<String> = listed["payload"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    ids.sort();
    assert_eq!(
        ids,
        vec!["alice-bot", "default", "team"],
        "bound to alice-bot: its own agent plus the shared ones, never bob's: {listed}"
    );

    // `agents.registry` is the surface the sidebar reads; the shared entries
    // must be there, marked read-only so the client does not offer a refused
    // rename.
    let registry = bound.request("agents.registry", json!({})).await;
    assert_eq!(registry["ok"], true, "{registry}");
    let entries = registry["payload"]["agents"].as_array().unwrap().clone();
    let by_id: std::collections::HashMap<String, &serde_json::Value> = entries
        .iter()
        .map(|e| (e["id"].as_str().unwrap_or_default().to_string(), e))
        .collect();
    assert!(by_id.contains_key("team"), "the shared agent must be listed: {registry}");
    assert_eq!(
        by_id["team"]["read_only"], true,
        "a shared agent is not this client's to reconfigure: {registry}"
    );
    assert!(
        by_id["alice-bot"].get("read_only").is_none(),
        "its own agent is the one it may reconfigure: {registry}"
    );

    // A shared agent's workspace is readable, and its conversation can be
    // opened — the two things a sidebar entry implies.
    let opened = bound
        .request("sessions.create", json!({ "session_id": "shared-bot", "agent_id": "team" }))
        .await;
    assert_eq!(opened["ok"], true, "a shared agent must accept a session: {opened}");
    let shared_ws = bound
        .request("workspace.list", json!({ "agent_id": "team" }))
        .await;
    assert_eq!(shared_ws["ok"], true, "a shared workspace must be readable: {shared_ws}");

    // Bob's private agent stays out of both the listing and reach.
    let personal = bound
        .request("agents.get", json!({ "id": "bob-bot" }))
        .await;
    assert_eq!(personal["ok"], false, "bob's agent must refuse: {personal}");
    assert_eq!(personal["error"]["code"], "FORBIDDEN", "{personal}");
    let hijack = bound
        .request("sessions.create", json!({ "session_id": "hijack", "agent_id": "bob-bot" }))
        .await;
    assert_eq!(hijack["ok"], false, "bob's agent must refuse a session: {hijack}");
}

/// Case 1 — two principals see different agent lists over the same gateway,
/// and the shared agents are in both.
///
/// `default` is in the expectation because every gateway spawns one; it carries
/// no `allowed_users`, so it is unrestricted and belongs to both lists.
#[tokio::test]
#[serial]
async fn two_principals_see_different_agent_lists() {
    let port = free_port();
    let (alice_token, bob_token, _) = start_isolation_gateway(port).await;

    let mut alice = connect_as(port, &alice_token).await;
    let alice_frame = alice.request("agents.list", json!({})).await;
    assert_eq!(alice_frame["ok"], true, "{alice_frame}");
    let mut alice_ids: Vec<String> = alice_frame["payload"]["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    alice_ids.sort();
    assert_eq!(
        alice_ids,
        vec!["alice-bot", "default", "team"],
        "alice must not be offered bob's agent: {alice_frame}"
    );

    let mut bob = connect_as(port, &bob_token).await;
    let bob_frame = bob.request("agents.list", json!({})).await;
    assert_eq!(bob_frame["ok"], true, "{bob_frame}");
    let mut bob_ids: Vec<String> = bob_frame["payload"]["agents"]
        .as_array()
        .expect("agents array")
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    bob_ids.sort();
    assert_eq!(
        bob_ids,
        vec!["bob-bot", "default", "team"],
        "bob must not be offered alice's agent: {bob_frame}"
    );
}

/// Case 2 — `agents.registry` agrees with `agents.list` on the same gateway.
/// Two sources of truth for "which agents exist" is how a sidebar and an
/// action list start disagreeing.
#[tokio::test]
#[serial]
async fn registry_and_list_agree_on_what_a_principal_may_see() {
    let port = free_port();
    let (alice_token, _, _) = start_isolation_gateway(port).await;

    let mut alice = connect_as(port, &alice_token).await;
    let listed = alice.request("agents.list", json!({})).await;
    let registry = alice.request("agents.registry", json!({})).await;
    assert_eq!(registry["ok"], true, "{registry}");

    let mut from_list: Vec<String> = listed["payload"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let mut from_registry: Vec<String> = registry["payload"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_str().map(str::to_string))
        .collect();
    from_list.sort();
    from_registry.sort();
    assert_eq!(
        from_list, from_registry,
        "the two agent surfaces must agree: list={from_list:?} registry={from_registry:?}"
    );
}

/// Case 3 — a session is private to the person who created it, end to end:
/// `sessions.create` records the principal, `sessions.list` finds it for them,
/// and the other person's list does not.
#[tokio::test]
#[serial]
async fn a_session_is_visible_only_to_its_owner() {
    let port = free_port();
    let (alice_token, bob_token, _) = start_isolation_gateway(port).await;

    let mut alice = connect_as(port, &alice_token).await;
    let created = alice
        .request("sessions.create", json!({ "session_id": "alice-private" }))
        .await;
    assert_eq!(created["ok"], true, "{created}");

    // Alice sees her own.
    let listed = alice.request("sessions.list", json!({})).await;
    assert_eq!(listed["ok"], true, "{listed}");
    let ids: Vec<String> = listed["payload"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["session_id"].as_str().map(str::to_string))
        .collect();
    assert!(
        ids.contains(&"alice-private".to_string()),
        "alice must see her own session: {ids:?}"
    );

    // Bob does not.
    let mut bob = connect_as(port, &bob_token).await;
    let bob_list = bob.request("sessions.list", json!({})).await;
    assert_eq!(bob_list["ok"], true, "{bob_list}");
    let bob_ids: Vec<String> = bob_list["payload"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["session_id"].as_str().map(str::to_string))
        .collect();
    assert!(
        !bob_ids.contains(&"alice-private".to_string()),
        "bob must not see alice's session: {bob_ids:?}"
    );

    // …and the list is not the only thing that respects the boundary: naming
    // the id directly is refused by the dispatcher, for reads as well as
    // writes. A hidden row that still answers to its id is not isolation.
    for (method, extra) in [
        ("chat.history", json!({})),
        ("sessions.rename", json!({ "name": "pwned" })),
        ("sessions.set_pinned", json!({ "pinned": true })),
        ("sessions.delete", json!({})),
    ] {
        let mut params = json!({ "session_id": "alice-private" });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                params[k] = v.clone();
            }
        }
        let refused = bob.request(method, params).await;
        assert_eq!(refused["ok"], false, "{method} must refuse alice's session to bob: {refused}");
        assert_eq!(refused["error"]["code"], "FORBIDDEN", "{method}: {refused}");
    }

    // The live event stream is the same promise in plural: subscribing is how a
    // client receives every token of a turn, so a single missed spelling here
    // would hand bob alice's conversation in real time.
    let subscribed = bob
        .request("sessions.subscribe", json!({ "session_ids": ["alice-private"] }))
        .await;
    assert_eq!(
        subscribed["ok"], false,
        "subscribing to alice's session must be refused: {subscribed}"
    );
    assert_eq!(subscribed["error"]["code"], "FORBIDDEN", "{subscribed}");

    // The refusals were refusals, not a quiet rename: alice's session is intact
    // and still hers.
    let still_there = alice.request("sessions.list", json!({})).await;
    let ids: Vec<String> = still_there["payload"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["session_id"].as_str().map(str::to_string))
        .collect();
    assert!(
        ids.contains(&"alice-private".to_string()),
        "bob's refusals must not have changed alice's session: {ids:?}"
    );
}

/// Case 5 — the listing hides bob's agent, and so does every handler that
/// takes an agent id. A hidden name that still answers when typed is not
/// isolation; it is a hint.
#[tokio::test]
#[serial]
async fn a_hidden_agent_is_refused_by_name_too() {
    let port = free_port();
    let (alice_token, _, _) = start_isolation_gateway(port).await;

    let mut alice = connect_as(port, &alice_token).await;

    for (method, extra) in [
        ("agents.get", json!({})),
        ("agents.get_config", json!({})),
        ("agents.memory.get", json!({})),
        ("agents.export", json!({})),
    ] {
        let mut params = json!({ "agent_id": "bob-bot" });
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                params[k] = v.clone();
            }
        }
        let frame = alice.request(method, params).await;
        assert_eq!(frame["ok"], false, "{method} must refuse bob's agent: {frame}");
        assert_eq!(frame["error"]["code"], "FORBIDDEN", "{method}: {frame}");
    }

    // The agent she *may* use still answers, so the gate is the principal and
    // not a blanket refusal of the method.
    let own = alice
        .request("agents.get_config", json!({ "agent_id": "alice-bot" }))
        .await;
    assert_eq!(own["ok"], true, "alice must still reach her own agent: {own}");

    let shared = alice
        .request("agents.memory.get", json!({ "agent_id": "team" }))
        .await;
    assert_eq!(shared["ok"], true, "the shared agent stays readable: {shared}");
}

/// Case 6 — an agent write-scoped handler refuses by the same gate, and the
/// refusal comes before the write.
#[tokio::test]
#[serial]
async fn a_write_handler_refuses_before_touching_bob_s_agent() {
    let port = free_port();
    let (alice_token, _, _) = start_isolation_gateway(port).await;

    let mut alice = connect_as(port, &alice_token).await;
    let frame = alice
        .request("agents.memory.clear", json!({ "agent_id": "bob-bot" }))
        .await;
    assert_eq!(frame["ok"], false, "{frame}");
    assert_eq!(frame["error"]["code"], "FORBIDDEN", "{frame}");
}

/// Case 7 — the destructive handlers refuse by the same gate. They are not a
/// read surface, so an id alice cannot *use* is also one she cannot unload,
/// rename or purge; `agents.purge` is the worst of them, since it deletes the
/// personality and its files for good.
#[tokio::test]
#[serial]
async fn the_destructive_handlers_refuse_bob_s_agent() {
    let port = free_port();
    let (alice_token, bob_token, _) = start_isolation_gateway(port).await;

    let mut alice = connect_as(port, &alice_token).await;
    // `agents.rename` names the agent `agent_id`; the other two use `id`.
    for (method, key, extra) in [
        ("agents.delete", "id", json!({})),
        ("agents.purge", "id", json!({})),
        ("agents.rename", "agent_id", json!({ "display_name": "pwned" })),
    ] {
        let mut params = json!({});
        params[key] = json!("bob-bot");
        if let Some(obj) = extra.as_object() {
            for (k, v) in obj {
                params[k] = v.clone();
            }
        }
        let frame = alice.request(method, params).await;
        assert_eq!(frame["ok"], false, "{method} must refuse bob's agent: {frame}");
        assert_eq!(frame["error"]["code"], "FORBIDDEN", "{method}: {frame}");
    }

    // Refused, not quietly deleted: bob's agent is still spawned and still
    // listed to him.
    let mut bob = connect_as(port, &bob_token).await;
    let listed = bob.request("agents.list", json!({})).await;
    let ids: Vec<String> = listed["payload"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    assert!(
        ids.contains(&"bob-bot".to_string()),
        "alice's refusals must not have removed bob's agent: {ids:?}"
    );
}

/// Case 4 — the agent an agent-created session names is one the caller may
/// actually use: creating one for an agent the router restricts to someone
/// else is refused, and refused before anything is written.
#[tokio::test]
#[serial]
async fn a_session_cannot_be_created_for_another_persons_agent() {
    let port = free_port();
    let (alice_token, _, _) = start_isolation_gateway(port).await;

    let mut alice = connect_as(port, &alice_token).await;
    let refused = alice
        .request("sessions.create", json!({ "session_id": "hijack", "agent_id": "bob-bot" }))
        .await;
    assert_eq!(
        refused["ok"], false,
        "creating a session on bob's agent must be refused: {refused}"
    );

    // …and nothing was left behind for it.
    let listed = alice.request("sessions.list", json!({})).await;
    let ids: Vec<String> = listed["payload"]["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["session_id"].as_str().map(str::to_string))
        .collect();
    assert!(
        !ids.contains(&"hijack".to_string()),
        "a refused session must not appear in the list: {ids:?}"
    );
}
