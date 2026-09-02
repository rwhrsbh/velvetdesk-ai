import type {
  AgentEntry,
  AgentMode,
  AppInfo,
  ContextStats,
  Man,
  PendingAction,
  Profile,
  SecurityLevel,
  Settings,
} from "./types";

export interface UiEntry extends AgentEntry {
  /** transient rows (live tool steps) are not persisted in the agent log */
  transient?: boolean;
}

export interface AppStore {
  info: AppInfo | null;
  settings: Settings | null;
  profiles: Profile[];
  men: Man[];
  entries: UiEntry[];
  pending: PendingAction[];
  activeModelId: string | null;
  activeManId: string | null;
  mode: AgentMode;
  security: SecurityLevel;
  channel: "chat" | "letter";
  logIncoming: boolean;
  menFilter: string;
  profileFilter: string;
  busy: boolean;
  /** Thinking level chosen next to the composer; empty means the provider decides. */
  thinking: string;
  /** Context usage for the open dossier, refreshed after every run. */
  context: ContextStats | null;
  /** A temporary chat: nothing said in it is written to the agent log. */
  temporary: boolean;
  /** Reasoning text streamed by the current run, shown under a spoiler. */
  thoughts: string;
  /** The master chat is open: one conversation across every profile. */
  master: boolean;
}

export const store: AppStore = {
  info: null,
  settings: null,
  profiles: [],
  men: [],
  entries: [],
  pending: [],
  activeModelId: null,
  activeManId: null,
  mode: "auto",
  security: "safe",
  channel: "chat",
  logIncoming: false,
  menFilter: "",
  profileFilter: "",
  busy: false,
  thinking: "",
  context: null,
  temporary: false,
  thoughts: "",
  master: false,
};

export function activeProfile(): Profile | null {
  return store.profiles.find((p) => p.id === store.activeModelId) ?? null;
}

export function activeMan(): Man | null {
  return store.men.find((m) => m.id === store.activeManId) ?? null;
}

export function visibleMen(): Man[] {
  const query = store.menFilter.trim().toLowerCase();
  if (!query) return store.men;
  return store.men.filter((man) => {
    const hay = [
      man.name,
      man.id,
      man.location,
      man.country,
      man.status,
      man.stage,
      ...man.tags,
      ...man.facts.map((f) => `${f.key} ${f.value}`),
    ]
      .join(" ")
      .toLowerCase();
    return hay.includes(query);
  });
}

export function visibleProfiles(): Profile[] {
  const query = store.profileFilter.trim().toLowerCase();
  if (!query) return store.profiles;
  return store.profiles.filter((p) =>
    `${p.name} ${p.id} ${p.site}`.toLowerCase().includes(query),
  );
}

export function pushEntry(entry: UiEntry) {
  store.entries.push(entry);
  if (store.entries.length > 400) store.entries.splice(0, store.entries.length - 400);
}

/**
 * One line of the conversation.
 *
 * A message the app writes itself keeps the key it came from, so switching the
 * interface language re-renders it rather than leaving yesterday's language on
 * screen.
 */
export function makeEntry(
  sender: UiEntry["sender"],
  text: string,
  meta: Record<string, unknown> | null = null,
  transient = false,
): UiEntry {
  return {
    id: `${Date.now()}-${Math.random().toString(16).slice(2)}`,
    sender,
    text,
    meta,
    ts: new Date().toISOString(),
    transient,
  };
}
