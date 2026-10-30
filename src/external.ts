import { invoke } from "@tauri-apps/api/core";

// Markdown links must never navigate the webview itself: the app has no
// back path out of a foreign page and none of the user's web session.
export async function openExternalLink(url: string): Promise<boolean> {
  try {
    await invoke("open_url", { url });
    return true;
  } catch {
    return false;
  }
}
