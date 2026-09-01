import { applyStatic, lang, setLang, t, type Lang } from "./i18n";
import { api, errorText, onAgentEvent } from "./api";
import type { ModalDeps } from "./deps";
import { $, bindModalDismiss, toast } from "./dom";
import { openManForm, openProfileForm } from "./forms";
import { openDoctorModal, openMasterModal, openPendingModal } from "./modals";
import { transcribeLocally } from "./local-whisper";
import { openKeysModal } from "./provider-modal";
import { activeMan, activeProfile, makeEntry, pushEntry, store } from "./store";
import type { AgentMode, RunStep, SecurityLevel, Settings } from "./types";
import {
  renderAll,
  renderChat,
  renderMen,
  renderProfiles,
  renderScope,
  renderTopbar,
  setIndexCounts,
} from "./views";

// ---------------------------------------------------------------------------
// data
// ---------------------------------------------------------------------------

async function loadIndexCounts() {
  try {
    const index = await api.rebuildIndex();
    setIndexCounts(index.models.map((m) => [m.id, m.men.length] as [string, number]));
  } catch (error) {
    console.error("index rebuild failed", error);
  }
}

async function refresh() {
  const [profiles, settings, pending] = await Promise.all([
    api.listProfiles(),
    api.getSettings(),
    api.pendingList(),
  ]);
  store.profiles = profiles;
  store.settings = settings;
  store.pending = pending;

  if (store.activeModelId && !profiles.some((p) => p.id === store.activeModelId)) {
    store.activeModelId = null;
    store.activeManId = null;
    store.men = [];
    store.entries = [];
  }
  if (!store.activeModelId && profiles.length > 0) {
    await selectProfile(settings.active_model_id ?? profiles[0].id, false);
  } else if (store.activeModelId) {
    store.men = await api.listMen(store.activeModelId);
  }
  await loadIndexCounts();
  await refreshLocalModels();
  renderAll();
}

async function selectProfile(modelId: string, redraw = true) {
  if (!store.profiles.some((p) => p.id === modelId)) return;
  store.activeModelId = modelId;
  store.activeManId = null;
  store.menFilter = "";
  ($("menSearch") as HTMLInputElement).value = "";

  try {
    store.men = await api.listMen(modelId);
    const log = await api.getAgentLog(modelId);
    store.entries = log.entries.slice(-120);
  } catch (error) {
    toast(errorText(error), "error");
    store.men = [];
    store.entries = [];
  }
  await persistSettings({ active_model_id: modelId });
  if (redraw) renderAll();
}

async function selectMan(manId: string | null) {
  store.activeManId = manId;
  if (manId && store.activeModelId) {
    try {
      const man = await api.getMan(store.activeModelId, manId);
      store.men = store.men.map((m) => (m.id === man.id ? man : m));
    } catch (error) {
      toast(errorText(error), "error");
    }
  }
  renderMen();
  renderScope();
}

async function persistSettings(patch: Partial<Settings>) {
  if (!store.settings) return;
  const next = { ...store.settings, ...patch };
  store.settings = next;
  try {
    store.settings = await api.saveSettings(next);
  } catch (error) {
    console.error("settings save failed", error);
  }
}

const deps: ModalDeps = { refresh, selectProfile: (id) => selectProfile(id), selectMan };

// ---------------------------------------------------------------------------
// agent run
// ---------------------------------------------------------------------------

function activeProviderReady(): boolean {
  const provider = store.settings?.providers.find(
    (p) => p.id === store.settings?.active_provider,
  );
  return Boolean(provider && provider.key_count > 0);
}

