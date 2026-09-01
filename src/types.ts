export type AgentMode = "auto" | "act" | "memorize";
export type SecurityLevel = "ask" | "safe" | "yolo";
export type Risk = "read" | "write" | "destructive";
export type MsgRole = "incoming" | "outgoing" | "note";
export type Channel = "chat" | "letter" | "note";

export interface Fact {
  id: string;
  key: string;
  value: string;
  source: string;
  created_at: string;
}

export interface Note {
  id: string;
  text: string;
  author: string;
  created_at: string;
}

export interface Gift {
  id: string;
  title: string;
  kind: string;
  value: number | null;
  note: string;
  date: string;
}

export interface Profile {
  id: string;
  name: string;
  age: number | null;
  site: string;
  avatar: string;
  bio: string;
  system_prompt_override: string;
  tone_rules: string[];
  banned_phrases: string[];
  languages: string[];
  facts: Fact[];
  created_at: string;
  updated_at: string;
  schema_version: number;
}

export interface Man {
  id: string;
  model_id: string;
  name: string;
  age: number | null;
  location: string;
  country: string;
  avatar: string;
  status: string;
  stage: string;
  sentiment: string;
  next_action: string;
  tags: string[];
  triggers: string[];
  boundaries: string[];
  gifts: Gift[];
  facts: Fact[];
  notes: Note[];
  last_contact: string | null;
  created_at: string;
  updated_at: string;
  schema_version: number;
}

export interface ChatMessage {
  id: string;
  role: MsgRole;
  channel: Channel;
  text: string;
  ts: string;
}

export interface ChatThread {
  model_id: string;
  man_id: string;
  messages: ChatMessage[];
  updated_at: string;
}

export interface AgentEntry {
  id: string;
  sender: "user" | "assistant" | "system" | "tool";
  text: string;
  meta: Record<string, unknown> | null;
  ts: string;
}

export interface AgentLog {
  model_id: string;
  entries: AgentEntry[];
}

export interface RunStep {
  kind: string;
  tool: string | null;
  summary: string;
  detail: unknown;
}

export interface Usage {
  prompt_tokens: number;
  completion_tokens: number;
  total_tokens: number;
}

export interface PendingAction {
  id: string;
  model_id: string;
  tool: string;
  args: Record<string, unknown>;
  risk: Risk;
  summary: string;
  before: unknown;
  after: unknown;
  created_at: string;
}

export interface RunOutput {
  reply: string;
  mode: AgentMode;
  security: SecurityLevel;
  model_id: string;
  man_id: string | null;
  steps: RunStep[];
  pending: PendingAction[];
  usage: Usage;
  key_index: number;
  turns: number;
}

export interface SearchHit {
  model_id: string;
  model_name: string;
  man_id: string;
  man_name: string;
  snippet: string;
  score: number;
}

export interface MasterDecision {
  model_id: string | null;
  man_id: string | null;
  confidence: number;
  reason: string;
  created: string | null;
  hits: SearchHit[];
  steps: RunStep[];
  usage: Usage;
}

export interface ProviderConfig {
  id: string;
  label: string;
  kind: "gemini" | "openai_compatible";
  base_url: string;
  api_version: string;
  model: string;
  extra_headers: [string, string][];
  temperature: number;
  max_output_tokens: number | null;
  key_count: number;
}

export interface Settings {
  providers: ProviderConfig[];
  active_provider: string | null;
  agent_mode: AgentMode;
  security_level: SecurityLevel;
  active_model_id: string | null;
  history_limit: number;
  max_tool_turns: number;
  global_style_rules: string;
  telemetry_disabled: boolean;
}

export interface KeyStatus {
  index: number;
  masked: string;
  cooling_seconds: number;
  failures: number;
  successes: number;
  last_error: string | null;
}

export interface IndexMan {
  id: string;
  name: string;
  tags: string[];
  stage: string;
  keywords: string;
}

export interface IndexModel {
  id: string;
  name: string;
  site: string;
  avatar: string;
  men: IndexMan[];
}

export interface GlobalIndex {
  models: IndexModel[];
  updated_at: string;
}

export interface AppInfo {
  version: string;
  data_dir: string;
  platform: string;
}

export interface DoctorIssue {
  level: "ok" | "warn" | "error";
  scope: string;
  path: string;
  message: string;
  fixable: boolean;
  fixed: boolean;
}

export interface DoctorReport {
  issues: DoctorIssue[];
  models_checked: number;
  men_checked: number;
  chats_checked: number;
  fixes_applied: number;
}

export interface Bootstrap {
  info: AppInfo;
  settings: Settings;
  profiles: Profile[];
  index: GlobalIndex;
  pending: PendingAction[];
}
