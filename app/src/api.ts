// Typed wrappers over the Tauri commands in app/src-tauri/src/commands.rs and login_flow.rs.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type AccountState = "active" | "parked" | "needs_login" | "paused";

export interface Usage {
  five_hour: number;
  weekly: number;
}

export interface Sample {
  ts: number;
  account_id: number;
  profile: string;
  session_id: string | null;
  five_pct: number | null;
  five_resets_at: number | null;
  week_pct: number | null;
  week_resets_at: number | null;
}

export interface Account {
  id: number;
  label: string;
  email: string | null;
  account_uuid: string | null;
  org_uuid: string | null;
  org_name: string | null;
  subscription_type: string | null;
  rate_limit_tier: string | null;
  state: AccountState;
  priority: number;
  added_at: number;
  last_login_at: number | null;
}

export interface AccountView extends Account {
  usage: Usage;
  last_sample: Sample | null;
  login_expires_at: number | null;
  login_updated_at: number | null;
  refresh_expires_at: number | null;
  eligible: boolean;
  available_at: number | null;
}

export interface Settings {
  swap_threshold_5h: number;
  swap_threshold_7d: number;
  swap_cooldown_s: number;
  lock_timeout_ms: number;
  sample_retention_days: number;
  statusline_downstream: string | null;
  profile_dir: string | null;
  claude_path: string | null;
}

export interface Snapshot {
  now_ms: number;
  active_id: number | null;
  accounts: AccountView[];
  pool: {
    weekly_accounts_left: number;
    usable: number;
    eligible: number;
    exhausted: boolean;
    next_available_at: number | null;
    swaps_24h: number;
    needs_login: string[];
  };
  settings: Settings;
  profile_dir: string;
  data_dir: string;
  last_event_id: number;
}

export interface EventRow {
  id: number;
  ts: number;
  kind: string;
  account_from: number | null;
  account_to: number | null;
  reason: string | null;
  detail: Record<string, unknown> | null;
}

export interface Check {
  name: string;
  status: "ok" | "warn" | "fail";
  detail: string;
}

export interface AppInfo {
  version: string;
  claude_version: string | null;
  tested_versions: string[];
  profile_dir: string;
  data_dir: string;
}

export interface FoundProfile {
  dir: string;
  name: string;
  email: string | null;
  account_uuid: string | null;
  subscription_type: string | null;
  signed_in: boolean;
  in_pool_as: string | null;
}

export type LoginStage = "starting" | "browser" | "importing" | "done" | "failed" | "cancelled";

export interface LoginProgress {
  id: string;
  stage: LoginStage;
  url: string | null;
  message: string | null;
  account: Account | null;
  activated: boolean;
}

export type SwapOutcome =
  | { outcome: "swapped"; from: number | null; to: number }
  | { outcome: "skipped"; why: string }
  | { outcome: "pool_exhausted" };

export const api = {
  snapshot: () => invoke<Snapshot>("snapshot"),
  series: (sinceMs: number) => invoke<Sample[]>("series", { sinceMs }),
  events: (limit: number) => invoke<EventRow[]>("events", { limit }),
  swapNow: (to: number | null) => invoke<SwapOutcome>("swap_now", { to }),
  setPaused: (id: number, paused: boolean) => invoke<void>("set_paused", { id, paused }),
  removeAccount: (id: number) => invoke<void>("remove_account", { id }),
  renameAccount: (id: number, label: string) => invoke<void>("rename_account", { id, label }),
  doctor: () => invoke<Check[]>("doctor"),
  setSetting: (key: string, value: string | null) => invoke<void>("set_setting", { key, value }),
  appInfo: () => invoke<AppInfo>("app_info"),
  autostartEnabled: () => invoke<boolean>("autostart_enabled"),
  setAutostart: (enabled: boolean) => invoke<void>("set_autostart", { enabled }),
  openUrl: (url: string) => invoke<void>("open_url", { url }),
  startLogin: (email: string | null, label: string | null, relogin: number | null, expectEmail: string | null = null) =>
    invoke<string>("start_login", { email, label, relogin, expectEmail }),
  otherProfiles: () => invoke<FoundProfile[]>("other_profiles"),
  submitLoginCode: (id: string, code: string) => invoke<void>("submit_login_code", { id, code }),
  cancelLogin: (id: string) => invoke<void>("cancel_login", { id }),
};

export const onStoreChanged = (fn: () => void): Promise<UnlistenFn> => listen("store-changed", () => fn());
export const onLoginProgress = (fn: (p: LoginProgress) => void): Promise<UnlistenFn> =>
  listen<LoginProgress>("login-progress", (e) => fn(e.payload));