async function sendMessage() {
  const input = $("composerInput") as HTMLTextAreaElement;
  const text = input.value.trim();
  if (!text) return;
  if (!store.activeModelId) {
    toast(t("toast.pickProfile"), "error");
    return;
  }
  if (!activeProviderReady()) {
    toast(t("toast.needKey"), "error");
    void openKeysModal(deps);
    return;
  }
  if (store.busy) return;

  store.busy = true;
  ($("btnSend") as HTMLButtonElement).disabled = true;
  input.value = "";
  input.style.height = "auto";
  pushEntry(makeEntry("user", text));
  renderChat();
  renderScope();

  try {
    const output = await api.runAgent({
      model_id: store.activeModelId,
      man_id: store.activeManId,
      mode: store.mode,
      security: store.security,
      message: text,
      channel: store.channel,
      log_incoming: store.logIncoming,
    });

    store.entries = store.entries.filter((e) => !e.transient);
    pushEntry(
      makeEntry("assistant", output.reply, {
        steps: output.steps as unknown as RunStep[],
        usage: output.usage,
        mode: output.mode,
        key_index: output.key_index,
        turns: output.turns,
      }),
    );

    if (output.pending.length > 0) {
      toast(t("toast.pendingCount", { n: output.pending.length }), "info");
    }
    store.men = await api.listMen(store.activeModelId);
    store.pending = await api.pendingList();
  } catch (error) {
    store.entries = store.entries.filter((e) => !e.transient);
    pushEntry(makeEntry("system", `Ошибка: ${errorText(error)}`));
    toast(errorText(error), "error");
  } finally {
    store.busy = false;
    ($("btnSend") as HTMLButtonElement).disabled = false;
    renderAll();
  }
}

// ---------------------------------------------------------------------------
// voice dictation
// ---------------------------------------------------------------------------

/** Repository of the model chosen for offline dictation, if any. */
function localModelRepo(): string | null {
  const id = store.settings?.local_speech_model;
  if (!id) return null;
  return localRepos.get(id) ?? null;
}

const localRepos = new Map<string, string>();

async function refreshLocalModels() {
  try {
    const models = await api.listLocalModels();
    localRepos.clear();
    models.filter((m) => m.installed).forEach((m) => localRepos.set(m.id, m.repo));
  } catch (error) {
    console.error("local models", error);
  }
}

/** Run the clip through whichever engine the operator picked. */
async function transcribeClip(blob: Blob, mime: string): Promise<string> {
  if (store.settings?.speech_engine === "local") {
    const repo = localModelRepo();
    if (!repo) throw new Error(t("toast.localNoModel"));
    $("micLabel").textContent = t("composer.loadingModel");
    return transcribeLocally(repo, blob);
  }
  const base64 = await blobToBase64(blob);
  return api.transcribe(base64, mime.split(";")[0]);
}

let recorder: MediaRecorder | null = null;
let chunks: Blob[] = [];

function micButton() {
  return $("btnMic") as HTMLButtonElement;
}

function setMicState(state: "idle" | "recording" | "working") {
  const btn = micButton();
  const label = $("micLabel");
  btn.classList.toggle("recording", state === "recording");
  btn.disabled = state === "working";
  label.textContent =
    state === "recording"
      ? t("composer.stop")
      : state === "working"
        ? t("composer.transcribing")
        : t("composer.dictate");
}

async function toggleDictation() {
  if (recorder && recorder.state === "recording") {
    recorder.stop();
    return;
  }
  // On-device recognition needs a downloaded model, not a provider key.
  if (store.settings?.speech_engine === "local") {
    if (!localModelRepo()) {
      toast(t("toast.localNoModel"), "error");
      void openKeysModal(deps);
      return;
    }
  } else if (!activeProviderReady()) {
    toast(t("toast.needKeyVoice"), "error");
    void openKeysModal(deps);
    return;
  }
  if (!navigator.mediaDevices?.getUserMedia) {
    toast(t("toast.noMic"), "error");
    return;
  }

  try {
    const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
    const mime = pickMime();
    recorder = new MediaRecorder(stream, mime ? { mimeType: mime } : undefined);
    chunks = [];

    recorder.ondataavailable = (event) => {
      if (event.data.size > 0) chunks.push(event.data);
    };

    recorder.onstop = async () => {
      stream.getTracks().forEach((track) => track.stop());
      const type = recorder?.mimeType || mime || "audio/webm";
      const blob = new Blob(chunks, { type });
      recorder = null;
      if (blob.size < 1200) {
        setMicState("idle");
        toast(t("toast.tooShort"), "error");
        return;
      }
      setMicState("working");
      try {
        const text = await transcribeClip(blob, type);
        if (text.trim()) {
          const input = $("composerInput") as HTMLTextAreaElement;
          input.value = input.value ? `${input.value.trim()} ${text.trim()}` : text.trim();
          input.dispatchEvent(new Event("input"));
          input.focus();
        } else {
          toast(t("toast.nothingHeard"), "error");
        }
      } catch (error) {
        toast(errorText(error), "error");
      } finally {
        setMicState("idle");
      }
    };

    recorder.start();
    setMicState("recording");
  } catch (error) {
    setMicState("idle");
    toast(t("toast.micDenied", { error: errorText(error) }), "error");
  }
}

