/**
 * Single source of truth for the gateway base URL, shared by the WebSocket
 * transport and every HTTP API call.
 *
 * - Browser: the page is served by the gateway, so the base is the current
 *   origin (same-origin).
 * - Tauri (desktop/mobile): the WebView loads embedded assets, so the base is
 *   resolved from the backend via `get_api_url` — the local gateway port, or
 *   the configured remote gateway in remote mode.
 *
 * HTTP paths like `/api/v1/...` are resolved against this base, and the
 * gateway token (mobile / remote mode) is attached as a Bearer header.
 */

let cachedBase: string | null = null;

/** Set (and normalize) the current gateway base URL. */
export function setGatewayBase(url: string): void {
  cachedBase = url.replace(/\/+$/, "");
}

/** Current gateway base URL (http scheme), for building API URLs. */
export function getGatewayBase(): string {
  if (cachedBase) return cachedBase;
  if (typeof window !== "undefined" && "__TAURI__" in window) {
    // Tauri: resolved async by initGatewayBase(); fall back to loopback.
    return "http://127.0.0.1:18080";
  }
  if (typeof window !== "undefined") {
    // Browser: honor a stored remote gateway base; otherwise same-origin
    // (the page is served by the gateway it talks to).
    const stored = localStorage.getItem("syscity_gateway_base");
    return stored ? stored.replace(/\/+$/, "") : window.location.origin;
  }
  return "http://127.0.0.1:18080";
}

/**
 * Resolve the gateway base from the Tauri backend and cache it. In the
 * browser this reads the stored remote base (or same-origin). Returns the
 * base.
 */
export async function initGatewayBase(): Promise<string> {
  if (cachedBase) return cachedBase;
  const isTauri = typeof window !== "undefined" && "__TAURI__" in window;
  let base = "http://127.0.0.1:18080";
  if (isTauri) {
    try {
      const { invoke } = await import("@tauri-apps/api/core");
      const apiUrl = await invoke<string>("get_api_url");
      if (apiUrl) base = apiUrl;
    } catch {
      // Fall back to loopback.
    }
  } else if (typeof window !== "undefined") {
    base = localStorage.getItem("syscity_gateway_base") || window.location.origin;
  }
  setGatewayBase(base);
  return base;
}

let cachedToken: string | null = null;

/**
 * Remember a gateway token the *host* supplied, for this session only.
 *
 * The mobile build hands the per-install token over through the Tauri command
 * (`get_gateway_token`), and the host can hand it over again after any reload —
 * so it lives in memory rather than in `localStorage`, where it would outlive
 * the session and be readable by anything running in the WebView. A token the
 * *user* typed (see `ConnectionSettings`) is a different case: nothing else can
 * re-supply it, so that one stays in `localStorage` and is read as a fallback.
 */
export function setGatewayToken(token: string | null): void {
  cachedToken = token;
}

/**
 * Gateway token for HTTP requests, if any.
 *
 * The host-supplied token wins: on mobile it is the credential for the app's
 * own gateway. Otherwise fall back to the one the user saved.
 */
export function getGatewayToken(): string | null {
  if (cachedToken) return cachedToken;
  return typeof localStorage !== "undefined"
    ? localStorage.getItem("syscity_gateway_token")
    : null;
}

/**
 * Fetch against the current gateway: prepends the gateway base to relative
 * paths and attaches the gateway token as a Bearer header.
 */
export async function apiFetch(
  input: string | URL,
  init?: RequestInit,
): Promise<Response> {
  let url = typeof input === "string" ? input : input.toString();
  if (url.startsWith("/")) {
    url = `${getGatewayBase()}${url}`;
  }
  const headers = new Headers(init?.headers);
  const token = getGatewayToken();
  if (token) headers.set("Authorization", `Bearer ${token}`);
  return fetch(url, { ...init, headers });
}
