# Channels — Setup & Configuration Guide

This guide covers the **operator-facing** setup for each messaging channel:
what is supported, what credentials to provide, and how to wire the platform
side. For the internal architecture (the `Channel` trait, inbound/outbound
pipelines, provenance), see [`docs/modules/channels.md`](modules/channels.md).

> **Data flow.** A connected channel is a bridge to that platform, and the bridge
> carries content both ways: inbound messages arrive from the platform's servers
> and your replies go back through them, so what you and your users exchange over
> the channel is visible to that platform exactly as it would be in that
> platform's own client. This is what a channel *is* — not a leak — but it is the
> one thing to keep in mind when deciding what to connect: `README.md`'s "your
> data, memory, and tools stay on your machine" describes the runtime, and names
> the provider, channels and MCP servers you configure as the places data
> leaves for.

## Supported channels

| Channel | Feature | Default | Direction | Webhook path |
|---|---|---|---|---|
| Telegram | `telegram` | ✅ | Bot polling / webhook | `/webhooks/telegram/:token` |
| Discord | `discord` | ✅ | Gateway | — |
| Slack | `slack` | ✅ | Events API | `/webhooks/slack` |
| WhatsApp | `whatsapp` | ✅ | Meta Cloud API webhook | `/webhooks/whatsapp` (+ `/verify`) |
| QQ | `qq` | ✅ | go-cqhttp / webhook | — |
| Feishu / Lark | `feishu` | ✅ | Event webhook | `/webhooks/feishu` |
| **WeChat MP (公众号)** | **`wechatmp`** | ✅ | Encrypted webhook | `/webhooks/wechatmp` |
| **LINE** | **`line`** | ✅ | Messaging API webhook | `/webhooks/line` |
| **Twilio SMS** | **`twilio`** | ✅ | Messages webhook (form-encoded) | `/webhooks/twilio` |
| **Google Chat** | **`googlechat`** | ✅ | Chat app webhook (bearer token) | `/webhooks/googlechat` |
| **Microsoft Teams** | **`teams`** | ✅ | Bot Framework activity (bearer token) | `/webhooks/teams` |
| **WeCom (企业微信)** | **`wecom`** | ✅ | AI Bot WebSocket (long-lived) | — |
| **DingTalk (钉钉)** | **`dingtalk`** | ✅ | Stream Mode (long-lived) | — |
| Signal | `signal` | ✅ | signal-cli daemon | — |
| iMessage | `imessage` | ✅ | BlueBubbles (macOS) | — |
| WebChat | `webchat` | ✅ | Browser UI | — |

Channels are compiled in behind Cargo features. The default feature set enables
all of the above; builds can prune them with `--no-default-features --features <subset>`.

## Configuring a channel

Two equivalent ways — the CLI is a wrapper that writes the same
`config.toml` `[channels.<name>]` section (and mirrors secrets into the secret
store).

### 1. CLI

```bash
syscity channels add <type> [--token <primary>] [--cred key=value ...]
```

Examples:

```bash
syscity channels add telegram --token 123456:ABC...            # TELEGRAM_BOT_TOKEN
syscity channels add feishu --token <app_id> --cred app_secret=xxx
syscity channels add wechatmp --token <app_id> --cred app_secret=xxx --cred token=xxx --cred encoding_aes_key=xxx
```

### 2. config.toml

```toml
[channels.wechatmp]
channel_type = "wechatmp"
enabled = true
dm_policy = "open"        # open | pairing | allowlist
require_mention = false
agent_id = "default"

[channels.wechatmp.credentials]
app_id = "..."
app_secret = "..."        # plaintext here is legacy; the secret store is preferred
token = "..."
encoding_aes_key = "..."
```

Per-channel credential keys:

| Channel | `credentials` keys | Fallback env vars |
|---|---|---|
| Telegram | `bot_token` | `TELEGRAM_BOT_TOKEN` |
| Discord | `bot_token` | `DISCORD_BOT_TOKEN` |
| Slack | `bot_token`, `signing_secret` | `SLACK_BOT_TOKEN`, `SLACK_SIGNING_SECRET` |
| WhatsApp | `access_token`, `phone_number_id`, `webhook_verify_token` | `WHATSAPP_*` |
| QQ | `app_id`, `app_secret`, `access_token`, `bot_qq` | `QQ_*` |
| Feishu/Lark | `app_id`, `app_secret`, `verification_token`, `encrypt_key` | `LARK_*` / `FEISHU_*` |
| WeChat MP | `app_id`, `app_secret`, `token`, `encoding_aes_key` | `WECHAT_MP_*` |
| LINE | `channel_access_token`, `channel_secret` | `LINE_CHANNEL_ACCESS_TOKEN`, `LINE_CHANNEL_SECRET` |
| Twilio SMS | `account_sid`, `auth_token`, `from_number`, `webhook_url` | `TWILIO_ACCOUNT_SID`, `TWILIO_AUTH_TOKEN`, `TWILIO_FROM_NUMBER`, `TWILIO_WEBHOOK_URL` |
| Google Chat | `issuer`, `audience`, `jwks_url`, `allowed_service_accounts`, `client_email`, `private_key_pem` | `GOOGLE_CHAT_ISSUER`, `GOOGLE_CHAT_AUDIENCE`, `GOOGLE_CHAT_JWKS_URL`, `GOOGLE_CHAT_ALLOWED_SERVICE_ACCOUNTS`, `GOOGLE_CHAT_CLIENT_EMAIL`, `GOOGLE_CHAT_PRIVATE_KEY` |
| Microsoft Teams | `app_id`, `tenant_id`, `client_secret`, `issuer`, `jwks_url` | `TEAMS_APP_ID`, `TEAMS_TENANT_ID`, `TEAMS_CLIENT_SECRET`, `TEAMS_ISSUER`, `TEAMS_JWKS_URL` |
| WeCom (企业微信) | `bot_id`, `secret`, `ws_url` | `WECOM_BOT_ID`, `WECOM_SECRET`, `WECOM_WEBSOCKET_URL` |
| DingTalk (钉钉) | `client_id`, `client_secret`, `openapi_endpoint` | `DINGTALK_CLIENT_ID`, `DINGTALK_CLIENT_SECRET`, `DINGTALK_OPENAPI_ENDPOINT` |
| Signal | `account` | `SIGNAL_ACCOUNT` |
| iMessage | — | (BlueBubbles on macOS) |

Secrets should be provided through the environment or the secret store;
plaintext `credentials` entries remain for backward compatibility.

## Webhook-based channels

For webhook channels you must expose a **public HTTPS** endpoint that points at
the gateway's webhook router. During local development use a tunnel (ngrok /
cloudflared) to forward e.g. `https://<tunnel>/webhooks/wechatmp` → `http://127.0.0.1:PORT`.

### WeChat MP (公众号) — the full flow

