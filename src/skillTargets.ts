// The bundled skill's install targets: what the app detected, and the
// one decision the install prompt makes from it. Pure module (no Tauri
// imports) so the prompt logic stays unit-testable.

export type SkillTargetStatus = "not_installed" | "up_to_date" | "differs";
export type SkillTarget = { path: string; label: string; status: SkillTargetStatus };

// The prompt speaks for the detected targets: nothing installed yet, or
// any copy diverging from the bundled skill. The key folds the whole
// target state in, so a dismissal sticks until the state actually
// changes (a new app version, a new install) and silence is the default.
export function skillPromptOf(
  targets: SkillTarget[],
  dismissedKey: string,
): { visible: boolean; mode: "install" | "update"; key: string } {
  const key = targets.map((target) => `${target.path}:${target.status}`).sort().join("|");
  const nothingInstalled = targets.length > 0 && targets.every((target) => target.status === "not_installed");
  const anyDiffers = targets.some((target) => target.status === "differs");
  const mode = anyDiffers ? "update" : nothingInstalled ? "install" : null;
  return { visible: mode !== null && key !== dismissedKey, mode: mode ?? "install", key };
}
