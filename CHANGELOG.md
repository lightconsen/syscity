# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release workflow integration: the `## [X.Y.Z]` section matching a pushed tag
becomes the GitHub Release body. Write release notes here BEFORE tagging;
if no section matches, the release falls back to auto-generated notes.

## [Unreleased]

### Added

- **GAIA Level 1 benchmark.** The official `question_scorer` is wired through the
  eval harness. The 53-task × 3-trial baseline was 70.4% (112/159); after the
  determinism, retrieval and OCR work below, a repeat run scores 78.0% (124/159),
  with tasks passing all three trials rising from 29/53 to 33/53 and zero-pass
  tasks falling from 8 to 3. Both runs are recorded in `evals/ledger.md`. See
  `docs/benchmark.md` §M2 for the method and the failure breakdown.
- **`web_search` can read the pages it finds.** `fetch_top: N` fetches the top N
  results in the same call (through `web_fetch`, so the SSRF guard applies), and
  `depth: "advanced"` asks the provider for full page text where supported
  (Tavily `search_depth` + `include_raw_content`). The result cap moves from 10
  to 20.
- **`image` reads text out of images** via the local RapidOCR models when the
  `vision` feature is on: the tool now returns recognized text in reading order
  alongside the existing metadata, instead of dimensions only. The OCR models are
  resolved on their own, without pulling the OmniParser detector.
- **OCR decodes with the model's own dictionary.** The recognition model emits
  class indices, so the english PP-OCRv3 dictionary is downloaded and pinned to
  the model; a class count that does not match it is an error, not a guess.
  Detection boxes expand by `ratio·area/perimeter` (the standard DBNet unclip)
  and long lines are no longer squeezed into a 1600 px recognition input, which
  is what made dense document screenshots read as fragments.
- **Suite summaries report task-level outcomes**: all trials passed / at least one
  / zero, with the zero-pass task ids listed. Trial-level rates hide flakiness.
- **Eval runs inherit the caller's proxy** through the new
  `AgentConfig.tool_environment`, so shell `curl` in an eval can reach sites that
  need it.

### Changed

- **Eval runs are deterministic and less budget-bound**: temperature 0.0 (was
  0.7), 60 tool iterations (was 30) with a soft "wrap up" reminder at 70%, and a
  system prompt built from the registered tool list (it previously named five
  tools that were not registered).
- **Eval runs state the answer contract explicitly.** The appended prompt now
  says the `FINAL ANSWER:` line is read by an automatic checker, not a person:
  the value alone, no unit, no explanation, no parentheticals, lists in the
  requested order with repeats kept. On the GAIA run that motivated it, ~10
  failing trials were answers the model knew but wrapped in prose or units
  (`22 years` for `22`, `No — Earl cannot do it; …` for `No`). Measured on the
  9-task subset drawn from those: 40.7% → 81.5% over 3 trials each, 7 tasks up,
  none down.
- **`web_fetch` retries transient failures once** and tells the model when a page
  looks JavaScript-rendered, instead of returning an empty body as if the page
  had nothing to say.

- **MSRV is now Rust 1.97** (was 1.95), required by wasmtime 50. `Cargo.toml`,
  `rust-toolchain.toml`, the CI workflows, the CI Dockerfile, `README.md` and
  `docs/build.md` all move together — the MSRV job asserts they agree.
- **wasmtime 48 → 50, zip 2 → 9** (locked at 9.1.2). zip 9's `ZipFile::name()`
  returns a `Result<Cow<str>>`; `screen_recorder` now propagates that error instead
  of comparing the name directly.

### Security

- **The zstd 0.13 advisories are gone** (RUSTSEC-2026-0391/0398/0403). They came
  from wasmtime's `cache` feature, which syscity never configures — it pulls
  `wasmtime-internal-cache` → zstd 0.13, a line with no patched release. Building
  wasmtime with `default-features = false` and without `cache` leaves zstd 0.14
  (via zip 9.1.2) as the only copy.
- **The three zip `extract()` waivers (RUSTSEC-2026-0367/0368/0369) are removed**
  from `deny.toml`: zip 9 fixes them, and `deny.toml`'s `wasmtime@48.0.5`
  duplicate-tree skip now names `wasmtime@50.0.0`.

## [0.3.14] - 2026-10-25

### Highlights

- **CI is hardened for a public repository.** Every workflow now declares `permissions: contents: read` (only the release job elevates, to `contents: write`); all 98 action references are pinned to commit SHAs rather than mutable tags; every job has a `timeout-minutes`; the expensive jobs need the lint job, so a formatting failure no longer spends an hour of macOS runner time; and a new push to a pull request cancels the run it supersedes.
- **macOS runner time cut.** macOS is 89% of this repository's metered usage — $714.61 of $801.90 for the 25 days in October's report, at ten times the Linux rate. The macOS test suite now runs on `main` only and the macOS arm of the `build` job is gone, so a pull request spends no macOS minutes at all. The release workflow is untouched.
- **`Eval Nightly` runs again.** It had failed every night, not on an eval but on the build before it: `cargo build --no-default-features --features plugins,sqlite-vec` did not compile.

### Added

- **`Windows Check`**: `cargo check --all-features` on `windows-latest`, i.e. the `x86_64-pc-windows-msvc` target the release pipeline ships. Before this, the only Windows coverage prior to a tag was the mingw cross-check, which cannot link and skips `ort` — 57 `cfg(target_os = "windows")` sites were unexercised on the target users run.

### Fixed

- **A fresh clone no longer fails suite validation on a suite nobody has
  generated.** `evals/suites/gaia_l1.yaml` is committed, but the task file it
  points at is generated per machine and gitignored (GAIA's terms forbid
  redistribution), so `tests/eval_suites_validate.rs` failed in CI on every
  commit since the GAIA wiring landed. A manifest now declares
  `generated_tasks: true` and validation skips it only when the absence is
  declared — an unparseable manifest still fails. Running the suite before the
  importer has run now names the importer instead of returning a bare ENOENT.
- **The Windows ffmpeg download compiles again.** zip 9's `ZipFile::name()`
  returns `Result<Cow<str>>`; the Unix path was updated but the
  `cfg(target_os = "windows")` path was not, and only the Windows job compiles
  it. `Windows Check` and `Windows Cross Check` both failed on it.
- **Tool-call markup no longer reaches the user as an answer.** When the
  tool-iteration budget is spent, the closing round runs without tools — exactly
  when a model is most likely to emit one more call as plain text. A provider
  that streams that attempt as template markup (deepseek-v4 through a relay)
  left `</｜｜DSML｜｜ calls>` as the reply; two GAIA trials answered with it.
  The guard that catches this on ordinary rounds did not cover the closing
  round, which has no round left to nudge. The markup is now stripped from that
  reply, so the answer is whatever prose surrounded it.
- **A provider outage is no longer recorded as a score.** Three consecutive tasks whose
  every trial failed without spending a token now abort the run with an explicit error and
  write no ledger row — no model answered, so there is nothing to measure. On 2026-10-27 an
  exhausted DeepSeek balance (HTTP 402) turned 148 of 159 GAIA trials into instant failures
  and the summary reported `7/159 = 4.4%` as if it were a capability number.
- **The crate compiles without `simplex` and `browser`.** `add_simplex_channel` referenced `crate::channels::simplex` without a feature gate (every other channel pairs a gated implementation with a `#[cfg(not(…))]` stub), and `InfraState`'s initializer set the `browser`-gated `export_pool` field unconditionally. That is what broke `Eval Nightly` nightly.
- **`MSRV Check` now checks something.** It ran `cargo check --all-features` on toolchain 1.95 — the same compiler every other job uses, since `rust-toolchain.toml`, `Cargo.toml` `rust-version` and the CI `toolchain:` input all say 1.95. It is now a consistency assertion between those three declarations, which is the property that silently breaks when a value drifts.

### Security

- **ctutils 0.4.5** clears RUSTSEC-2026-0385 (non-constant-time mask generation).
- **RUSTSEC-2026-0387** (crypto-bigint, secret-dependent branches) is waived. The patched line is 0.7.6, but `jsonwebtoken 11.1.0` — the latest release — still pins `elliptic-curve 0.13` / `p256 0.13`, which are built against crypto-bigint 0.5. The waiver follows the reasoning already recorded for RUSTSEC-2023-0071 on the same chain: the runtime path only verifies inbound bearer tokens against public keys, and the private-key side is a dev-dependency minting a token for a mock JWKS. Both files say when to revisit it.

### Changed

- **Work lands directly on `main`; `develop` is gone.** Actions minutes are free on a public repository, so the integration branch only parked unverified commits until a pull request opened. The one rule that matters is written down in `ci.yml`: no `v*` tag until the CI run for the commit it points at is green.

## [0.3.13] - 2026-10-22

### Highlights

- **The chat input works again.** Typing a single character into the composer threw `Maximum update depth exceeded. The result of getSnapshot should be cached to avoid an infinite loop.` and swallowed the keystrokes. The guard is assistant-ui's Tap store; the defect is `@assistant-ui/core@0.2.23`, which recomputed the thread-list state into a referentially unstable object while the list was unconnected, so every store read looked like a change (upstream assistant-ui/assistant-ui#6440). `@assistant-ui/react` moves to 0.15.30 and `@assistant-ui/react-markdown` to 0.14.23, the first release on that peer range. The 0.15 line retired `useComposerRuntime`, so the composer is reached through `useAui().composer`.