1. Register an Official Account at [mp.weixin.qq.com](https://mp.weixin.qq.com).
2. In **Settings → Development → Basic Config**, configure:
   - **URL**: `https://<public>/webhooks/wechatmp`
   - **Token**: a random string (saved as the channel's `token` credential)
   - **EncodingAESKey**: the generated 43-char key (saved as `encoding_aes_key`)
   - **Message encryption**: **Safety mode** (encrypted) — the only mode this
     channel supports.
3. Configure the channel:
   ```bash
   export WECHAT_MP_APP_ID=wx...
   export WECHAT_MP_APP_SECRET=...
   export WECHAT_MP_TOKEN=<token>
   export WECHAT_MP_ENCODING_AES_KEY=<43-char key>
   syscity channels add wechatmp
   ```
   Then `syscity start` to run the gateway.
4. Verify: WeChat issues a GET to your URL; the gateway verifies the `echostr`
   signature and returns it. Messages the user sends arrive encrypted, are
   decrypted, and are routed to the agent; replies go out asynchronously via
   the customer-service message API (WeChat's 5-second passive-reply window
   does not apply to LLM turns).

### LINE — the full flow

1. Create a Messaging API channel in the
   [LINE Developers console](https://developers.line.biz/console/).
2. Take the **channel secret** from the channel's *Basic settings*, and issue a
   **channel access token** under *Messaging API*. They are two different
   credentials: the secret verifies what LINE sends you, the token authorises
   what you send to LINE.
3. Configure the channel:
   ```bash
   export LINE_CHANNEL_ACCESS_TOKEN=<channel access token>
   export LINE_CHANNEL_SECRET=<channel secret>
   syscity channels add line
   ```
   Then `syscity start` to run the gateway.
4. Under *Messaging API → Webhook URL* set `https://<public>/webhooks/line` and
   press **Verify**. LINE signs the raw body with the channel secret and sends
   `X-Line-Signature: base64(HMAC-SHA256(secret, body))`; a delivery whose
   signature does not match is refused with `401`.
5. Replies go out through `message/push` rather than `message/reply`: a reply
   token is only valid for the delivery that carried it and expires within a
   minute, which an agent turn routinely outlives.

Only text messages are routed. LINE reports follows, joins, leaves and
postbacks through the same webhook; those are acknowledged and skipped.

### Twilio SMS — the full flow

1. In the [Twilio console](https://console.twilio.com/), take the **Account SID**
   and **Auth Token** from the account dashboard, and note the Twilio phone
   number replies should come from.
2. Configure the channel:
   ```bash
   export TWILIO_ACCOUNT_SID=AC...
   export TWILIO_AUTH_TOKEN=...
   export TWILIO_FROM_NUMBER=+15551234567
   export TWILIO_WEBHOOK_URL=https://<public>/webhooks/twilio
   syscity channels add twilio
   ```
   Then `syscity start` to run the gateway.
3. Under the phone number's **Messaging → A message comes in**, set the webhook
   to `https://<public>/webhooks/twilio`, method **HTTP POST**.

`TWILIO_WEBHOOK_URL` is not optional, and it must be exactly the URL you entered
in the console. Twilio signs the URL it dialled plus the request's parameters
(`X-Twilio-Signature`, HMAC-SHA1, base64); behind a tunnel or a proxy the
gateway sees `127.0.0.1:<port>` while Twilio signed `https://…`, so the public
URL cannot be recovered from the request. Without it a delivery cannot be
authenticated and is refused — there is deliberately no "skip validation"
switch. Both spellings of the scheme's default port are accepted, since Twilio
has signed URLs with and without it.

Replies longer than 1600 characters are split into consecutive texts, on a word
boundary where there is one (SMS has no editing, threading or reply-to, so
consecutive texts are the closest thing to one reply the medium has). Status
callbacks — which Twilio posts to the same URL for messages it has already
handled — carry no body and are acknowledged without becoming a turn.

### Google Chat — the full flow

1. Create a Chat app in the [Google Cloud console](https://console.cloud.google.com/)
   and take its **service account** address and private key (the JSON key file's
   `client_email` and `private_key`).
2. Under the app's connection settings, note the **Authentication Audience**
   setting — it decides three of the values below, and getting them wrong means
   every delivery is refused:
   - **HTTP endpoint URL** — the token is an OIDC ID token with
     `iss = chat@system.gserviceaccount.com` and `aud` = your endpoint URL; keys
     are published at
     `https://www.googleapis.com/service_accounts/v1/metadata/jwk/chat@system.gserviceaccount.com`.
   - **Project number** — the token is signed by the same service account with
     `aud` = your GCP project number.
3. Configure the channel:
   ```bash
   export GOOGLE_CHAT_ISSUER=chat@system.gserviceaccount.com
   export GOOGLE_CHAT_AUDIENCE=<your endpoint URL or project number>
   export GOOGLE_CHAT_JWKS_URL=<the key URL matching that mode>
   export GOOGLE_CHAT_ALLOWED_SERVICE_ACCOUNTS=chat@system.gserviceaccount.com
   export GOOGLE_CHAT_CLIENT_EMAIL=bot@project.iam.gserviceaccount.com
   export GOOGLE_CHAT_PRIVATE_KEY="$(cat service-account-key.pem)"
   syscity channels add googlechat
   ```
   Then `syscity start` to run the gateway.
4. Point the app's connection settings at `https://<public>/webhooks/googlechat`.

`allowed_service_accounts` is not optional and has no default: Google signs these
tokens for **its own** service accounts, so a valid signature proves Google issued
the token — not that it was issued for your app. The `email` claim is checked
against that list, and an empty list refuses everything. `issuer`, `audience` and
`jwks_url` are required for the same reason: they depend on the mode above, and a
guessed default would reject every delivery.

Only user messages are routed. A bot's own reply comes back through the same
webhook — answering it would make the bot talk to itself — and Chat posts
non-message events (adds, removals) to the same URL; both are acknowledged and
skipped. Replies over 4096 characters are split into consecutive messages.

### Microsoft Teams — the full flow

1. Register a bot in the [Azure Bot Service](https://portal.azure.com/) and note
   three things: the **Microsoft App ID**, an app **secret** for it, and the
   **tenant** it lives in.
2. Configure the channel:
   ```bash
   export TEAMS_APP_ID=<the app id>
   export TEAMS_TENANT_ID=<the tenant id>
   export TEAMS_CLIENT_SECRET=<the app secret>
   syscity channels add teams
   ```
   Then `syscity start` to run the gateway.
3. Set the bot's **messaging endpoint** to `https://<public>/webhooks/teams`.

`TEAMS_ISSUER` and `TEAMS_JWKS_URL` default to the public cloud's values
(`https://api.botframework.com` and
`https://login.botframework.com/v1/.well-known/keys`) and need setting only for a
sovereign cloud.

Two checks stand between a delivery and a turn, and they answer different
questions. The token has to verify against the issuer's published keys **and**
name your app as its audience — that is what says Microsoft minted it for this
bot. Then its `serviceurl` claim — one word, lower case; `serviceUrl` is only the
activity field's spelling — has to match the activity's, which is what says it
was minted for *this* conversation surface. A token Microsoft legitimately
signed for another tenant's connector passes the first check and fails the
second, which is why both are there.

The reply address is two values, not one. A Bot Framework reply goes to
`{serviceUrl}v3/conversations/{conversationId}/activities`, and the service URL is
per-tenant — the regions differ by path (`/amer/`, `/emea/`, `/apac/`) — so both
travel together in the conversation id, joined by `|`. The service URL is held to
an allowlist of Bot Framework hosts before a freshly minted token is attached to
it, because that host is what decides where this bot's credentials go. The
allowlist covers the commercial and government clouds; a sovereign deployment on
another host is not supported yet.

Only messages from people are routed. Teams posts conversation updates,
reactions, invokes and typing to the same URL, and the bot's own activity would
loop if it were answered — all of them are acknowledged and skipped. The
`<at>…</at>` markup Teams wraps a mention in is removed, but the name inside it is
kept, so `<at>Alice</at> can you check this` reaches the agent as
`Alice can you check this`. Replies over 28,000 characters are split into
consecutive messages.

## Connection-based channels

These channels need no public endpoint: the gateway **dials out** and holds the
connection open, so there is nothing to expose and nothing to point at the
gateway.

### WeCom (企业微信) — the full flow

1. In the WeCom admin console, create an **AI Bot**: *Workspace → Smart Robot →
   Create smart robots*, and select **API Mode**.
2. Copy the **Bot ID** and **Secret** it shows you.
3. Configure the channel:
   ```bash
   export WECOM_BOT_ID=<the bot id>
   export WECOM_SECRET=<the secret>
   syscity channels add wecom
   ```
   Then `syscity start` to run the gateway. Nothing else to point anywhere — the
   gateway connects out to `wss://openws.work.weixin.qq.com` itself.

`WECOM_WEBSOCKET_URL` only needs setting for a sovereign deployment or a test
peer; the public gateway is the default.

**A reply is a frame that echoes the message it answers.** Every inbound message
carries a `req_id`, and answering means sending it back — which is what puts the
answer in the thread the question came from. That window closes, so the channel
remembers the last `req_id` per chat and, when it has expired, falls back to
starting a message instead. **A group has no such fallback**: an AI Bot cannot
start a conversation in a group, so a reply whose window has closed is refused
with that reason rather than sent into the void. The same rule means the channel
can only answer a conversation it has heard from — a chat that has sent nothing
has nowhere to be replied to.

The gateway allows **one connection per bot**, and a reconnect is a fresh
authentication rather than a resumption, so the channel reconnects with a
backoff of 2/5/10/30/60 seconds and re-subscribes. If another connection takes
the bot's place the channel stands down rather than fighting it for the socket,
since reconnecting would only kick the newcomer back.

Only **text** messages are routed. WeCom delivers images, files and voice notes
over the same socket, and those carry no text this channel can act on, so they
are acknowledged and skipped. Replies over 4000 characters go out as consecutive
messages. The typing bubble on this platform is the first frame of a *streamed*
reply, which this channel does not send — so there is no typing indicator, and a
long answer arrives when it is finished rather than growing in place.

### DingTalk (钉钉) — the full flow

1. At [open-dev.dingtalk.com](https://open-dev.dingtalk.com/), create an app and
   take its **Client ID** (the AppKey) and **Client Secret** (the AppSecret).
2. Under **Add Capability → Robot**, enable the robot, and set its **Message
   Reception Mode** to **Stream Mode**.
3. Configure the channel:
   ```bash
   export DINGTALK_CLIENT_ID=<the app key>
   export DINGTALK_CLIENT_SECRET=<the app secret>
   syscity channels add dingtalk
   ```
   Then `syscity start` to run the gateway. Nothing to point anywhere — the
   gateway connects out to DingTalk itself.

`DINGTALK_OPENAPI_ENDPOINT` only needs setting for a sovereign deployment or a
test peer; `https://api.dingtalk.com` is the default.

**Stream Mode opens no socket until the platform hands one over.** The gateway
first makes an HTTP call that returns an endpoint and a one-shot ticket, and
only then connects — which is why a reconnect starts with the handshake again
rather than with the socket.

**Every delivery is acknowledged, before it becomes a turn.** An unacknowledged
message is redelivered, so the acknowledgement goes out first and the message is
handed on after — and a redelivery of the same message is acknowledged again
without becoming a second turn.

**A reply does not go over that socket.** It is an HTTP POST to a
`sessionWebhook` URL that **arrived inside the message**, and that URL is
therefore checked against DingTalk's own hosts before anything is sent to it.
The URL **expires** — which is the platform's limitation rather than this
channel's: a bot can only reply to a conversation it has heard from recently,
and a reply after the window closes is refused with that reason. The durable
alternative needs an access token and is not implemented here.

Only **text** messages are routed; pictures, files, audio and cards arrive on the
same connection and are skipped. A group message's mention comes from the
platform's own field rather than being parsed out of the text, so `require_mention`
works here. Replies over 20,000 characters go out as consecutive messages.

## Access control

Per channel, `dm_policy` gates who may talk to the agent:
- `open` — anyone may DM.
- `pairing` — require device pairing.
- `allowlist` — only `allow_from` entries are accepted.

Group chats honor `require_mention` (the agent responds only when mentioned).

## Troubleshooting

- `✅ <Channel> channel initialized` in gateway logs means the adapter started;
  webhook delivery still depends on the public URL being reachable.
- Run `syscity channels status <type>` / `syscity channels test <type>` to
  inspect a channel's configuration and credential presence.
- Signature mismatches on webhook POSTs usually mean the `token` /
  `encoding_aes_key` in your config do not match what the platform console
  shows.
