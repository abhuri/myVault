const SPIKE_FOLDER_PATTERN = /^myVault-spike-\d{4}-\d{2}-\d{2}-[a-z0-9]{6,}$/;

export function isAllowedSpikeFolderName(name: string): boolean {
  return SPIKE_FOLDER_PATTERN.test(name);
}

export function assertAllowedSpikeFolder(name: string): void {
  if (!isAllowedSpikeFolderName(name)) {
    throw new Error("Refusing to operate outside an allowlisted Phase 0 fixture folder");
  }
}

