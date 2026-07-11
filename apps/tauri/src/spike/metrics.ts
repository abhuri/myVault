export function percentile(samples: readonly number[], quantile: number): number | null {
  if (samples.length === 0) return null;
  const bounded = Math.min(1, Math.max(0, quantile));
  const sorted = [...samples].sort((left, right) => left - right);
  return sorted[Math.ceil(bounded * sorted.length) - 1] ?? sorted[0];
}

export function formatMilliseconds(value: number | null): string {
  return value === null ? "waiting" : `${value.toFixed(1)} ms`;
}
