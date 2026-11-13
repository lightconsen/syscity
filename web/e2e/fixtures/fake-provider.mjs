// A scriptable OpenAI-compatible provider on a random port.
//
// The point of a fake provider is that a UI test says what the *model* does
// and then asserts on what the *screen* does. Driving a real model instead
// would make every UI test also a test of model judgement — slow, paid for,
// and nondeterministic. This server answers from a script:
//
//   const p = await startFakeProvider({ script: [{ text: "hello" }] });
//   // ... drive the UI ...
//   await p.close();
//
// See `fixtures/scripts.mjs` for the shared scripts (plain text, a tool call,
// an approval-bound shell call, an error) so the same scenario can be reused
// across specs. Scripts advance one step per request: the first completion
// returns step 1, the follow-up (after the tool result is fed back) returns
// step 2, and the last step repeats.
import http from "node:http";

/**
 * One script step describes a single assistant turn.
 *
 * @typedef {object} ScriptStep
 * @property {string}  [text]        Text streamed back to the client.
 * @property {number}  [chunkDelayMs] Delay between text chunks (default 30 ms).
 * @property {number}  [chunkCount]   Split `text` into this many deltas.
 * @property {Array<{name: string, arguments: object}>} [toolCalls]
 *           Tool calls the assistant requests this turn.
 * @property {number}  [errorStatus]  Answer with this HTTP status instead.
 * @property {string}  [errorBody]    Body for the error answer.
 */

const DEFAULT_CHUNK_DELAY_MS = 30;

/**
 * Start the fake provider.
 *
 * @param {{script: ScriptStep[], port?: number, onRequest?: (body: any) => void,
 *          user?: object | null}} opts
 *   `user` answers the cloud API's `GET /auth/me` (the gateway's own call when
 *   a cloud token is submitted); `null` leaves that route 404ing, which is
 *   what a non-cloud test wants.
 * @returns {Promise<{url: string, port: number, requests: any[], close: () => Promise<void>}>}
 */
export async function startFakeProvider({ script, port = 0, onRequest, user = null } = {}) {
  if (!Array.isArray(script) || script.length === 0) {
    throw new Error("startFakeProvider needs a non-empty script");
  }

  /** Every completion request seen, so a test can assert what the agent sent. */
  const requests = [];
  let stepIndex = 0;

  const server = http.createServer((req, res) => {
    if (req.method !== "POST" || !req.url?.startsWith("/v1/chat/completions")) {
      // The gateway probes /v1/models to answer "is an LLM configured?".
      if (req.method === "GET" && req.url?.startsWith("/v1/models")) {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(
          JSON.stringify({
            object: "list",
            data: [{ id: "fake-ui-model", object: "model", owned_by: "test" }],
          }),
        );
        return;
      }
      // Cloud identity: `cloud.token` calls this with the submitted token to
      // verify it before storing it. A 401 here is a rejected token, so only
      // answer when the test asked for a signed-in user.
      if (req.method === "GET" && req.url?.startsWith("/auth/me") && user) {
        res.writeHead(200, { "Content-Type": "application/json" });
        res.end(JSON.stringify({ user }));
        return;
      }
      res.writeHead(404).end();
      return;
    }

    let raw = "";
    req.on("data", (c) => (raw += c));
    req.on("end", () => {
      let body = {};
      try {
        body = JSON.parse(raw);
      } catch {
        /* answered below as an empty request */
      }
      requests.push(body);
      onRequest?.(body);

      const step = script[Math.min(stepIndex, script.length - 1)];
      stepIndex += 1;

      if (step.errorStatus) {
        res.writeHead(step.errorStatus, { "Content-Type": "application/json" });
        res.end(step.errorBody ?? JSON.stringify({ error: { message: "scripted failure" } }));
        return;
      }

      res.writeHead(200, {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache",
        Connection: "keep-alive",
      });

      const model = body.model || "fake-ui-model";
      const send = (delta, finishReason = null, extra = {}) =>
        res.write(
          `data: ${JSON.stringify({
            id: "chatcmpl-fake-ui",
            object: "chat.completion.chunk",
            created: Math.floor(Date.now() / 1000),
            model,
            choices: [{ index: 0, delta, finish_reason: finishReason }],
            ...extra,
          })}\n\n`,
        );

      // A scripted tool call is streamed the way OpenAI streams one: the id and
      // name first, then the arguments in pieces. The gateway accumulates them.
      if (step.toolCalls?.length) {
        step.toolCalls.forEach((call, index) => {
          send({
            role: "assistant",
            tool_calls: [
              {
                index,
                id: `call_fake_${stepIndex}_${index}`,
                type: "function",
                function: { name: call.name, arguments: "" },
              },
            ],
          });
          send({
            tool_calls: [
              {
                index,
                function: { arguments: JSON.stringify(call.arguments ?? {}) },
              },
            ],
          });
        });
        send({}, "tool_calls");
        res.write("data: [DONE]\n\n");
        res.end();
        return;
      }

      const text = step.text ?? "";
      const chunkCount = step.chunkCount ?? Math.min(12, Math.max(1, text.split(" ").length));
      const chunkDelayMs = step.chunkDelayMs ?? DEFAULT_CHUNK_DELAY_MS;

      if (!text) {
        send({ role: "assistant", content: "" });
        send({}, "stop", { usage: { prompt_tokens: 1, completion_tokens: 0, total_tokens: 1 } });
        res.write("data: [DONE]\n\n");
        res.end();
        return;
      }

      // Split on word boundaries so a test can assert on "chunk3 " arriving
      // mid-turn rather than on a byte count.
      const words = text.split(/(?<=\s)/);
      const perChunk = Math.max(1, Math.ceil(words.length / chunkCount));
      const chunks = [];
      for (let i = 0; i < words.length; i += perChunk) {
        chunks.push(words.slice(i, i + perChunk).join(""));
      }

      send({ role: "assistant", content: "" });
      let i = 0;
      const timer = setInterval(() => {
        if (i < chunks.length) {
          send({ content: chunks[i] });
          i += 1;
          return;
        }
        clearInterval(timer);
        send({}, "stop", {
          usage: { prompt_tokens: 10, completion_tokens: chunks.length, total_tokens: 10 + chunks.length },
        });
        res.write("data: [DONE]\n\n");
        res.end();
      }, chunkDelayMs);

      // `res`, not `req`: the request body is already fully read, so clearing
      // on `req` would cut the stream off before a single chunk.
      res.on("close", () => clearInterval(timer));
    });
  });

  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", resolve);
  });

  const actualPort = server.address().port;
  return {
    url: `http://127.0.0.1:${actualPort}/v1`,
    port: actualPort,
    requests,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}
