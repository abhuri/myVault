import { describe, expect, it } from "vitest";
import { assertAllowedSpikeFolder, isAllowedSpikeFolderName } from "./fixture";

describe("Phase 0 Drive fixture guard", () => {
  it("accepts a dated fixture folder with a random suffix", () => {
    expect(isAllowedSpikeFolderName("myVault-spike-2026-07-11-a1b2c3")).toBe(true);
  });

  it.each([
    "myVault",
    "myVault-spike",
    "myVault-spike-2026-07-11-short",
    "personal-vault-2026-07-11-a1b2c3",
    "myVault-spike-2026-7-11-a1b2c3",
  ])("rejects a non-allowlisted folder: %s", (name) => {
    expect(isAllowedSpikeFolderName(name)).toBe(false);
    expect(() => assertAllowedSpikeFolder(name)).toThrow(/Refusing to operate/);
  });
});
