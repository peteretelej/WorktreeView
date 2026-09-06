export const ZOOM_LEVELS = [0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2];
export const DEFAULT_ZOOM = 1;

export type ZoomShortcut = "in" | "out" | "reset";

// Maps arbitrary stored or computed factors back onto a supported level;
// non-finite values fall back to the default.
export function snapZoom(value: number): number {
  if (!Number.isFinite(value)) return DEFAULT_ZOOM;
  return ZOOM_LEVELS.reduce((best, level) => (Math.abs(level - value) < Math.abs(best - value) ? level : best));
}

export function stepZoom(current: number, direction: 1 | -1): number {
  const index = ZOOM_LEVELS.indexOf(snapZoom(current));
  return ZOOM_LEVELS[Math.min(ZOOM_LEVELS.length - 1, Math.max(0, index + direction))];
}

// Matches the desktop-standard zoom keys, including the shifted (+, _) and
// numpad (+, -, 0) spellings of each shortcut.
export function zoomShortcut(key: string): ZoomShortcut | null {
  if (key === "=" || key === "+") return "in";
  if (key === "-" || key === "_") return "out";
  if (key === "0") return "reset";
  return null;
}
