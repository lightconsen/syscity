import { test, expect } from "./fixtures/app.mjs";
import { plainText } from "./fixtures/scripts.mjs";

/**
 * First launch, before any onboarding — the one place the connection setting
 * was previously unreachable.
 *
 * Settings → Connection has always held local/remote, host, port, TLS and a
 * token, and `connection.rs` persists it to `~/.syscity/client.toml`. But
 * Settings is rendered *after* the launch gates, and both gates need a
 * working connection to answer: a fresh desktop pointed at a gateway that is
 * down (or at the wrong one) sat on the startup screen forever, and a fresh
 * desktop with no model could not even be told where to get one. Editing
 * `client.toml` by hand was the only way out.
 *
 * These run against the `tauriShell` fixture: the entry points are desktop
 * only, and what they ultimately do is call two Tauri commands.
 */
test.describe("first-launch gateway connection", () => {
  test("the welcome screen can open the connection form", async ({ ui, tauriShell }) => {
    // A gateway with no provider: `models.list` is empty, so the launch gate
    // is the Welcome screen rather than the chat.
    const app = await ui.launch(plainText("ok"), {
      skipModel: true,
      skipOnboarding: false,
      waitForChat: false,
    });

    const link = app.page.getByTestId("welcome-connect-gateway");
    await expect(link).toBeVisible({ timeout: 30_000 });
    await link.click();

    const dialog = app.page.getByTestId("connect-gateway-dialog");
    await expect(dialog).toBeVisible();
    // It is the real Connection form, not a copy of it: these labels come
    // from ConnectionSettings and would be missing from a hand-rolled clone.
    await expect(dialog.getByText("Local (embedded / reuse)")).toBeVisible();
    await expect(dialog.getByText("Remote gateway")).toBeVisible();

    // Escape closes it and leaves the welcome gate intact, so a person who
    // opened it by accident loses nothing.
    await app.page.keyboard.press("Escape");
    await expect(dialog).toHaveCount(0);
    await expect(link).toBeVisible();
    expect(tauriShell).toBeTruthy();
  });

  test("a remote gateway and its token are saved from that form", async ({
    ui,
    tauriShell,
  }) => {
    const app = await ui.launch(plainText("ok"), {
      skipModel: true,
      skipOnboarding: false,
      waitForChat: false,
    });
    await app.page.getByTestId("welcome-connect-gateway").click();

    const dialog = app.page.getByTestId("connect-gateway-dialog");
    await dialog.getByRole("radio", { name: "Remote gateway" }).check();
    await dialog.getByLabel("Host", { exact: true }).fill("192.168.1.10");
    await dialog.getByLabel("Port", { exact: true }).fill("18080");
    // The token field carries the agent-token promise: an agent access token
    // is accepted here, and the gateway — not this app — decides which agent
    // it binds to (`bound_agent` in the connect handshake).
    const tokenField = dialog.getByLabel(
      "Token (the remote gateway's shared token, or an agent token)",
      { exact: true },
    );
    await tokenField.fill("e2e-agent-token");

    await dialog.getByRole("button", { name: "Save", exact: true }).click();
    // Saving offers the restart that applies it — the step that used to be
    // "quit the app and open it again" with no button to say so.
    const restart = dialog.getByRole("button", { name: "Save & restart" });
    await expect(restart).toBeVisible();
    await expect(
      dialog.getByText("Saved — restart the app to apply the new connection."),
    ).toBeVisible();

    const saved = await tauriShell.saved();
    expect(saved).toHaveLength(1);
    expect(saved[0]).toMatchObject({
      mode: "remote",
      host: "192.168.1.10",
      port: 18080,
      tls: false,
      token: "e2e-agent-token",
    });
  });

  test("an impossible remote config is refused, not written", async ({
    ui,
    tauriShell,
  }) => {
    const app = await ui.launch(plainText("ok"), {
      skipModel: true,
      skipOnboarding: false,
      waitForChat: false,
    });
    await app.page.getByTestId("welcome-connect-gateway").click();

    const dialog = app.page.getByTestId("connect-gateway-dialog");
    await dialog.getByRole("radio", { name: "Remote gateway" }).check();
    // Clear the host — the field arrives prefilled with `127.0.0.1`, so
    // switching to Remote alone is a valid config. An empty host is the case
    // that cannot connect, and remote mode has no local fallback, so this
    // would park the next launch on a spinner with no way back in.
    await dialog.getByLabel("Host", { exact: true }).fill("");
    await dialog.getByRole("button", { name: "Save", exact: true }).click();

    await expect(dialog.getByText(/needs a host/i)).toBeVisible();
    // Refused *before* the write: nothing reached the shell, so the previous
    // (working) `client.toml` is still the one on disk.
    expect(await tauriShell.saved()).toHaveLength(0);
    // …and no restart is offered for a change that was never made.
    await expect(dialog.getByRole("button", { name: "Save & restart" })).toHaveCount(0);
  });

  test("a gateway that never answers can still be reconfigured", async ({
    ui,
    tauriShell,
  }) => {
    // The mocked shell points the socket at a port nothing listens on, which
    // is the remote-gateway-down case: no handshake, so `needsSetup` never
    // resolves and the app stays on the startup screen.
    const app = await ui.launch(plainText("ok"), {
      waitForChat: false,
      apiUrl: "http://127.0.0.1:1",
    });
    expect(tauriShell).toBeTruthy();

    // The escape hatch appears only after the wait has stopped looking like
    // a normal start (the same 6 s gate as the "still connecting" hint).
    const check = app.page.getByTestId("starting-check-connection");
    await expect(check).toBeVisible({ timeout: 20_000 });
    await check.click();

    const dialog = app.page.getByTestId("connect-gateway-dialog");
    await expect(dialog).toBeVisible();
    // And it is usable without a connection: the form reads its values from
    // the Tauri command, not from the gateway.
    await expect(dialog.getByRole("radio", { name: "Remote gateway" })).toBeVisible();
  });
});