### Security

- **tokio 1.54.1** clears RUSTSEC-2026-0384, a race in `time::Interval` that arrived with 0.3.12's tokio 1.54.0 bump.
- **rustls-webpki 0.103.16** clears RUSTSEC-2026-0382 (certificate verification could accept distrusted intermediates); **webpki-roots** and **webpki-root-certs 1.0.10** clear RUSTSEC-2026-0381 and -0383 (bundled roots trusting distrusted emSign intermediates).
- **vite 6.4.5** clears three `launch-editor` advisories in the `web/` dev dependency.

## [0.3.12] - 2026-10-20

### Highlights

- **The dependency graph is current across the board.** The Rust side moved to sqlx 0.9, rand 0.10, `sha1`/`hmac`/`sha2` on the digest 0.11 stack, ed25519-dalek 3, notify 8, cpal 0.18, nix 0.31, toml 1.1, ort 2.0.0-rc.15 and pdf-extract 0.12 (lopdf 0.42); the frontend to TypeScript 7, React 19.3, Vite 8 in `site/` and vite-plugin-pwa 2. Two of those carry security fixes: **wasmtime 48.0.7** clears RUSTSEC-2026-0378 and RUSTSEC-2026-0379 — the latter a Cranelift aarch64 miscompile, CVSS 9.3 — and **pdf-extract 0.12** moves lopdf to 0.42, clearing GHSA-6fj4-rcr3-r7rr. rand 0.10 splits the OS entropy source into a fallible `SysRng`; the session-token, PKCE and gateway-token paths keep using the OS source, and session creation now reports a failure instead of panicking.
- **macOS release builds link again.** `cargo build --release` failed with an undefined `___isPlatformVersionAtLeast`: ort's prebuilt `libonnxruntime.a` carries Objective-C++ CoreML objects that call that clang compiler-rt helper, and rustc links with `-nodefaultlibs`. It only surfaced under the release profile's `lto = true` + `-dead_strip`, which walk the whole archive. `build.rs` now links compiler-rt on macOS when `vision` is enabled.
- **Two CI blind spots are closed.** A `Windows Cross Check` job compiles `x86_64-pc-windows-gnu` on every pull request — `tools/win_appcontainer.rs` previously had no PR-time coverage at all — and a `Release Link (macOS)` job performs a real release link on pushes, because every other job runs `cargo check`, which never links.

### Changed

- Pairing codes are drawn with `random_range` rather than `%` over a full-range value, removing the modulo bias.
- The stale `RUSTSEC-2026-0187` (lopdf) audit ignore is gone: pdf-extract 0.12 moved to lopdf 0.42, so the advisory no longer matches anything.

### Fixed

- cpal 0.18 and notify-debouncer-full 0.7 compile on Linux — `DeviceTrait::name` is the `Display` impl now, and the debouncer's recommended cache is `NoCache` there — and under clippy.
- The HMAC e2e helpers import `KeyInit`, which hmac 0.13 moved off `Mac`.

### Removed

- `rustyline`, declared since March but never referenced: interactive input moved to crossterm the same day it was added. Dropping it also removes `clipboard-win`, `error-code` and a duplicate `unicode-width`.

## [0.3.11] - 2026-10-18

### Removed

- **Office document export** — the `pptx`/`docx`/`xlsx` converters (`src/office/`) and the `?to=` download target are gone, along with the four crates that existed only for them (`docx-rs`, `rust_xlsxwriter`, `ppt-rs` and the test-only `calamine`). The dependency graph is the point: it carried **four copies of `zip`** and, through `ppt-rs` → `pdfrs`, a whole PDF-rendering subtree (`syntect`, `subsetter`, `ttf-parser`, `aes`, `cbc`) for one feature. It now carries **two** `zip` copies — the direct one, still used by the plugin installer and the RAG loader, and the updater's — and `ppt-rs`/`pdfrs` are gone. `write_report` still writes and previews markdown and HTML, and **HTML now downloads as PDF** (`?to=pdf`) rendered by a real browser: the gateway holds one `BrowserPool` for export, shared with the debugging bridge when it is enabled, and each export gets a fresh tab closed whether or not the render succeeds, with a 30-second ceiling. A browser is the honest engine here — the document is arbitrary agent-authored HTML, and a partial CSS renderer would not fail but produce a plausible-looking PDF with a broken layout. Without the `browser` feature the export answers 501; with it and no pool yet, 503.

### Added

