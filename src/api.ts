import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import type {
  AgentLog,
  Bootstrap,
  ChatThread,
  DoctorReport,
  GlobalIndex,
  KeyStatus,
  LocalModel,
  Man,
  ModelCatalog,
  MasterDecision,
  PendingAction,
  Profile,
  RunOutput,
  SearchHit,
  Settings,
} from "./types";

export const AGENT_EVENT = "velvetdesk://agent";
export const MODEL_EVENT = "velvetdesk://model";

export interface RunInput {
  model_id: string;
  man_id?: string | null;
  mode?: string;
  security?: string;
  message: string;
  channel?: string;
  log_incoming?: boolean;
}

export const api = {
  bootstrap: () => invoke<Bootstrap>("bootstrap"),

  listProfiles: () => invoke<Profile[]>("list_profiles"),
  createProfile: (input: {
    name: string;
    id?: string;
    age?: number;
    site?: string;
    avatar?: string;
    bio?: string;
    system_prompt_override?: string;
    languages?: string[];
    tone_rules?: string[];
    banned_phrases?: string[];
  }) => invoke<Profile>("create_profile", { input }),
  getProfile: (modelId: string) => invoke<Profile>("get_profile", { modelId }),
  saveProfile: (profile: Profile) => invoke<Profile>("save_profile", { profile }),
  deleteProfile: (modelId: string) => invoke<void>("delete_profile", { modelId }),

  listMen: (modelId: string) => invoke<Man[]>("list_men", { modelId }),
  getMan: (modelId: string, manId: string) => invoke<Man>("get_man", { modelId, manId }),
  saveMan: (man: Man) => invoke<Man>("save_man", { man }),
  createMan: (modelId: string, args: Record<string, unknown>) =>
    invoke<Man>("create_man", { modelId, args }),
  deleteMan: (modelId: string, manId: string) => invoke<void>("delete_man", { modelId, manId }),

  getChat: (modelId: string, manId: string) => invoke<ChatThread>("get_chat", { modelId, manId }),
  appendMessage: (input: {
    model_id: string;
    man_id: string;
    role: string;
    channel?: string;
    text: string;
  }) => invoke<ChatThread>("append_message", { input }),

  getAgentLog: (modelId: string) => invoke<AgentLog>("get_agent_log", { modelId }),
  clearAgentLog: (modelId: string) => invoke<void>("clear_agent_log", { modelId }),

  runAgent: (input: RunInput) => invoke<RunOutput>("run_agent", { input }),
  masterRoute: (raw: string, autoCreate: boolean) =>
    invoke<MasterDecision>("master_route", { raw, autoCreate }),
  globalSearch: (query: string) => invoke<SearchHit[]>("global_search", { query }),
  rebuildIndex: () => invoke<GlobalIndex>("rebuild_index"),

  pendingList: () => invoke<PendingAction[]>("pending_list"),
  pendingApprove: (id: string) => invoke<PendingAction>("pending_approve", { id }),
  pendingReject: (id: string) => invoke<void>("pending_reject", { id }),
  pendingClear: () => invoke<void>("pending_clear"),

  doctorScan: () => invoke<DoctorReport>("doctor_scan"),
  doctorFix: () => invoke<DoctorReport>("doctor_fix"),

  getSettings: () => invoke<Settings>("get_settings"),
  saveSettings: (settings: Settings) => invoke<Settings>("save_settings", { settings }),

  listKeys: (providerId: string) => invoke<KeyStatus[]>("list_keys", { providerId }),
  setKeys: (providerId: string, keys: string[]) =>
    invoke<KeyStatus[]>("set_keys", { providerId, keys }),
  addKey: (providerId: string, key: string) => invoke<KeyStatus[]>("add_key", { providerId, key }),
  removeKey: (providerId: string, index: number) =>
    invoke<KeyStatus[]>("remove_key", { providerId, index }),
  listProviderModels: (providerId: string) =>
    invoke<ModelCatalog>("list_provider_models", { providerId }),
  transcribe: (audioBase64: string, mime: string) =>
    invoke<string>("transcribe", { audioBase64, mime }),
  listLocalModels: () => invoke<LocalModel[]>("list_local_models"),
  downloadLocalModel: (modelId: string) => invoke<LocalModel>("download_local_model", { modelId }),
  deleteLocalModel: (modelId: string) => invoke<LocalModel[]>("delete_local_model", { modelId }),
  localModelsBaseUrl: () => invoke<string>("local_models_base_url"),
  testProvider: () => invoke<Record<string, unknown>>("test_provider"),

  seedDemo: () => invoke<Profile[]>("seed_demo"),
};

export function onAgentEvent(handler: (payload: Record<string, unknown>) => void) {
  return listen<Record<string, unknown>>(AGENT_EVENT, (event) => handler(event.payload));
}

export function onModelEvent(handler: (payload: Record<string, unknown>) => void) {
  return listen<Record<string, unknown>>(MODEL_EVENT, (event) => handler(event.payload));
}

export function errorText(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return JSON.stringify(error);
}