function pickMime(): string | undefined {
  const candidates = ["audio/webm;codecs=opus", "audio/webm", "audio/mp4", "audio/ogg"];
  return candidates.find((type) => MediaRecorder.isTypeSupported?.(type));
}

function blobToBase64(blob: Blob): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error("read failed"));
    reader.onload = () => {
      const result = String(reader.result);
      resolve(result.slice(result.indexOf(",") + 1));
    };
    reader.readAsDataURL(blob);
  });
}

// ---------------------------------------------------------------------------
// wiring
// ---------------------------------------------------------------------------

function bindTopbar() {
  document.querySelectorAll<HTMLButtonElement>("#modeControl .segmented-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      store.mode = btn.dataset.mode as AgentMode;
      void persistSettings({ agent_mode: store.mode });
      renderTopbar();
    });
  });

  document.querySelectorAll<HTMLButtonElement>("#securityControl .segmented-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      store.security = btn.dataset.security as SecurityLevel;
      void persistSettings({ security_level: store.security });
      renderTopbar();
    });
  });

  $("btnKeys").addEventListener("click", () => void openKeysModal(deps));
  $("providerChip").addEventListener("click", () => void openKeysModal(deps));
  $("btnDoctor").addEventListener("click", () => void openDoctorModal(deps));
  $("btnMaster").addEventListener("click", () => void openMasterModal(deps));
  $("btnPending").addEventListener("click", () => void openPendingModal(deps));
  $("btnLang").addEventListener("click", () => applyLanguage(lang() === "ru" ? "en" : "ru", true));
}

/** Switch UI language, redraw everything and remember the choice. */
function applyLanguage(next: Lang, persist = false) {
  setLang(next);
  applyStatic();
  $("langLabel").textContent = next.toUpperCase();
  if (store.settings) renderAll();
  if (persist) void persistSettings({ ui_language: next });
}

function bindPanels() {
  $("profileList").addEventListener("click", (event) => {
    const target = event.target as HTMLElement;
    if (target.dataset.act === "seed") {
      event.preventDefault();
      void api
        .seedDemo()
        .then(refresh)
        .then(() => toast(t("toast.demoCreated"), "success"))
        .catch((error) => toast(errorText(error), "error"));
      return;
    }
    const card = target.closest<HTMLElement>("[data-profile]");
    if (!card?.dataset.profile) return;
    if (card.dataset.profile === store.activeModelId) {
      void openProfileForm(deps, activeProfile());
    } else {
      void selectProfile(card.dataset.profile);
    }
  });

  $("menList").addEventListener("click", (event) => {
    const card = (event.target as HTMLElement).closest<HTMLElement>("[data-man]");
    if (!card?.dataset.man) return;
    if (card.dataset.man === store.activeManId) {
      void openManForm(deps, activeMan());
    } else {
      void selectMan(card.dataset.man);
    }
  });

  $("messages").addEventListener("click", async (event) => {
    const btn = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-act]");
    if (!btn) return;
    const entry = store.entries.find((e) => e.id === btn.dataset.entry);
    if (!entry) return;

    if (btn.dataset.act === "copy") {
      await navigator.clipboard.writeText(entry.text);
      toast(t("chat.copied"), "success");
    }

    if (btn.dataset.act === "send-as-outgoing") {
      if (!store.activeModelId || !store.activeManId) {
        toast(t("toast.pickMan"), "error");
        return;
      }
      try {
        await api.appendMessage({
          model_id: store.activeModelId,
          man_id: store.activeManId,
          role: "outgoing",
          channel: store.channel,
          text: entry.text,
        });
        toast(t("chat.logged"), "success");
      } catch (error) {
        toast(errorText(error), "error");
      }
    }
  });

  ($("profileSearch") as HTMLInputElement).addEventListener("input", (event) => {
    store.profileFilter = (event.target as HTMLInputElement).value;
    renderProfiles();
  });

  ($("menSearch") as HTMLInputElement).addEventListener("input", (event) => {
    store.menFilter = (event.target as HTMLInputElement).value;
    renderMen();
  });

  $("btnAddProfile").addEventListener("click", () => void openProfileForm(deps, null));
  $("btnEditProfile").addEventListener("click", () => {
    const profile = activeProfile();
    if (!profile) {
      toast(t("toast.pickProfile"), "error");
      return;
    }
    void openProfileForm(deps, profile);
  });

  $("btnAddMan").addEventListener("click", () => {
    if (!store.activeModelId) {
      toast(t("toast.pickProfile"), "error");
      return;
    }
    void openManForm(deps, null);
  });
  $("btnManCard").addEventListener("click", () => {
    const man = activeMan();
    if (!man) {
      toast(t("toast.pickMan"), "error");
      return;
    }
    void openManForm(deps, man);
  });
}

