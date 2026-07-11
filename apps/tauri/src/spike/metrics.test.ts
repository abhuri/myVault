import { describe, expect, it } from "vitest";
import { formatMilliseconds, percentile } from "./metrics";

describe("Phase 0 performance metrics", () => {
  it("calculates p95 without mutating samples", () => {
    const samples = [5, 1, 4, 3, 2];
    expect(percentile(samples, 0.95)).toBe(5);
    expect(samples).toEqual([5, 1, 4, 3, 2]);
  });

  it("handles empty and out-of-range inputs", () => {
    expect(percentile([], 0.95)).toBeNull();
    expect(percentile([1, 2, 3], 2)).toBe(3);
    expect(percentile([1, 2, 3], -1)).toBe(1);
    expect(formatMilliseconds(null)).toBe("waiting");
  });
});
