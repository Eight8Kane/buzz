import { expect, test } from "@playwright/test";

import { installMockBridge } from "../helpers/bridge";
import { FEATURE_OVERRIDES_STORAGE_KEY } from "../helpers/features";

const BESTIE_PUBKEY =
  "be571e0000000000000000000000000000000000000000000000000000000000";

const bestie = {
  avatarUrl: null,
  name: "Bestie",
  personaId: "builtin:bestie",
  pubkey: BESTIE_PUBKEY,
  status: "running" as const,
};

test("the enabled Bestie experiment adds a direct-message entry below Agents", async ({
  page,
}) => {
  await installMockBridge(page, { managedAgents: [bestie] });
  await page.goto("/");

  const agentsEntry = page.getByTestId("open-agents-view");
  const bestieEntry = page.getByTestId("open-bestie-dm");
  await expect(bestieEntry).toBeVisible();
  await expect(bestieEntry).toContainText("Bestie");

  const [agentsBox, bestieBox] = await Promise.all([
    agentsEntry.boundingBox(),
    bestieEntry.boundingBox(),
  ]);
  expect(agentsBox).not.toBeNull();
  expect(bestieBox).not.toBeNull();
  expect(bestieBox?.y).toBeGreaterThan(agentsBox?.y ?? 0);

  await bestieEntry.click();
  await expect(page.getByTestId("chat-title")).toHaveText("Bestie");
});

test("the disabled Bestie experiment does not mount the sidebar entry", async ({
  page,
}) => {
  await installMockBridge(page, { managedAgents: [bestie] });
  await page.addInitScript((key) => {
    const overrides = JSON.parse(
      window.localStorage.getItem(key) ?? "{}",
    ) as Record<string, boolean>;
    overrides.bestie = false;
    window.localStorage.setItem(key, JSON.stringify(overrides));
  }, FEATURE_OVERRIDES_STORAGE_KEY);
  await page.goto("/");

  await expect(page.getByTestId("open-bestie-dm")).toHaveCount(0);
});
