import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { t } from "./i18n";
import type {
  Backup,
  ContextStats,
  LettersOutput,
  MasterOutput,
  TrustedRoot,
  AgentLog,
  Bootstrap,
  ChatThread,
  DoctorReport,
  GlobalIndex,
  KeyStatus,
  LocalModel,
  Man,
  ModelCatalog,
  PendingAction,
  Profile,
  RunOutput,
  SearchHit,
  Settings,
  UpdateInfo,
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
  /** Overrides the provider's thinking level for this run only. */
  thinking_effort?: string;
  /** Act, but keep nothing in the chat log. */
  temporary?: boolean;
  /** Screenshots and photos attached to this message. */
  images?: ImagePart[];
  /** Names this run, so its progress events can be told from another chat's. */
  run_id?: string;
}

/** An attached picture: its type, and its bytes without the `data:` prefix. */
export interface ImagePart {
  mime: string;
  data: string;
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
    writing_samples?: string[];
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

  getAgentLog: (modelId: string, manId?: string | null) =>
    invoke<AgentLog>("get_agent_log", { modelId, manId }),
  clearAgentLog: (modelId: string, manId?: string | null) =>
    invoke<void>("clear_agent_log", { modelId, manId }),

  runAgent: (input: RunInput) => invoke<RunOutput>("run_agent", { input }),
  writeLetters: (input: {
    model_id: string;
    man_ids?: string[];
    brief?: string;
    channel?: string;
    thinking_effort?: string;
    temporary?: boolean;
    run_id?: string;
  }) => invoke<LettersOutput>("write_letters", { input }),

  masterChat: (input: {
    message: string;
    security?: string;
    thinking_effort?: string;
    temporary?: boolean;
    images?: ImagePart[];
    run_id?: string;
  }) => invoke<MasterOutput>("master_chat", { input }),
  masterContextStats: () => invoke<ContextStats>("master_context_stats"),
  getMasterLog: () => invoke<AgentLog>("get_master_log"),
  clearMasterLog: () => invoke<void>("clear_master_log"),

  listTrustedRoots: () => invoke<TrustedRoot[]>("list_trusted_roots"),
  trustFolder: (path: string, writable = true) =>
    invoke<TrustedRoot[]>("trust_folder", { path, writable }),
  revokeFolder: (path: string) => invoke<TrustedRoot[]>("revoke_folder", { path }),
  listBackups: () => invoke<Backup[]>("list_backups"),
  restoreBackup: (backupId: string) => invoke<string>("restore_backup", { backupId }),

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
  contextStats: (modelId: string, manId: string | null) =>
    invoke<ContextStats>("context_stats", { modelId, manId }),
  clearContext: (modelId: string, manId: string) =>
    invoke<ContextStats>("clear_context", { modelId, manId }),
  compactChat: (modelId: string, manId: string | null) =>
    invoke<AgentLog>("compact_chat", { modelId, manId }),
  compactContext: (modelId: string, manId: string, keepLast?: number) =>
    invoke<ContextStats>("compact_context", { modelId, manId, keepLast }),

  transcribe: (audioBase64: string, mime: string, language?: string) =>
    invoke<string>("transcribe", { audioBase64, mime, language }),
  listLocalModels: () => invoke<LocalModel[]>("list_local_models"),
  downloadLocalModel: (modelId: string) => invoke<LocalModel>("download_local_model", { modelId }),
  deleteLocalModel: (modelId: string) => invoke<LocalModel[]>("delete_local_model", { modelId }),
  localModelsBaseUrl: () => invoke<string>("local_models_base_url"),
  testProvider: () => invoke<Record<string, unknown>>("test_provider"),

  deleteAgentEntries: (model_id: string, man_id: string | null, ids: string[]) =>
    invoke<AgentLog>("delete_agent_entries", { modelId: model_id, manId: man_id, ids }),
  deleteMasterEntries: (ids: string[]) => invoke<AgentLog>("delete_master_entries", { ids }),
  saveChat: (model_id: string, man_id: string, messages: unknown[], summary?: string) =>
    invoke<ChatThread>("save_chat", { modelId: model_id, manId: man_id, messages, summary }),
  digestChat: (model_id: string, man_id: string, keep_last?: number) =>
    invoke<ChatThread>("digest_chat", { modelId: model_id, manId: man_id, keepLast: keep_last }),
  learnVoice: (model_id: string, samples?: number) =>
    invoke<Profile>("learn_voice", { modelId: model_id, samples }),
  deleteChatMessages: (model_id: string, man_id: string, ids: string[]) =>
    invoke<ChatThread>("delete_chat_messages", { modelId: model_id, manId: man_id, ids }),

  checkUpdate: () => invoke<UpdateInfo>("check_update"),

  fetchImage: (url: string) =>
    invoke<{ name: string; mime: string; data: string }>("fetch_image", { url }),

  seedDemo: () => invoke<Profile[]>("seed_demo"),
};

export function onAgentEvent(handler: (payload: Record<string, unknown>) => void) {
  return listen<Record<string, unknown>>(AGENT_EVENT, (event) => handler(event.payload));
}

export function onModelEvent(handler: (payload: Record<string, unknown>) => void) {
  return listen<Record<string, unknown>>(MODEL_EVENT, (event) => handler(event.payload));
}

/**
 * Turn whatever the core rejected with into a sentence.
 *
 * The core does not write prose: it names the failure and hands over the
 * details, and the wording lives in the dictionaries, in both languages.
 */
export function errorText(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;

  const failure = error as {
    kind?: string;
    key?: string;
    params?: Record<string, string | number>;
    message?: string;
  } | null;

  if (failure?.key) {
    const translated = t(failure.key, failure.params ?? {});
    if (translated !== failure.key) return translated;
  }
  if (failure?.kind && failure.message !== undefined) {
    return t(`error.${failure.kind}`, { message: failure.message });
  }
  if (failure?.message) return failure.message;
  return JSON.stringify(error);
}