- **`video_generate`**, as the fourth media kind — the gap the hermes comparison actually records ("完全没有视频"). It rides the same `generate_media` abstraction as image, speech and transcription, with the configured provider's credential. **Video is the first kind that is not one request**: the vendor accepts a job, reports progress and serves the result when it is done, so the submit → poll → fetch loop lives in the provider, with a five-second interval and a ten-minute deadline (a job that never finishes must not hold an agent turn open for ever). The paths are the OpenRouter-style contract (`POST /videos`, `GET /videos/{id}`, `GET /videos/{id}/content`), which is a real, already-configured syscity preset. `duration`, `aspect_ratio` and `resolution` travel as vendor hints, the way the image and speech knobs do. **Two limits stated rather than implied**: the path is not yet per-preset (a vendor whose media endpoint sits elsewhere needs that field before it works), and `video_analyze` is deliberately absent — watching a video is a *vision* capability, not a generation one, and folding it in here would put two unrelated things behind one request type. The tool waits synchronously, so a turn that generates a video blocks for as long as the job takes.
- **A media capability on the provider layer** — the vocabulary, not yet the implementations. `image_generate`, `tts` and `stt` each grew their own path: a hardcoded `https://api.openai.com/...` endpoint and their own read of `OPENAI_API_KEY`, which means media work ignores the configured provider, its credential and its fallback chain. `src/providers/media.rs` now defines the kinds (`Image`, `Speech`, `Transcription`, `Video`), one request shape, and the three ways a vendor actually answers — inline bytes, a URL, or text — because modelling those as one `Vec<u8>` would make every caller guess. `Provider::generate_media` is **defaulted to a refusal in words** ("this provider does not support speech generation"): most providers do not do media, and that is a better answer than a missing method or an opaque failure. The OpenAI provider now implements image, speech and transcription on it, through the same gateway client chat uses — so media gets the configured credential, retry, timeout and rate limiting instead of a second read of `OPENAI_API_KEY`. Speech reads bytes from `post_json_streaming`, which already existed; no new client method was needed. Transcription now goes through the same path too, via a **multipart** method on the gateway client whose parts are described rather than built — the retry loop takes `Fn`, so a one-shot form could not be replayed, and the form is rebuilt from owned bytes on every attempt. **The three tools now go through it.** `image_generate`, `tts` and `stt` no longer create their own `reqwest::Client`, post to a hardcoded `https://api.openai.com/...`, or read `OPENAI_API_KEY` / `DALL·E_API_KEY` at all: they take the configured provider from the tool context and call `generate_media`, so media uses the same credential, retry, timeout and rate limiting as chat. **Two limits stated plainly:** it uses the *active* provider, not the router's fallback chain, and only the OpenAI provider implements media so far — so a deployment whose chat provider is, say, Gemini now gets "gemini does not support image generation" where it previously worked by reading `OPENAI_API_KEY` regardless of the configured provider. That is the honest consequence of making media follow the configuration: the old behaviour was a second, unconfigured path to one vendor. The vendor-specific knobs the tools already exposed — image `size`/`style`, speech `voice`/`speed`, transcription `language`/`temperature`/prompt — travel as documented vendor hints rather than being silently dropped. `stt`'s `response_format` argument is gone: the abstraction always returns text, so the parameter no longer selected anything. With no provider on the context a tool says so instead of falling back to an environment variable, and `tts` still falls back to the local `say`/`espeak` binary, which is not a vendor. **The three tools are not re-plumbed yet**; until they are, they still reach OpenAI directly.
- **Named execution backends, reachable from the `shell` tool.** A deployment declares them under `[security.execution_backends.<name>]` — `type = "docker"` with an `image`, or `type = "ssh"` with a `host` — and the agent asks for one **by name**, never by image or host, so what is reachable is the deployment's decision and a call only chooses among those. The name travels to the tool through the same per-call context the fence posture uses. **A name that is not configured is an error, not a quiet fallback to local**: a command that meant to run elsewhere and ran here instead would have answered a different question, and the message names what *is* available so the next call can be right. A backend that cannot honour the fence (ssh) fails the call with its reason. The same argument and the same rule are on `shell`, `process` and `code_exec`, so the three cannot disagree about what a missing backend means. **A broken backend now says what is broken**: a missing client program is reported as the backend being unusable ("the `docker` backend cannot run: the `docker` command was not found — is it installed and on PATH?") rather than as the local runner failing to spawn a program the caller never named, while a permission error keeps its own meaning; and a container is refused a working directory the fence did not grant, since it can only see what was mounted — the alternative is docker answering "no such file or directory", which reads like a typo. The `ssh` options are built in one place shared with `computer::remote_control`, pinned by a test, so the two clients cannot drift.
- **Remote execution: a `docker` and an `ssh` backend behind the existing `ProcessRunner` seam** (`src/tools/process_runner/remote.rs`). A request can now name where it should run — `ExecutionBackend`, defaulting to `Local`, so nothing that does not ask changes — and selection is per request rather than a property of the process. **The two backends answer the write-fence question differently, and that difference is the whole design.** *Docker translates it*: granted roots become read-write bind mounts, `deny_network` becomes `--network none`, and the `.git`/`.syscity` carve-outs become read-only mounts emitted after the roots that contain them, because a nested mount is what shadows an outer one. The host filesystem outside those mounts is not read-only but absent, which is stronger than the local fence can say — and an absent carve-out is shadowed with an empty read-only tmpfs, so a `.git` that does not exist yet still cannot be created. That last case is one Linux's own posture does not cover, since Landlock cannot subtract a subtree. *SSH refuses it*: there is no far-side kernel a fence could be applied to, so a fenced request is rejected with the reason and told to run locally or on docker — never run unfenced, because quietly dropping the fence would turn the one guarantee this project is built around into a suggestion. An unfenced request runs with every argument quoted, since the command is reassembled into one string and parsed again by the remote shell. **What a remote backend cannot do is documented rather than left to be discovered**: the seccomp escape-vector deny list, `pre_exec` rlimits (an opaque closure, so there is nothing a container flag could be built from — dropped with a warning rather than refused, because the shell tool sets one unconditionally), process-group kill at timeout, and the escalation path keyed on the local fence's stderr.
- **SimpleX Chat**, as a channel — the fourth with no webhook route, and the one that is a client of a *local daemon* rather than of the platform: the gateway holds a WebSocket to `simplex-chat`, and what travels on it is **SimpleX's own command language wrapped in JSON** (`{"corrId": …, "cmd": "/_send @<id> json […]"}`), with the message JSON-encoded inside the command string. Addressing is always by id — the display-name forms resolve at the daemon and silently drop when they do not, which is not something a reply should be able to do. **Contact requests are accepted automatically**, since otherwise nobody could ever reach the bot; that is not the access control, which runs on every message afterwards. **SimpleX has no mention** — in a group a sender cannot address the bot, so group messages arrive marked unmentioned, which makes `require_mention` mean "say nothing in groups" (off by default, so a group the bot is in works). Only text is routed; images, files and voice notes are separate events carrying no text and are skipped, as are the file-transfer offers, which are not fetched. The daemon is application-silent for long stretches, so idle time is not mistaken for a dead socket — the keepalive is the WebSocket's own ping. Configure with `SIMPLEX_WS_URL` (default `ws://127.0.0.1:5225`), then `syscity channels add simplex`.
- **IRC**, as a channel — the third with no webhook route and the first that is not a WebSocket: IRC is a plain line protocol over a TCP socket, spoken directly. TLS is on by default on port 6697. **In a channel the bot answers when it is named** (`nick:`, `nick,` or `nick `), and the name is removed before the agent sees the text; a message that does not name it is still delivered but marked unmentioned, so `require_mention` decides rather than the channel — and this is the one channel whose default for that setting is `true`, because a bot sitting in a channel answering everything said in it is the wrong default. A private message is answered to its sender. **The nickname asked for is not always the one handed back** — a server that already has it returns `nick_`, then `nick_1` — and it is the confirmed name that a channel message must be addressed to, so that is what the bot watches for. **The message limit is bytes, not characters**: IRC caps a whole line at 510 bytes including the command that carries it, so a long channel name leaves less room, and replies are split on that budget at character boundaries. A reply is itself a protocol line, so newlines and NULs are neutralised before sending — otherwise an agent's answer could carry a second IRC command — and incoming colour codes are stripped. Configure with `IRC_SERVER`, `IRC_CHANNEL` and `IRC_NICKNAME` (plus `IRC_NICKSERV_PASSWORD` for a registered nickname), then `syscity channels add irc`.
- **DingTalk (钉钉)**, as a channel, over **Stream Mode** — the second one with no webhook route, and its own shape again. Stream Mode opens no socket until an HTTP call hands one over: the gateway asks for a connection, gets an endpoint and a one-shot ticket, and only then connects — so a reconnect starts with that handshake rather than with the socket. **Every delivery has to be acknowledged**, before it becomes a turn and on the same connection, because an unacknowledged message is redelivered; a redelivery is acknowledged again without becoming a second turn. And **a reply does not go back over that socket at all** — it is an HTTP POST to a `sessionWebhook` URL that arrives inside the message, which means the sender influences a URL the gateway then sends a request to, so it is checked against DingTalk's own hosts before anything is sent. That URL expires, which is the platform's own limit rather than this channel's: a bot can only answer a conversation it has heard from recently, and a reply after the window closes is refused with that reason. A group message's mention comes from the platform's own field rather than being scraped out of the text, so `require_mention` works here. Only text messages are routed, and replies over 20,000 characters go out as consecutive messages. Configure with `DINGTALK_CLIENT_ID` and `DINGTALK_CLIENT_SECRET` — with the app's robot set to Stream Mode — then `syscity channels add dingtalk`.
- **WeCom (企业微信)**, as a channel — and the first one with **no webhook route at all**. The gateway dials out to WeCom's AI Bot gateway and holds a WebSocket open; the same socket carries messages both ways, so there is nothing to expose publicly and nothing to point at the gateway. Inbound still goes through the tail every other direct-message channel uses, so access control, `/new` and the session mapping that reply addressing depends on are shared rather than reimplemented — which is also why that tail grew a mention state: a group message is exactly one that is not a direct message, and the inbound stage reads that to decide whether an unmentioned group message should be suppressed. Without it a WeCom group's bot would answer every message in it. **A reply is a frame that echoes the message it answers**: every inbound message carries a `req_id` and answering means sending it back, which is what puts the answer in the right thread. That window closes, so the last `req_id` per chat is remembered and an expired one falls back to starting a message instead — a fallback a **group does not have**, because an AI Bot cannot start a conversation in a group, so such a reply is refused with that reason rather than sent into the void. The same rule means the channel can only answer a conversation it has heard from. The platform allows one connection per bot and a reconnect is a fresh authentication rather than a resumption, so the channel reconnects on a 2/5/10/30/60-second backoff and re-subscribes; if another connection takes the bot's place it stands down rather than fighting for the socket. Only text messages are routed — images, files and voice notes arrive over the same socket and are acknowledged and skipped — replies over 4000 characters go out as consecutive messages, and there is no typing indicator, because the bubble on this platform is the first frame of a *streamed* reply and this channel does not stream. Configure with `WECOM_BOT_ID` and `WECOM_SECRET`, then `syscity channels add wecom`.
- **Microsoft Teams**, as a channel: a Bot Framework activity in, the Bot Framework connector out. It is the second bearer-token channel, and it needs a check Google Chat's does not. The token has to verify against the issuer's published keys **and** name your app as its audience — that is what says Microsoft minted it for this bot — and then its `serviceurl` claim — one word, lower case, which is the name both Bot Framework SDKs look up and the one the token actually carries — has to match the activity's, which is what says it was minted for *this* conversation surface. A token Microsoft legitimately signed for another tenant's connector passes the first check and fails the second, which is why both are there. The reply address is two values rather than one: a reply goes to `{serviceUrl}v3/conversations/{conversationId}/activities`, and the service URL is per-tenant (the regions differ by path — `/amer/`, `/emea/`, `/apac/`), so the pair travels joined in the conversation id. The service URL is held to an allowlist of Bot Framework hosts before a freshly minted token is attached to it, because that host is what decides where this bot's credentials go. Conversation updates, reactions, invokes and typing all arrive on the same URL and the bot's own activity would loop if it were answered, so only messages from people become turns; Teams' `<at>…</at>` mention markup is removed but the name inside it is kept, so a mention of a colleague does not lose their name on the way to the agent. Replies past 28,000 characters go out as consecutive messages, and the outbound half is configured separately from the inbound one, so a missing app secret warns without taking the verification down with it. Configure with `TEAMS_APP_ID`, `TEAMS_TENANT_ID` and `TEAMS_CLIENT_SECRET` (`TEAMS_ISSUER`/`TEAMS_JWKS_URL` default to the public cloud), then `syscity channels add teams`.
- **Google Chat**, as a channel — the first whose webhook is authenticated by a **bearer token** rather than a signature, which is why it arrives with a shared verifier (`gateway::jwt_verify`) and an outbound OAuth token source (`channels::oauth_token`) rather than a signature helper of its own. The token is verified against the issuer's published keys and **then its identity is checked**: Google signs these for its own service accounts, so a valid signature proves Google issued the token, not that it was issued for *this* app — the `email` claim is held to a configured allowlist, and an empty list refuses everything. `issuer`, `audience` and `jwks_url` are required rather than defaulted, because they depend on the app's **Authentication Audience** setting and a wrong guess would reject every delivery. Only user messages are routed: a bot's own reply arrives through the same webhook and answering it would make the bot talk to itself, and Chat's non-message events are acknowledged and skipped. Replies past 4096 characters go out as consecutive messages. The outbound half is configured separately from the inbound half — they use different credentials — and a missing service account warns without taking the inbound check down with it.
- **Twilio SMS**, as a channel: a form-encoded webhook in, the Messages API out. Its signature differs from every other channel's in one way that shapes the whole config: `X-Twilio-Signature` is `base64(HMAC-SHA1(auth_token, url + Σ key+value))` over **the URL Twilio dialled** plus the parameters in key order — and that URL cannot be read back off the request, because behind a tunnel or a proxy the gateway sees `127.0.0.1:<port>` while Twilio signed `https://…`. So `TWILIO_WEBHOOK_URL` is required configuration rather than a convenience, and a delivery that cannot be checked is refused: there is deliberately no "skip validation" switch, because a channel whose authentication can be turned off is not authenticated. Both spellings of the scheme's default port verify, since Twilio has signed with and without it. Replies past 1600 characters go out as consecutive texts split on a word boundary — SMS has no editing, threading or reply-to, so that is the closest thing to one reply the medium has — and Twilio's status callbacks, which arrive on the same URL carrying no body, are acknowledged without becoming an empty turn. Configure with `TWILIO_ACCOUNT_SID`, `TWILIO_AUTH_TOKEN`, `TWILIO_FROM_NUMBER` and `TWILIO_WEBHOOK_URL`, then `syscity channels add twilio`.
- **LINE**, as a channel: a signed webhook in, the Messaging API out. The signature is `base64(HMAC-SHA256(channel_secret, raw_body))` in `X-Line-Signature`, computed over the bytes *as sent* — so the body is verified before it is parsed, since re-serialising the JSON would change the whitespace the signature covers and the check would stop matching. Inbound and outbound use **different** credentials (the channel secret verifies what LINE sends us, the channel access token authorises what we send to LINE), which is why they are two config fields rather than one `token`. Replies go out through `message/push` rather than `message/reply`: a reply token is valid only for the delivery that carried it and expires within a minute, which an agent turn routinely outlives. Only text messages are routed — LINE reports follows, joins, leaves and postbacks through the same endpoint, and those are acknowledged and skipped. Markdown is stripped rather than sent literally, because the text message type renders none of it. Configure with `LINE_CHANNEL_ACCESS_TOKEN` and `LINE_CHANNEL_SECRET`, then `syscity channels add line`.
- **`/handoff [take <file>]`**, over `sessions.handoff` and `sessions.handoff_take`: carry a session to another gateway. There is no transport between gateways, so a handoff is a file — the conversation plus the session state that is not in the messages (its name, its model, and the plan it was working through) — which the other gateway applies to a session of its own. Nothing about the sending session changes. The bundle is versioned, and a version this build does not read is refused rather than partly applied. Two limits are stated rather than implied: the plan travels but is not applied, because a plan is the receiving agent's own bookkeeping (it builds one from the conversation), and the file is confined to the transcripts directory, since a caller-supplied path that could name any file would make this "read whatever the gateway can".
- **`/snapshot` and `/rollback`**: a session-scoped checkpoint of the *files* its writes replace, and the way back to it. The content a write destroys cannot be reconstructed afterwards, so the pre-image is kept **before** the write — which means a hook on the three tools that name the file they are about to overwrite (`file_write`, `file_edit`, `apply_patch`) rather than anything reading the audit log afterwards. Nothing is recorded until `/snapshot` opens a checkpoint, so the feature costs nothing until it is asked for; a rollback restores those files, removes the ones the writes created, and drops that checkpoint and every newer one, since they describe a state that no longer applies. Recording survives a restart (a checkpoint left open stays open), because a rollback that silently covered only part of a session's writes would be worse than none. `shell` is deliberately not covered: it can overwrite any file without naming one, and a checkpoint that guessed would be worse than one that says what it covers. The conversation is untouched by a rollback — `/undo` is what walks that back.
- **`/branch [name]`**, over a new `sessions.branch` method: continue this conversation down a second path without disturbing the first. The branch is a new session holding what this one has so far — copied by the agent, which owns the turns and the message window, and written to the session store as it goes, so it is a conversation like any other: it appears in the session list, `chat.history` shows it, and `/resume` switches to it. The copy carries the turns and the context and deliberately not the redo stack, which is a fact about the parent's history rather than about the conversation. A copy that fails takes its session row with it, so a failed branch does not leave an empty session that looks like a branch.
- **`/queue` queues prompts for the next turn**, with `list`, `rm N`, `edit N <text>`, `move A B` and `clear` to change your mind before it runs. The name was already taken by the *mode* (what an arriving message does to a running turn — steer, interrupt, follow-up), and that moved to `/queue mode`; the bare `/queue interrupt` spelling still sets it, so an existing habit does not start queuing the word. Routing follows hermes': a leading management verb whose arguments fit manages the queue, anything else is a prompt, and `/queue add <prompt>` forces the other reading. A released prompt goes back through the inbound pipeline rather than straight to the agent, so it is routed, written to the history and answered to the channel it came from — and it runs when the turn it was queued behind finishes, which is what makes the queue a queue rather than a second way to interrupt.
- **`/import <file>`** in the TUI, over a new `sessions.import` method: the other half of `/save`, and narrower on purpose — only the JSON rendering round-trips, because Markdown, HTML and text are documents written for a person and re-reading a conversation out of one would be guesswork. The messages are appended to the session's history, where the next turn and the history view find them. The file is resolved inside the transcripts directory and nowhere else: a caller-supplied name that could name any file would make this "read whatever the gateway can read".
- **`/save [md|html|json|text]`** in the TUI, over a new `sessions.export` method — and `/export-session` takes the same format argument. The two export paths disagreed: the command wrote HTML and nothing else, from the transcript buffer this process happened to hold, so a session untouched since the last start could not be exported at all and the formats one path offered the other did not. Both now read the **session store**, which is the durable record, and render through one writer (`gateway::session_export`); `/export-session` with no argument still writes HTML, which is what it always did. `/save` prints the path it wrote.
- **`/plan`** in the TUI, over a new `sessions.plan` method: the goal, each step of the agent's plan marked done / current / pending with the current step's expected outcome, and a count. The plan is the agent's own (built by the planner, advanced by the `update_plan` tool), so the method asks the agent rather than tracking anything separately; a conversation with no plan answers "none", which is its ordinary state rather than an error.
- **`/undo [n]`** in the TUI, and the `sessions.undo` method behind it: the conversation's last turn — or `n` of them — is dropped from the agent's context and its turn log. The machinery was already there and merely unreachable (`Thread::undo_last_turn` with its redo stack and SQLite cleanup, and an `AgentQuery::UndoLastTurn` that nothing sent); what is new is a way in, and a loop that stops when there is nothing left rather than asking for turns that do not exist. The count reported is the agent's answer, not the one asked for. `/title` is `/rename` under the name hermes uses.
- **The OAuth device flow (RFC 8628)**, for logging a provider in with no browser on the gateway's host — over SSH, in a container, on a headless box. A provider config gains an optional `device_auth_url`; `providers.auth_start` then takes `mode: "device"` (and the CLI, `syscity provider auth <id> --device`) and answers with a code to type at the provider's verification URI instead of a redirect. There is no callback to complete it: `providers.auth_status` polls the provider on each call, so the client that is waiting is what advances the login, and the answer carries the code, the interval the provider asked for, and the reason for a refusal. A provider configured with `device_auth_url` and no `auth_url` is device-only and is started in device mode without being asked; `auth_url` is now optional to allow exactly that.
- **The preset list is seven,** down from a high of 35: `openai`, `anthropic`, `deepseek`, `kimi`, `ollama`, `qwen` and `openrouter`. A preset buys convenience, not capability — any OpenAI-compatible endpoint was always reachable through `base_url` — and each of the other twenty-eight was one more fact (base URL, model ids, auth shape) that could rot without anyone noticing. Nothing was deleted from the code: `gemini`, `azure` and the rest are still protocols a custom provider can name, and adding a vendor back is one TOML table plus a resolver test. The alternative entry, kept below, records the breadth pass that this narrows.
- **Twenty more built-in providers**, taking the registry from 15 to 35: the aggregators and gateways `openrouter`, `ai-gateway` (Vercel), `huggingface`, `opencode` (Zen) and `kilocode`; the inference platforms `nvidia`, `fireworks`, `novita`, `gmi`, `nebius`, `deepinfra`, `arcee` and `upstage`; the vendor endpoints `groq`, `zai` (Z.AI's international GLM), `stepfun`, `xiaomi`, `tencent-tokenhub`; and the local pair `ollama-cloud` and `lmstudio`. Each is one table in `presets.toml` — display name, suggested models, and the endpoint's protocol, base URL, auth and context window — so it appears in `syscity setup`, in the WebUI's provider picker and in `models.presets` without any code naming it. Providers whose model ids carry a vendor prefix (OpenRouter, the Vercel gateway, Hugging Face) are listed by the gateway's own spelling.
- **`--profile <name>`** (and `SYSCITY_PROFILE`) runs against an isolated home under `<base>/profiles/<name>` — its own config, database, skills, memories, sessions and secrets. A profile is a path root, and a path root is a process: two profiles never share a process, a database or a cache, which is what makes them isolation rather than a namespace. `syscity profile list|create|path` manages them, and a name that is not one ordinary path segment is refused with the reason rather than sanitised.
- **`@path` completion in the TUI.** Typing `@` at a word boundary completes a path in the agent's workspace: the list shows one directory at a time, fuzzy-ranks the names, and accepting a directory inserts `@src/` so Tab keeps drilling while accepting a file closes the token with a space. Lists come from the gateway's existing `workspace.list` (the same call the WebUI file tree uses), fetched lazily — one request per directory actually entered, never one per keystroke — and completion itself stays a synchronous read of what is cached, so Tab never waits on the network and works offline (it simply offers nothing). A directory the gateway refuses is remembered as unavailable; the cache is dropped on a session switch or a lost connection.
- **Code blocks are syntax-highlighted in the TUI.** A fenced block is highlighted as a whole — one parse pass over the block, so a multi-line string or comment keeps its colour across the break — using the language tag off the opening fence (`rust`, `rs`, `py`, …), and rendered per line. The theme is built from the TUI's own palette rather than a stock colour scheme, so on a 16-colour terminal the highlighting stays inside the colours that terminal actually has instead of degrading into arbitrary collisions. Blocks are cached by `(language, body)`; a growing block is simply a new entry, so streaming does not re-highlight on every frame.

- **`[security] guardian`** (default `false`) has a second model review every tool call that reaches human approval. It is **deny-only**: a rejection refuses the call without asking anyone, a passing review adds the reviewer's reason to the approval a human sees, and an unavailable reviewer leaves the approval exactly as it was — the model can stop a call but never grant one in the human's place. `guardian_model` picks the reviewer; the default is the configured provider's own.
- **`[security] shell_safety` and `[security] sandbox_interceptor`** (both default `false`) install two policy hooks that were written but never plugged in. `shell_safety` classifies every segment of a shell command — a safe binary runs without prompting, a risky one asks, a dangerous one (`rm -rf /`, fork bombs) is refused; `shell_safety_unknown_binary = "ask" | "allow"` decides what an unrecognised binary means, since the built-in safe list is ~90 names and ordinary work runs plenty outside it. `sandbox_interceptor` applies a command/path blocklist and flags credential-shaped arguments for approval. Both are gateway-wide and read at startup. Off by default: turning either on changes which calls reach a human.
- **`[security] browser_allow_private`** (default `false`). The browser tool now runs the same SSRF guard `web_fetch` has always run, so private and loopback addresses need this switch to be reachable again — for a deployment driving its own dev server on `localhost`. The scheme rule is unaffected: `file:` stays refused either way.

### Changed

- **`syscity channel list` shows every channel.** It read a hand-written list that had drifted: WeChat MP was missing, so a channel the gateway was actually running could not appear in `channel list` at all, and the same pattern in `channel status --agent` was missing two more. The channel list, the compile-time feature table and the `Channels:` hint are now all derived from the channel enum, so a channel added later shows up without also being added to four places — and `channel add`/`stop`/`remove`/`test` read one mapping rather than seven copies of it.
- **One provider registry, and a preset is authoritative for its vendor.** Vendors were listed twice — once in `presets.toml`, once in a hand-rolled table in the model router that carried a second copy of the display names, base URLs and model lists — and the two disagreed, so a vendor could resolve while never appearing in a picker. The second table is now derived from the first: adding a provider is one entry, and a test holds the two in step. Alongside it, a provider's config key now names the vendor at runtime: a provider keyed `grok` gets the xAI preset's context window and stream family rather than whichever preset its `provider_type` spelling pointed at, and `provider_type` keeps its other job — choosing which endpoint of a dual-protocol vendor to use, like Kimi's `/v1` or its `/anthropic`. Also: the Ollama preset handed out `http://localhost:11434` as a base URL while the resolver appends `/chat/completions`, so the `/v1` path the preset itself uses is what it now gives out; `models.presets` returns its vendors sorted rather than in hash order, so the picker's buttons stop moving between opens; and a preset's first suggested model is now its default model, where the two used to disagree.
- **The main turn loop compacts context before asking, not after failing.** Compaction was proactive only on the non-streaming path; the loop that runs ordinary turns waited for a provider to reject the request for length. It now consults the same budget test, and records one decision per turn carrying both the token figures and the cost guard's, so the two budget levers read in one place.
- **The browser tool refuses private, loopback and non-web navigation targets.** Every `Navigate` now goes through the SSRF guard before Chrome is pointed anywhere: private ranges (literal and DNS-resolved) and the default `localhost` blocklist are refused, and so is `file://`, which used to open — handing a local file's contents to the page and to whatever the model can read back. Set `[security] browser_allow_private = true` to re-open private targets. What this does *not* cover: link clicks, form submits, `window.location`, meta-refresh, `window.open`, new tabs, `Back`/`Forward`/`Reload` and page subresources — closing those needs a CDP-level navigation subscription, not a check on the URL an action names.

## [0.3.10] - 2026-10-02

### Highlights

- **Channel replies reach the channel.** A message that arrived over Telegram, Slack, WhatsApp, Discord or any other channel was processed end to end — pipeline, agent turn, history, `AgentResponse` — and then stopped: nothing bridged that event into the reply dispatcher, so the answer never left the gateway. Every configured channel user got silence. The inbound dispatch path now runs the final answer through the outbound pipeline like the queue path does, and cron's `Announce` delivery — which had the same shape of hole — now dispatches to the channel it names instead of only emitting an event.
- **Authentication was two switches, and only one of them was enforced.** `auth_mode = "token"` gated WebSocket upgrades but left the REST surface open, because the REST middleware keyed solely off `security.auth_required` (default false). That surface includes `/v1/chat/completions` — the operator's provider key — and `POST /api/v1/ws-ticket`, whose ticket clears the WebSocket gate: an unauthenticated caller could mint one and connect. A configured auth mode now requires credentials on both transports, and the preferred ticket flow works end to end (the handshake used to re-derive identity from the `connect` frame alone, discarding what the upgrade had already validated, so ticket-authenticated clients landed with zero scopes).
- **MCP tools describe themselves.** A server's `annotations` (`readOnlyHint`, `destructiveHint`, `idempotentHint`) now drive a tool's capability: read-only tools stop prompting for approval and stay visible in plan mode, destructive ones keep it. Servers that publish nothing keep the conservative default, and tools whose names carry destructive verbs are classified when annotations are absent. Alongside: `[permissions]` rules can target MCP tools by name (`ask = ["mcp__gmail__send*"]`), and the Gmail preset — which pointed at a package that does not exist on npm — now names one that does.

### Added

- **`device_open_url`**: hand a `tel:`, `mailto:`, `sms:`, `https:` or `http:` URL to the operating system on mobile — the dialer opens pre-filled, the mail composer opens with the recipient, and the user confirms in the target app. Schemes are allowlisted in Rust and again natively.
- MCP tool capabilities are derived from the server's own annotations, with a name-based fallback for servers that publish none.
- `[permissions]` bare-name glob rules reach tools with no primary invocation argument, which is what MCP tools are: `ask = ["mcp__gmail__send*"]` now works.
- The desktop window title is "Syscity Desktop"; mobile keeps "Syscity".

### Changed

- **MSRV is 1.95** (from 1.94), pinned by `rust-toolchain.toml` as well as `rust-version`, and every CI job follows.
- Every WS refusal for a missing or unusable parameter answers `INVALID_PARAMS` — handlers previously read a missing `id` as `""` and reported `NOT_FOUND`, indistinguishable from a resource that does not exist.
- An approval-gated tool in a context with nobody to ask (cron, heartbeat, standing orders, goal runs, delegated sub-agents) now fails closed with a deny naming the `[permissions].allow` pre-approval path, instead of running silently on one path and hanging five minutes on another.
- A policy hook that matched a call and allowed it is authoritative; a hook configured for other tools no longer disables the approval fallback for every MCP and high-risk tool.
- Tests: the e2e tree gains journey coverage for the HTTP surface (probes, OpenAI-compatible endpoints, the ticket exchange, signed webhooks), approval and permission rules, MCP tools, hooks `ask`, channel round trips, heartbeat and standing orders, the admin WS method surface, plus integration coverage for the model router, RAG ingestion, the planner DAG, office conversion, attachments and the eval harness.

### Fixed

- **wasmtime 46 → 48.0.3** (RUSTSEC-2026-0314, a guest-triggerable host panic reachable from the `code_exec` wasm path, and RUSTSEC-2026-0316). The old version has no backport except an LTS line three majors back, which is why the MSRV moved.
- **A signal could abort the daemon on macOS.** wasmtime's default signal-based traps install a Mach exception port whose handler calls `libc::abort()` when a receive is interrupted — and any stray `SIGCHLD` from a child spawn can interrupt it. Signal-based traps are off for both the plugin runtime and `code_exec`.
- **A memory the agent observed was invisible to every conversation.** Recall is conversation-scoped, and all three memory stores filtered with strict equality, so an unbound (user-level) memory — which is what `observe` stores — was excluded by `NULL != ?`. The manager's own observe → session-context loop could not close.
- A reply reaching the channel is the assistant's answer, not an echo of the prompt — pinned by test.

## [0.3.9] - 2026-09-28

Released without a changelog entry (auto-generated notes). Its changes, from
the commits between v0.3.8 and its tag: TUI welcome banner; a chat re-login
card for an expired Syscity Cloud session; the cloud token clear gated behind
the `cloud` feature; permission rules judging each command-chain segment; a
network-allowlist policy proxy for fenced children; language-aware markers in
the fallback secret scanner; the process runner split into a module directory.

## [0.3.8] - 2026-09-23

### Highlights

- **A permission rules engine in front of every tool call.** A `[permissions]` config block carries the gateway-wide mode (`default` / `accept_edits` / `plan` / `bypass`), an `allow_bypass` switch (bypass is refused without it), and `allow` / `deny` / `ask` rule lists — each `"tool"` or `"tool:glob"` matched against the call's primary argument (shell → command, file tools → path, `web_fetch` → URL), evaluated in a locked order: deny → ask → allow → mode → hooks. A session can override its mode (`sessions.set_mode`, ephemeral until restart), the TUI gets `/mode` plus a status-line mode tag, and approvals offer approve / approve-and-remember (the invocation or its parent directory becomes a persistent allow rule) / deny. Plan mode hides and refuses every tool that has not declared itself read-only.
- **The Linux fence grows two layers and the macOS one grows an escape hatch.** A fenced command may ask to run outside its fence — declared up front via a reserved `permissions` block on the call, or failure-driven when the output says the fence refused it — with a human approving before the unfenced re-run, which is announced in the result. On Linux, the Landlock write fence is now stacked with a **seccomp escape-vector deny list** installed on every fenced run (`ptrace`, `process_vm_*`, `bpf`, keyring, module, mount and namespace syscalls are refused; a `clone` carrying any `CLONE_NEW*` flag is refused; `clone3` is forced back to a filterable `clone`), and an optional **namespace view** (`[security] fence_namespaces`, default `auto`): a user namespace plus a private mount namespace with a read-only root and a private `/tmp`, the working trees and `/dev`/`/run`/`/proc` re-bound read-write on top so `2>/dev/null` and AF_UNIX IPC keep working. The network posture (`[security] fence_network`) is a three-platform clause: Seatbelt `(deny network*)`, a Linux seccomp socket filter that leaves `AF_UNIX` alone, and withheld Windows capability SIDs. The `.git` and `.syscity` directories are unwritable inside any granted root on macOS.
- **Delegation is visible while it runs.** Task rows carry `parent_session` and `usage_tokens`; the gateway pushes `agent.usage` and `delegation.updated`; the TUI renders live delegation task rows, a run token counter, and an `/agents` view.

### Added

- The gateway publishes the WS method table and the dispatcher is held to it — every registered method must be in the table and every table entry must dispatch.
- TUI: `/theme` switches the palette and persists it in config; the palette is a `Theme` struct ported to Claude Code's dark theme; the terminal's background is detected via OSC 11 and the palette degrades to the terminal's color depth.
- TUI markdown rendering: inline code spans, consecutive pipe rows aligned as tables, block quotes with a dim gutter, OSC 8 hyperlinks for URLs in scrollback, list items with an accented marker, and ATX headings rendered as bold titles without the marker.
- TUI: multi-line paste inserts content one character at a time (no bracketed-paste surprises), `OSC 52` copies the last answer to the clipboard, `/retry` resends the last prompt through the send gate, and `/expand` reprints the last tool call in full.
- `read_only` is a declared tool capability, and `SandboxedTool` stops masking the capabilities of the tool it wraps.

### Changed

- Multi-user RBAC scaffolding that nothing wired up is removed; `requires_approval` now gates a call where there is someone to ask.
- The TUI event loop is a module directory rather than one file.
- The app version is 0.3.8 across the workspace, the desktop crate, and the web client.

### Fixed

- A reply that was only markup (no text after formatting) no longer completes the turn early.
- Browser: pages are reused across tool calls instead of pooled and abandoned; the empty-object argument shape the schema advertises is accepted; keys reach the page in a form the browser recognizes.
- The planner database URL had a space in it.
- The CORS example in the config guide taught a pairing the gateway refuses; the permissions system is documented in the tools module doc.

## [0.3.7] - 2026-09-20

### Highlights

- **The TUI runs inline.** `syscity tui` writes the conversation into the terminal's own scrollback as it arrives and redraws only a small live region at the bottom — composer, status row, blocking prompts. Native scrolling, text selection and copy keep working, and the transcript outlives the client. It is also no longer a one-line-at-a-time thing: output streams and freezes line by line, tool calls and results appear as they happen, approvals and `ask_user` questions take over the bottom of the screen rather than the whole of it, a message typed mid-turn is queued instead of run alongside, and a dropped socket is reconciled against the gateway's history on reconnect.
- **Browser control can actually interact.** Key combinations, right/middle/double/triple clicks, and drags that move; a screenshot now states the coordinate space it is in and refuses clicks outside it; `escalate` is an explicit way to say the page is not enough.
- **A pass over every trust boundary.** The gateway trades a long-lived token for a single-use, 30-second upgrade ticket and keeps credentials out of its own log lines; every event carries an audience so one client is not handed another's traffic; scopes are granted from the credential rather than from the request; webhook deliveries are bounded and replayed signatures refused; CORS no longer mirrors the request origin, and a wildcard origin can no longer be paired with credentials — **an existing config that has both will refuse to start** (see the note under Changed for the one-line fix).

### Added

- The TUI's status row doubles as a run indicator: an animated frame, a rotating word, and a parenthetical that carries the real information — elapsed time and whether the turn is thinking, responding, or inside a named tool call. It all goes away when the turn ends.
- `/history [n]` reprints a window of the conversation in reading order, and `/history more` pages backwards from the oldest message shown using the gateway's `before` cursor — so a session longer than the 100 messages a resume loads is now walkable back to its beginning.
- The composer lists the matching slash commands while a `/command` is being typed, windowed around the current `Tab` selection.
- `syscity tui` can run against a piped stdin/stdout as a line protocol: one line in, one turn out, slash commands included. Prompts that need a human are settled rather than left to time out.
- `feat(browser)`: key combinations with the macOS command that makes them deliverable; right, middle, double and triple clicks; drags that move the pointer.
- `feat(android)`: a stable code on a failed UI dump, an explicit check for whether the screen shows what was expected, and an observation that still hands over the screenshot when the UI tree could not be read.
- `feat(tools)`: `requires_approval` now gates a call where there is someone to ask; tools declare whether they can be safely retried; tools that drive one shared target are serialized against each other.
- `feat(skills)`: `/learn` authors a skill from a source or from this session, and writes where the watcher is already looking.
- A `...` menu on agent rows in the sidebar — rename, delete, settings.
- Observed token estimates are recorded beside the provider's own count, so the two can be compared.

### Changed

- **Breaking for existing configs: a CORS wildcard may no longer be paired with `allow_credentials`.** The gateway now refuses to start on `security.cors.allowed_origins = ["*"]` together with `security.cors.allow_credentials = true` — the pair tells every browser that any site may make credentialed requests. A config that has both will fail to start with an error naming the setting; set `allow_credentials = false` (nothing here needs it: cookies are not a credential, and the flag only decides whether a browser may attach the ones it holds), or list the origins you trust in `allowed_origins`. The config block in `docs/security-config.md` showed the pair and has been corrected.
- One place decides which model a fresh install uses, instead of the default being restated wherever it was needed.
- Authentication no longer treats cookies as a credential.
- The TUI's tool output is one transcript line per row and is capped by a row budget: the ellipsis is the last row of the budget, not an extra one. Live previews get five argument rows (leaving room for the `⚙ tool` header in a six-row region); a reprint from history gets eight.
- The web client refuses a connection rather than putting the gateway token in the URL when the ticket exchange fails for a reason other than the gateway not having one.

### Fixed

- The TUI padded every wide character in scrollback with a blank column, which made Chinese text look sparse and made lines wrap sooner than they should.
- The composer wrapped its input to the full width while the first row also carried the `> ` prompt, so an input that filled the row exactly lost its last character and the cursor clamped onto the one before it.
- A control chord that was not bound typed its bare letter — `Ctrl+U` put a `u` in the draft. `Ctrl+Alt` is still typed, because that is AltGr on several layouts.
- A multi-line tool call or result was packed into a single transcript line; the renderer drops a newline, so it came out as one run-together row, on screen and in scrollback.
- In line mode, a streamed answer was printed twice, and the line read from stdin was never the line sent.
- A slash command that failed ended the whole TUI; it is now a line of output.
- An approval the gateway refused was dropped from the queue, leaving the blocked turn with no way to be unblocked.
- The run state did not converge when the connection dropped: the status row could spin "running" forever and a prompt could keep the keyboard. In-flight requests now fail as soon as the socket dies rather than after their own timeout.
- Events were applied without checking which session they belonged to.
- A keystroke typed while a command was in flight was dropped instead of queued.
- Gateway calls held the state lock across the round trip, and commands ran inside the event loop, so a slow gateway froze input and redraw.
- The panic hook was left installed after the TUI exited, and a signal left the terminal in raw mode.
- The TUI reported a failed send as a successful one.
- An approval is now routed to the session that raised it, not to whichever client asks next, and an approval whose waiting client is gone is denied rather than left to expire.
- A provider stream that failed mid-flight was dropped; it now has a channel and is retried before any output has been shown.
- Compaction could separate a tool call from its result; the pair is now kept together.
- `audit security --format json|yaml` emitted something other than what it says; the daemon endpoint is resolved instead of assuming `18080`; the `config.toml` the daemon writes now parses and takes effect.
- Outbound replies could be cut inside a multi-byte character.
- A tripped cost guard now clears itself, and can be cleared.
- Browser: keys are pressed in a form the browser recognises rather than as script-dispatched events a page can tell are fake; a script that reports an error is no longer counted as a successful action; the action shapes the schema advertises are accepted.
- Computer-use verification that could not run was being counted as one that passed.
- A spaceless script is no longer classified as a "very short message".
- Shutdown lets in-flight writes land before closing the pool.
- The released notes are drafted from this file; a failure to draft them is no longer silent.
- `rustls` is bumped to the release that fixes RUSTSEC-2026-0285, and the web client's shipped and build dependency trees are clear of advisories.

## [0.3.6] - 2026-09-14

### Added
- Android observation via `android_observe`: returns the numbered actionable UI elements and a screenshot from one call, reports the pixel size of each, and warns when the screenshot and the UI tree describe different screens.
- `android_input` can tap by element index using `target`; the target is re-read from the live screen and the tap is refused if the element moved, is no longer clickable, is disabled, or does not match `target_description`.
- `android_input` supports `sequence` for dispatching multiple coordinate taps in a single adb command, useful for controls that auto-fade before the next turn.
- Android text input now escapes device-shell metacharacters, turns newlines into Enter key events, and types non-ASCII via ADBKeyboard when active; otherwise it fails with actionable guidance instead of silently mangling the text.
- `android_ui_tree` now returns a numbered list of actionable elements with class, labels, center, bounds, and clickability/enabled state, rather than raw XML.

### Fixed
- The `device` argument is now honored by `android_screenshot`, `android_observe`, `android_input`, `android_app_manager`, and `android_ui_tree`; each result reports the serial actually acted on, and invalid device values are refused rather than silently falling back.
- Android screenshots are now returned as valid PNG bytes instead of being corrupted by text encoding.
- Android UI dump failures now classify the likely cause (device offline, unsettled screen, secure window, disabled accessibility service) and point to the screenshot fallback for continued work.
- Empty Android UI trees now explain that the screen is likely a game, canvas, or secure surface and recommend using the screenshot, rather than reporting an unexplained failure.
- Fixed matching for the common adb error `device 'emulator-5554' not found`.
- Tracked scripts and generated Android build files no longer leak machine-specific absolute paths.
- Removed vulnerable dependencies by replacing `serde_yml` with `serde_norway`, upgrading `ratatui` to 0.30, and bumping `event-listener` to a patched release.

## [0.3.5] - 2026-09-11

### Added
- Cloud models are discovered from the cloud proxy's `GET /v1/models` instead of a hardcoded list, refreshed lazily (10-minute TTL) when the model picker opens. The picker shows the models the proxy actually serves and drops entries it no longer advertises.
- The model picker lists **Cloud** and **Local** sections independently. A model id served by both (e.g. `deepseek-v4-pro` through the cloud proxy and from a directly-configured provider) now appears in both, and the cloud copy is independently selectable and routes through the proxy.

### Changed
- Authenticated sessions and device pairings are persisted to the shared SQLite store, so login state and paired devices survive a restart. Only domain-separated SHA-256 token digests are written — never plaintext.
- Audit records attribute actions to the real user (and the rate limiter is keyed per user) instead of hardcoded placeholder actors.
- The secret store and filesystem layout are per-instance handles threaded through the gateway rather than process globals. Single-instance behaviour is unchanged — same `~/.syscity` root, same 0600 encrypted store.

### Fixed
- macOS downloads no longer trigger Gatekeeper's "cannot verify the developer" warning: CLI tarballs and desktop builds are Developer ID signed and notarized.
- `system.reload` no longer removes the runtime cloud provider.
- The status bar shows the bare model name for cloud-pinned sessions instead of the internal `cloud/<id>` reference.

## [0.3.4] - 2026-09-10

### Added
- Composer **+** picker with Experts, Skills, and Connectors tabs. Experts is searchable and single-select; Skills and Connectors are multi-select. In sessions already bound to a specific agent, the Experts tab is hidden and the active tab falls back to Skills.
- Attached expert and skill chips are sent with the next message as hidden per-turn context that guides skill loading and expert delegation without being saved in the transcript. Responses are cached separately for different chip selections.
- Skills and Connectors tabs now show a search box once their list exceeds 10 items. Search matches names and descriptions, and each tab has its own no-match row. Switching tabs clears the search.
- Connector chips are enabled automatically on send when they are not already enabled. If enabling fails, the message still sends and a toast shows the failure.

### Changed
- In Marketplace, an expert’s Summon button now keeps its primary style after the expert is installed.

### Fixed
- The chat view now stays pinned to the bottom while message scrolling settles, so late message measurements don’t leave it short of the newest message.
- Markdown messages no longer rebuild from scratch on unrelated re-renders, reducing flicker and scroll jumps.
- Gzip archive member splitting is now exact instead of scanning for magic byte sequences, fixing flaky archive index rebuilds when compressed data contained gzip-header-like bytes.

## [0.3.3] - 2026-09-09

### Added
- Added full English/Chinese localization across the web UI — chat, settings, Knowledge Base, Workspace, onboarding, ask/approval, updates, and Extensions — plus a settings language switcher.
- Added cloud credit controls: account popover with balance badge, daily check-in, one-time signup bonus, invite code redemption, purchasable packs, and recent ledger. A low-balance banner now appears when credit is running out.
- Added usage and credit reporting to completed chat turns when the provider supplies it, and cloud credit exhaustion now surfaces as an `insufficient_credits` error.
- Added per-model credit multiplier labels in the model picker and a cloud-first model selector with vendor logos.
- Added skill and expert package installation from the marketplace, with toast feedback, starter prompt prefill, and live installed/connected/error state.
- Added in-memory SWR caches for the Extensions catalog and Knowledge Base pages.

### Changed
- The web accent palette switched to Syscity indigo.
- Marketplace catalog language now follows the active UI language; switching languages re-syncs instead of serving stale mixed translations.
- Model router provider selection is deterministic across restarts and prefers direct non-cloud providers for duplicated model IDs.
- The model router now honors each provider's configured default model for calls that do not specify a model.
- Multi-query retrieval can be pinned to a specific provider/model and now falls back to the original query when expansion fails.

### Fixed
- Fixed `/sw.js` returning HTTP 500.
- Fixed cached-but-empty marketplace catalogs being served indefinitely; they now self-heal on the next fetch.
- Fixed in-place marketplace upgrades wiping already-cached package contents.
- Fixed failed health probes from extending circuit breaker cooldown and locking providers out after the cooldown expired.
- Fixed collection-filtered sqlite-vec KNN search by using a subquery.
- Fixed cloud provider requests being sent without the `/v1` prefix, which caused 404s and circuit breaker trips.

## [0.3.2] - 2026-09-07

### Added

- feat(ui): titlebar session context strip + PanelLeft sidebar toggle
- feat(chat): defer agent session creation to first message
- feat(ui): sidebar nav highlighting by active view
- feat(kb): document preview in resizable right panel, Titlebar toggle
- feat(kb): cloud backups toolbar chip + popover, progressive page load
- feat(kb): merge KB page with per-file backup and document viewer
- feat(kb): kb.doc_content WS method for document preview
- feat(kb): allow local_gguf embeddings for the knowledge base
- feat(kb): Backup & Sync panel replaces cloud KB management
- feat(cloud): KB backup & sync — cloud.kb.docs/push/pull orchestration over WS
- feat(kb): flat local document list with Agent column
- feat(kb): Knowledge Base view — local per-agent collections + cloud KBs
- feat(ui): show cloud avatar in AccountButton; English cloud banner
- feat(ui): New Session opens a welcome page; session created on first message
- feat(ui): merge marketplace sidebar entries into one Extensions item
- feat(ui): account button in titlebar; split marketplace entry into three
- feat(ui): move workspace toggle from composer toolbar to the Titlebar
- feat(ui): move status dot and theme toggle into the Statusbar zone
- feat(ui): move sidebar header (logo + name + toggle) into the titlebar zone
- feat(ui): align titlebar identity with the sidebar's right edge
- feat(ui): 3-row shell with Titlebar and Statusbar
- feat(desktop): overlay titlebar config + platform command
- feat(release): LLM-drafted release notes via scripts/.env config
- feat(release): full release flow in release.sh — changelog, bump, tag

### Fixed

- fix(rag): enable embeddings in local GGUF context params
- fix(kb): drop duplicated count in backup/restore result notes
- fix(rag): scope vector delete_by_source to a collection
- fix(cloud): flatten /auth/me user identity in cloud.status/cloud.token
- fix(web): wait for WS connection before submitting the OAuth callback token
- fix(cloud): rewrite asset URLs to absolute on the OAuth callback route
- fix(cloud): repair OAuth login flow (console URL + callback route)
- fix(web): substitute {VERSION} in title for dev server and prod build

### Changed

- chore(web): rename app title to "Syscity Agent"
- style(ui): dedicated bg-rail token, left rail sits a step below page
- chore: ignore .e2e-tmp scratch directory
- style(web): tighten sidebar top action spacing
- style: fix ASCII diagram alignment in README
- refactor(ui): rename MarketplaceView to ExtensionsView
- style(ui): move workspace toggle to the far right of the Titlebar
- style(ui): workspace toggle icon → lucide PanelRight
- feat(ui): pane-following chrome colors for Titlebar and Statusbar
- chore(desktop): grant core window/event capabilities

## [0.3.1] - 2026-09-04

### Highlights

- **Cloud features on by default in shipped builds, with a `--nocloud` opt-out**: release artifacts (CLI + desktop) and `./scripts/build.sh` compile the `cloud` feature by default; `syscity start --nocloud` disables cloud at runtime (propagated through background start and self-update restarts). Source `cargo build` still contains no cloud code (§2.7).

### Added

- `/goal` durability: round-level checkpoint hardening (atomic writes, TaskRegistry drain on shutdown), mid-round resume (an in-flight round continues from its checkpointed messages instead of restarting), per-goal token accounting, terminal outcome write-back to the parent session, and a `syscity goal list/resume/cancel` CLI
- Eval: suite token cost reported in TrialResult/Suite Summary, auto-appended eval ledger (`evals/ledger.md`), release-gate iteration loop recipe, and machine-enforced gate integrity (pre-commit + CI checks against threshold tampering)

### Changed

- `./scripts/build.sh` defaults to cloud features; `--nocloud` opts out and the old `--cloud` flag is removed

### Fixed

- A `[cloud]` config section without an explicit `enabled` key silently disabled cloud (serde `bool` default); it now shares the env-aware on-by-default value
- CI: publish one-line installers to GitHub Pages

## [0.3.0] - 2026-09-02

### Highlights

- **Syscity Cloud platform** (default-off `cloud` feature): account sign-in with popup + callback, a marketplace to browse and install experts/skills/connectors, cloud model / search / knowledge-base providers, device binding with usage & subscription display, and cloud-provisioned connectors — all double-gated behind the feature.
- **Document authoring**: slides canvas pipeline (`write_report format=slides` with live preview and PPTX export), full `write_document` coverage via authored-HTML docx/xlsx slices, chart/diagram generation (`svg_to_png` + `generate_chart`) with image embedding, and a document-authoring skill.
- **Closed-loop harness / eval**: turn feedback buttons with an eval dashboard, layered scorer wired into governance-weighted verdict gates, compression quality gating, human-review sampling, online monitoring, and a governed regression suite (`badcase-run`) — plus online sampling, a low-retention compression gate, feedback ops aggregation, and N=1 online shadow replay.
- **CLI migrated to WebSocket RPC**: agents, cron, skills, providers, plugin, device, admin, and audit commands now speak WS instead of REST.
- **Desktop & remote**: remote gateway connection mode (desktop and mobile), reuse of an already-running gateway, an in-web tool approval UI (modal + sidebar badge), and a WeChat Official Account channel.

### Added

- WS methods: `agents.*` (default/memory/import/export/config), `audit.*`, `cron.*`, `skills.*`, `providers.fallback`, `plugins.reload_all`, `device.pairing`, `mcp`, `security gate/allowlist/status`
- Marketplace: expert summoning, connector WS/HTTP surface + events, MCP connector abstraction (catalog sync, lifecycle DSL, bundled skills, persistent state)
- Office: slides rich text, anchoring, gradients, PPTX export; docx/xlsx authored-HTML slices; svg_to_png + generate_chart tools; docx image embedding
- Cloud: login/token/status/logout endpoints, `cloud_kb` tool, cloud search provider, OpenAI-compatible cloud model provider
- Site: per-platform download buttons (macOS Apple Silicon / Intel split), tab-switched install commands, pricing (USD on EN / RMB on zh), a Syscity Cloud section, light demo GIF with EN/中文 switcher, favicons
- Windows installer (`install.ps1`), `--cloud` build flag
- Release: CHANGELOG-driven release notes with auto-notes fallback; release artifacts published to Cloudflare R2 and served via the `syscity-releases` worker

### Changed

- Sidebar account/login moved to the top and bottom rows; composer merges image/file into one attach button; marketplace gets a full-screen view
- Desktop: reuse an already-running gateway; remote gateway connection mode with unified frontend HTTP base
- Site hero install tabs, larger platform icons, enlarged mobile screenshots

### Fixed

- CI: dropped the stale `engine_metrics` field, gated wechatmp webhooks behind the feature, hoisted `routes` above `r2_buckets` in the releases worker config
- Bumped wasmtime 45.0 → 46.0 for RUSTSEC-2026-0269

## [0.2.2] - 2026-08-24

### Highlights

- **Kernel write fences on all three desktop platforms**: `workspace_only` command tools (shell / process / code_exec) now run inside macOS Seatbelt, Linux Landlock, and Windows AppContainer + Job Object sandboxes — the agent can no longer write outside the workspace, enforced by the OS kernel, fail-closed.
- **Fresh-context goal loops ("Ralph" mode)**: `/goal --fresh` runs each round in a brand-new seedless sub-agent with a validated `handoff` JSON contract; round notes are browsable in the workspace, and interrupted goals are suspended with a structured `blocked_reason` until `/goal resume`.
- **Human-in-the-loop `ask_user` tool** with a web modal — agents can ask blocking questions mid-turn.
- **Per-turn observability**: full-fidelity turn records under `~/.syscity/turns/` plus SQLite metrics and a `syscity observe {stats,list,show,export,prune}` CLI.
- **Online self-update** for CLI, daemon, and desktop (minisign-signed updater bundles; verify-then-apply with SHA-256).

### Added

- Claude-Code-compatible shell hooks (`~/.syscity/hooks.json`): PreToolUse / PostToolUse / UserPromptSubmit / Stop with deny/ask/block decisions, fail-open by design
- Runtime invariant registry: modules register their own checks, `syscity invariants [--json]` runs them all, enforced by static analysis
- Content-addressed attachment store (`~/.syscity/attachments/sha256/…`) with dedup and `observe prune` GC
- Report artifacts now live in each agent's own workspace, served at `/api/v1/artifacts/@<agent>/…`
- Agent workspace file browser in the web UI (`workspace.list` / `workspace.read`)
- `screen_ui_detect` tool (OmniParser) with automatic fallback when the a11y tree is empty
- Post-execute tool hooks with block-with-feedback
- Cron run digest appended to `workspace/cron-log.md`
- Durable compaction: boundaries persisted, tool pairs kept, overflow retries once
- Crash-recovery: orphaned in-flight tasks marked failed at startup; session repair with `TOOL_OUTCOME_UNKNOWN` sentinel
- Eval framework in CI: deterministic YAML validation on every push, nightly `ci_smoke` smoke run against a live LLM (non-blocking, opens a tracking issue on failure), manual `release_gate` before tagging

### Changed

- Todo tool is now whole-snapshot replace (`{"todos": [...]}`); plans clear automatically on each new turn
- File writes guarded by read-before-edit version tracking
- Oversized tool outputs spill to disk (path-aware exemptions), shell keeps output tails
- Current time moved out of the system prompt into a per-request state snapshot (keeps the prompt prefix byte-stable for KV-cache reuse)
- Secrets masked on all config surfaces; `config.set` uses revision CAS (`REVISION_CONFLICT` on stale writes)
- `web_fetch`: stable error codes, per-hop redirect revalidation, non-2xx returned as result
- Unix process spawns report the terminating signal and kill the whole process group
- Grounding & honesty rules in the default system prompt: cite only tool-result facts, surface source conflicts, label prior knowledge as unverified

### Fixed

- Windows builds: gated Unix-only code paths; verified continuously via the zig cross-compile harness
- LLM judge hardened for evals: JSON recovery from prose, format-correction retry, declared-dimension gating, normalized threshold keys
- E2E tests allocate ports dynamically (no more "Failed to bind gateway" flakes)
- Desktop release pipeline: macOS deployment target pinned for llama.cpp, updater signing key rotated, updater manifests generated for tauri v2, desktop bundles collected from the workspace-root target dir

## [0.1.2] - 2026-06-11

### Added

- Initial release of Syscity AI Assistant
- Core agent architecture with tool system
- SQLite persistence for sessions and memory
- Provider abstraction supporting OpenAI and Anthropic APIs
- CLI with interactive chat mode
- Web search and fetch tools
- File operations (read, write, edit, glob)
- Shell command execution
- Code execution with Python sandbox
- Todo/task management
- Session search with FTS5
- Dual memory architecture (procedural + user model)
- Context compression strategies
- Iteration budget management
- Autonomous skill creation with security guard
- Subagent delegation
- Persistent assistant spawning
- Assistant mesh for inter-assistant communication
- MCP (Model Context Protocol) integration
- Security module with auth, allowlist, and rate limiting
- Cron scheduler for recurring tasks
- Telegram channel integration
- Discord channel integration
- Slack channel integration
- Message formatting for all channels
- Docker deployment configuration
- Systemd service configuration
- Kubernetes manifests
- GitHub Actions CI/CD workflows
- Example skills (weather, news, calculator, todo, reminder)
- Comprehensive documentation

### Changed

- Relicense project from MIT to Apache-2.0

## [0.1.1] - 2026-06-04

### Changed

- Upgrade `sqlx` 0.7 -> 0.8.6 (security fix)
- Upgrade `wasmtime` 15 -> 45.0.0 (security fix)
- Remove `rustls-pemfile` and `rsa` from dependency tree

## [0.1.0] - 2024-01-01

### Added
- Initial project setup
- Basic structure and CI
