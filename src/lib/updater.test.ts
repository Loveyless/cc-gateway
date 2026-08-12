import { describe, expect, it } from "vitest";
import { checkForUpdate, UPDATES_ENABLED } from "./updater";

describe("maintenance fork updater", () => {
  it("stays disabled until a fork-owned release pipeline is configured", async () => {
    expect(UPDATES_ENABLED).toBe(false);
    await expect(checkForUpdate()).resolves.toEqual({ status: "up-to-date" });
  });
});
