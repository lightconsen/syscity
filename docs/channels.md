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
