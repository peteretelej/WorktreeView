// The backend's remote load envelope, unwrapped for the webview: local
// loads serialize as bare JSON, remote loads carry the explicit health
// state so badges render from payload fields and never from inferred
// errors.

export type RemoteLoadState = "live" | "offline" | "stale";

// The wire shape of a wrapped remote load (the backend's ReadOutcome).
export type RemoteLoad<T> = {
  state: RemoteLoadState;
  last_success_age_ms: number | null;
  message: string | null;
  data: T | null;
};

export type UnwrappedLoad<T> =
  | { remote: false; state: null; message: null; data: T }
  | { remote: true; state: RemoteLoadState; message: string | null; data: T | null };

const LOAD_STATES: readonly string[] = ["live", "offline", "stale"];

function isRemoteLoad<T>(payload: T | RemoteLoad<T>): payload is RemoteLoad<T> {
  return (
    typeof payload === "object" &&
    payload !== null &&
    !Array.isArray(payload) &&
    "state" in payload &&
    "data" in payload &&
    LOAD_STATES.includes(String((payload as { state: unknown }).state))
  );
}

export function unwrapLoad<T>(payload: T | RemoteLoad<T>): UnwrappedLoad<T> {
  if (isRemoteLoad(payload)) {
    return { remote: true, state: payload.state, message: payload.message, data: payload.data };
  }
  // The negative branch cannot exclude RemoteLoad<T> from a generic union;
  // the discriminating check above already did.
  return { remote: false, state: null, message: null, data: payload as T };
}
