// Typed wrappers over the Tauri commands in app/src-tauri/src/commands.rs and login_flow.rs.
import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

export type AccountState = "active" | "parked" | "needs_login" | "paused";

export type WindowKind = "session" | "weekly" | "other";

/** One usage window of an account's latest reading, as it stands now. */
export interface WindowUsage {
  name: string;
  /** Mid-sentence name: "5-hour", "weekly", "Opus weekly". */
  label: string;
  kind: WindowKind;
  /** The model family the window is limited to; null for all models. */
  model: string | null;
  /** The last reading, or 0 once the window has reset. */
  pct: number;
  resets_at: number | null;
  threshold: number;
  /** Whether the window limits the model the rotation profile is using. */
  applies: boolean;
}

export type WeeklyLimit = "none" | "unknown" | "has";
export type WeeklyLimitSetting = "auto" | "yes" | "no";

export interface SampleWindow {
  name: string;
  pct: number;
  resets_at: number | null;
}

export interface Sample {
  ts: number;
  account_id: number;
  profile: string;
  session_id: string | null;
  model: string | null;
  windows: SampleWindow[];
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
  /** Place in the user's order: lower is used first. */
  position: number;
  weekly_limit: WeeklyLimitSetting;
  added_at: number;
  last_login_at: number | null;
}

/** What cc-rotate remembers about an account's usage checks (Claude Code's `/usage`). */
export interface CheckState {
  last_attempt_at: number | null;
  last_read_at: number | null;
  /** No automatic check before this (epoch ms). */
  not_before: number | null;
  /** Why the last check failed. */
  problem: string | null;
  /** Not used until a check settles whether its login still works. */
  held_back: boolean;
}

export type CheckOutcome =
  | { outcome: "read"; windows: SampleWindow[] }
  | { outcome: "skipped"; why: string }
  | { outcome: "rate_limited"; until: number }
  | { outcome: "login_rejected" }
  | { outcome: "failed"; why: string };

export interface AccountView extends Account {
  /** "Max 5x", "Max 20x", "Team 5x", "Pro", or the raw subscription type. */
  plan: string | null;
  windows: WindowUsage[];
  weekly: { setting: WeeklyLimitSetting; detected: WeeklyLimit; effective: WeeklyLimit };
  last_sample_at: number | null;
  /** `rotate`, `check`, or the folder of another profile (`.claude-work`) that reported the reading. */
  last_sample_source: string | null;
  usage_check: CheckState;
  login_expires_at: number | null;
  login_updated_at: number | null;
  refresh_expires_at: number | null;
  eligible: boolean;
  available_at: number | null;
  /** Place in the pick order among the accounts usable now. */
  rank: number | null;
  reason: string;
}

export interface Ranked {
  account_id: number;
  label: string;
  rank: number | null;
  eligible: boolean;
  reason: string;
}

export interface Settings {
  swap_threshold_5h: number;
  swap_threshold_7d: number;
  swap_cooldown_s: number;
  prefer_no_weekly_limit: boolean;
  switch_back_dwell_s: number;
  lock_timeout_ms: number;
  sample_retention_days: number;
  statusline_downstream: string | null;
  profile_dir: string | null;
  claude_path: string | null;
  usage_checks: boolean;
  usage_check_interval_s: number;
  usage_check_max_age_s: number;
}

export interface Snapshot {
  now_ms: number;
  active_id: number | null;
  /** In the user's order. */
  accounts: AccountView[];
  ranking: Ranked[];
  next: { account_id: number; label: string; reason: string } | null;
  model: string | null;
  pool: {
    weekly_accounts_left: number;
    weekly_limited: number;
    no_weekly_limit: number;
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
  checkUsage: (id: number) => invoke<CheckOutcome>("check_usage", { id }),
  setPaused: (id: number, paused: boolean) => invoke<void>("set_paused", { id, paused }),
  removeAccount: (id: number) => invoke<void>("remove_account", { id }),
  renameAccount: (id: number, label: string) => invoke<void>("rename_account", { id, label }),
  setOrder: (ids: number[]) => invoke<void>("set_order", { ids }),
  setWeeklyLimit: (id: number, value: WeeklyLimitSetting) => invoke<void>("set_weekly_limit", { id, value }),
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
