import {
  filterBuildAvailableFeatures,
  isFeatureBuildAvailable,
  type FeatureBuildFlags,
} from "./buildAvailability";
import { desktopFeatures } from "./manifest";
import { resolveEnabled } from "./resolveEnabled";
import type { FeatureDefinition } from "./types";

const runtimeBuildFlags: FeatureBuildFlags = {
  bestie: import.meta.env?.VITE_BUZZ_BESTIE === "1",
};

export function isFeatureAvailableInThisBuild(
  feature: Parameters<typeof isFeatureBuildAvailable>[0],
): boolean {
  return isFeatureBuildAvailable(feature, runtimeBuildFlags);
}

/** Canonical preview resolution for the running build. */
export function resolveFeatureEnabledInThisBuild(
  feature: FeatureDefinition,
  overrides: Record<string, boolean>,
): boolean {
  return resolveEnabled(
    feature,
    overrides,
    isFeatureAvailableInThisBuild(feature),
  );
}

export const availableDesktopFeatures = filterBuildAvailableFeatures(
  desktopFeatures,
  runtimeBuildFlags,
);
