import { test, expect } from "./fixtures/app.mjs";
import { readConfig } from "./fixtures/gateway.mjs";
import { plainText } from "./fixtures/scripts.mjs";

/**
 * The composer's permission picker, and the one thing it can turn on.
 *
 * `bypass` is gated behind `[permissions].allow_bypass` — a gateway-wide knob
 * that defaults to off so a stray "approve everything" is a deliberate act.
 * The picker must let a person take that step from the composer rather than
 * editing `config.toml` by hand, and it must say so before it happens.
 */
test.describe("permission picker", () => {
  test("bypass is locked until the picker enables it, then applies", async ({ ui }) => {
    const app = await ui.launch(plainText("ok"));
    const picker = app.page.getByRole("button", { name: "Permission mode" });

    // Default posture: the config flag is off on a fresh gateway.
    await expect(picker).toContainText("Ask when needed");

    await picker.click();
    const bypass = app.page.getByTestId("mode-option-bypass");
    await expect(bypass).toBeVisible();
    await expect(bypass).toHaveAttribute("data-locked", "true");
    // A locked row explains what clicking it does, rather than looking broken.
    await expect(bypass).toContainText("Turn on bypass");

    // One click: flips the config flag (persisted) and switches this session.
    await bypass.click();

    await expect(picker).toContainText("Bypass permissions");
    // The row is no longer an unlock action; it is an ordinary option.
    await picker.click();
    await expect(app.page.getByTestId("mode-option-bypass")).toHaveAttribute(
      "data-locked",
      "false",
    );
    await expect(app.page.getByTestId("mode-option-bypass")).toContainText("Never ask");

    // The gateway persisted the flag, not just the session override: the
    // whole point of doing this from the composer is that it survives a
    // restart, exactly like editing the file would.
    const config = await readConfig(app.gateway.home);
    expect(config).toMatch(/^\[permissions\]/m);
    expect(config).toMatch(/^allow_bypass\s*=\s*true$/m);

    // Leaving the modal open is fine: Escape closes it without answering.
    await app.page.keyboard.press("Escape");
    await expect(bypass).toHaveCount(0);
  });
});