function bindComposer() {
  const input = $("composerInput") as HTMLTextAreaElement;
  $("btnSend").addEventListener("click", () => void sendMessage());
  micButton().addEventListener("click", () => void toggleDictation());

  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.ctrlKey || event.metaKey)) {
      event.preventDefault();
      void sendMessage();
    }
  });
  input.addEventListener("input", () => {
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, 220)}px`;
  });

  ($("logIncoming") as HTMLInputElement).addEventListener("change", (event) => {
    store.logIncoming = (event.target as HTMLInputElement).checked;
  });
  ($("channelSelect") as HTMLSelectElement).addEventListener("change", (event) => {
    store.channel = (event.target as HTMLSelectElement).value as "chat" | "letter";
  });
}

function bindTabs() {
  const tabs = document.querySelectorAll<HTMLButtonElement>(".tab-btn");
  const apply = (paneId: string) => {
    ["paneProfiles", "paneChat", "paneMen"].forEach((id) => {
      $(id).classList.toggle("pane-active", id === paneId);
    });
    tabs.forEach((tab) => tab.classList.toggle("active", tab.dataset.pane === paneId));
  };
  tabs.forEach((tab) => tab.addEventListener("click", () => apply(tab.dataset.pane!)));
  apply("paneChat");
}

function bindAgentEvents() {
  void onAgentEvent((payload) => {
    const kind = String(payload.kind ?? "");
    if (kind === "step") {
      const step = payload.step as RunStep | undefined;
      if (!step) return;
      pushEntry(makeEntry("system", step.summary, { steps: [step] }, true));
      renderChat();
    } else if (kind === "llm_retry") {
      pushEntry(
        makeEntry(
          "system",
          `${t("chat.key", { n: Number(payload.key_index ?? 0) + 1 })}: ${payload.verdict}`,
          null,
          true,
        ),
      );
      renderChat();
    } else if (kind === "llm_wait") {
      pushEntry(makeEntry("system", String(payload.message ?? ""), null, true));
      renderChat();
    }
  });
}

async function boot() {
  bindModalDismiss();
  bindTopbar();
  bindPanels();
  bindComposer();
  bindTabs();
  bindAgentEvents();

  try {
    const data = await api.bootstrap();
    store.info = data.info;
    store.settings = data.settings;
    store.profiles = data.profiles;
    store.pending = data.pending;
    store.mode = data.settings.agent_mode;
    store.security = data.settings.security_level;
    applyLanguage(data.settings.ui_language === "en" ? "en" : "ru");
    setIndexCounts(data.index.models.map((m) => [m.id, m.men.length] as [string, number]));

    const preferred = data.settings.active_model_id ?? data.profiles[0]?.id ?? null;
    if (preferred) await selectProfile(preferred, false);

    renderAll();

    if (data.profiles.length === 0) {
      pushEntry(
        makeEntry(
          "system",
          t("hint.firstRun"),
        ),
      );
      renderChat();
    } else if (!activeProviderReady()) {
      pushEntry(makeEntry("system", t("hint.noKey")));
      renderChat();
    }
  } catch (error) {
    toast(t("toast.bootFailed", { error: errorText(error) }), "error");
  }
}

void boot();
