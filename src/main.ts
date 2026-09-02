import { applyStatic, lang, setLang, t, type Lang } from "./i18n";
import { api, errorText, onAgentEvent } from "./api";
import type { ModalDeps } from "./deps";
import { $, bindModalDismiss, confirmDialog, toast } from "./dom";
import {
  copyText,
  editingEntries,
  openContextMenu,
  selectionWithin,
  type MenuEntry,
} from "./context-menu";
import { openManForm, openProfileForm } from "./forms";
import { openDoctorModal, openPendingModal } from "./modals";
import { loadModel, SilentClipError, transcribeLocally } from "./local-whisper";
import { openKeysModal } from "./provider-modal";
import {
  activeMan,
  activeProfile,
  makeEntry,
  pushEntry,
  store,
  visibleMen,
  type UiEntry,
} from "./store";
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
  void refreshLocalModels();
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
    await loadChat();
  } catch (error) {
    toast(errorText(error), "error");
    store.men = [];
    store.entries = [];
  }
  await persistSettings({ active_model_id: modelId });
  if (redraw) renderAll();
}

/**
 * Load the conversation for whatever is selected.
 *
 * Every dossier is its own chat, and the profile has one of its own for when
 * none is open — switching men switches conversations, the way a messenger
 * does. A temporary chat is in memory only and must not be overwritten.
 */
async function loadChat() {
  if (!store.activeModelId || store.temporary) return;
  try {
    const log = await api.getAgentLog(store.activeModelId, store.activeManId);
    store.entries = log.entries.slice(-120);
  } catch (error) {
    console.error("chat load failed", error);
    store.entries = [];
  }
}

async function selectMan(manId: string | null) {
  if (store.activeManId === manId) return;
  store.activeManId = manId;
  if (manId && store.activeModelId) {
    try {
      const man = await api.getMan(store.activeModelId, manId);
      store.men = store.men.map((m) => (m.id === man.id ? man : m));
    } catch (error) {
      toast(errorText(error), "error");
    }
  }
  await loadChat();
  renderChat();
  renderMen();
  renderScope();
  void refreshContextGauge();
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
  if ("speech_engine" in patch || "local_speech_model" in patch) warmLocalModel();
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
  const typed = ($("composerInput") as HTMLTextAreaElement).value.trim();
  if (typed.startsWith("/") && (await runSlashCommand(typed))) return;

  // Letters are not a conversation: the brief goes to one man or to a whole
  // list, and each letter comes back as its own card.
  if (store.mode === "letters" && !store.master) {
    await writeLetters(typed);
    return;
  }

  if (store.master) {
    if (!typed || store.busy) return;
    const input = $("composerInput") as HTMLTextAreaElement;
    input.value = "";
    input.style.height = "auto";
    await sendToMaster(typed);
    return;
  }
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
      thinking_effort: store.thinking || undefined,
      temporary: store.temporary,
    });

    const streamed = store.entries.find((e) => e.transient && e.sender === "assistant");
    const thoughts =
      output.thoughts || ((streamed?.meta as { thoughts?: string })?.thoughts ?? "");
    store.entries = store.entries.filter((e) => !e.transient);
    // A reply the app wrote itself arrives as a key, so it reads in the
    // interface language rather than the one the core was written in.
    const reply = output.reply_key ? t(output.reply_key) : output.reply;
    pushEntry(
      makeEntry("assistant", reply, {
        steps: output.steps as unknown as RunStep[],
        usage: output.usage,
        mode: output.mode,
        model: output.model,
        key_index: output.key_index,
        turns: output.turns,
        thoughts,
      }),
    );
    store.thoughts = "";

    if (output.pending.length > 0) {
      toast(t("toast.pendingCount", { n: output.pending.length }), "info");
    }
    store.men = await api.listMen(store.activeModelId);
    store.pending = await api.pendingList();
  } catch (error) {
    store.entries = store.entries.filter((e) => !e.transient);
    pushEntry(makeEntry("system", t("chat.error", { message: errorText(error) })));
    toast(errorText(error), "error");
  } finally {
    store.busy = false;
    ($("btnSend") as HTMLButtonElement).disabled = false;
    renderAll();
    void refreshContextGauge();
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
  warmLocalModel();
}

let warming: Promise<unknown> | null = null;

/**
 * Load the offline recogniser ahead of time.
 *
 * Building a session takes a few seconds; doing it on the first press made the
 * button look dead. Whenever on-device dictation is the chosen engine the model
 * is warmed in the background, so pressing Dictate starts recording at once.
 */
function warmLocalModel() {
  if (store.settings?.speech_engine !== "local" || warming) return;
  const repo = localModelRepo();
  if (!repo) return;
  warming = loadModel(repo)
    .catch((error) => console.error("whisper warm-up failed", error))
    .finally(() => {
      warming = null;
    });
}

/**
 * Language for dictation: the operator's choice, or the interface language.
 * Never empty — Whisper treats "no language" as English and silently
 * translates, which turns dictated Russian into English prose.
 */
function speechLanguage(): string {
  const chosen = store.settings?.speech_language?.trim();
  if (chosen) return chosen;
  return lang();
}

/** Run the clip through whichever engine the operator picked. */
async function transcribeClip(blob: Blob, mime: string): Promise<string> {
  const language = speechLanguage();
  if (store.settings?.speech_engine === "local") {
    const repo = localModelRepo();
    if (!repo) throw new Error(t("toast.localNoModel"));
    $("micLabel").textContent = t("composer.loadingModel");
    return transcribeLocally(repo, blob, { language });
  }
  const base64 = await blobToBase64(blob);
  return api.transcribe(base64, mime.split(";")[0], language);
}

/**
 * Live input level, shown while recording.
 *
 * Without it a dead microphone looks exactly like a working one until the
 * transcript comes back as nonsense.
 */
let meter: { context: AudioContext; timer: number } | null = null;

function startMeter(stream: MediaStream) {
  stopMeter();
  const context = new AudioContext();
  const analyser = context.createAnalyser();
  analyser.fftSize = 1024;
  context.createMediaStreamSource(stream).connect(analyser);
  const samples = new Float32Array(analyser.fftSize);
  const bar = $("micLevel");
  let peak = 0;

  const timer = window.setInterval(() => {
    analyser.getFloatTimeDomainData(samples);
    let sum = 0;
    for (let i = 0; i < samples.length; i += 1) sum += samples[i] * samples[i];
    const rms = Math.sqrt(sum / samples.length);
    peak = Math.max(peak, rms);
    // Speech sits around 0.05–0.2 RMS; scale so normal talking fills the bar.
    bar.style.width = `${Math.min(100, Math.round(rms * 600))}%`;
  }, 100);

  meter = { context, timer };
  return () => peak;
}

function stopMeter() {
  if (!meter) return;
  window.clearInterval(meter.timer);
  void meter.context.close();
  meter = null;
  const bar = document.getElementById("micLevel");
  if (bar) bar.style.width = "0%";
}

let recorder: MediaRecorder | null = null;
let chunks: Blob[] = [];
let peakLevel: (() => number) | null = null;

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
    const stream = await openMicrophone();
    const mime = pickMime();
    recorder = new MediaRecorder(stream, mime ? { mimeType: mime } : undefined);
    chunks = [];

    recorder.ondataavailable = (event) => {
      if (event.data.size > 0) chunks.push(event.data);
    };

    recorder.onstop = async () => {
      stopMeter();
      stream.getTracks().forEach((track) => track.stop());
      const type = recorder?.mimeType || mime || "audio/webm";
      const blob = new Blob(chunks, { type });
      recorder = null;
      // A silent clip compresses to almost nothing, so check the level first:
      // otherwise a dead microphone is reported as a too-short recording.
      const silent = !peakLevel || peakLevel() < 0.01;
      if (silent) {
        setMicState("idle");
        toast(t("toast.silentClip"), "error");
        return;
      }
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
        toast(
          error instanceof SilentClipError ? t("toast.silentClip") : errorText(error),
          "error",
        );
      } finally {
        setMicState("idle");
      }
    };

    recorder.start();
    peakLevel = startMeter(stream);
    setMicState("recording");
  } catch (error) {
    setMicState("idle");
    toast(micErrorText(error), "error");
  }
}

/**
 * Open the chosen microphone, falling back to the default one when the saved
 * device has gone away (unplugged, or claimed by another app).
 */
async function openMicrophone(): Promise<MediaStream> {
  const deviceId = store.settings?.speech_device ?? "";
  if (deviceId) {
    try {
      return await navigator.mediaDevices.getUserMedia({
        audio: { deviceId: { exact: deviceId } },
      });
    } catch (error) {
      console.warn("saved microphone unavailable, falling back", error);
    }
  }
  return navigator.mediaDevices.getUserMedia({ audio: true });
}

/**
 * Microphone picker, opened from the caret on the dictate button. Device
 * labels only exist once access has been granted at least once, so unnamed
 * devices get a number instead.
 */
async function openMicrophoneMenu(anchor: HTMLElement) {
  let devices: MediaDeviceInfo[] = [];
  try {
    devices = (await navigator.mediaDevices.enumerateDevices()).filter(
      (d) => d.kind === "audioinput",
    );
  } catch {
    devices = [];
  }
  const chosen = store.settings?.speech_device ?? "";
  const entries: MenuEntry[] = [
    {
      label: `${chosen ? "" : "✓ "}${t("composer.micDefault")}`,
      onSelect: () => void persistSettings({ speech_device: "" }),
    },
  ];
  devices.forEach((device, index) => {
    const label = device.label || t("composer.micNumbered", { n: index + 1 });
    entries.push({
      label: `${device.deviceId === chosen ? "✓ " : ""}${label}`,
      onSelect: () => void persistSettings({ speech_device: device.deviceId }),
    });
  });
  const box = anchor.getBoundingClientRect();
  openContextMenu(box.left, Math.max(8, box.top - 12 - entries.length * 28), entries);
}

/**
 * getUserMedia fails for three quite different reasons and the fix differs
 * every time, so the message has to say which one it was.
 */
function micErrorText(error: unknown): string {
  const name = (error as { name?: string })?.name ?? "";
  switch (name) {
    case "NotAllowedError":
    case "SecurityError":
      return t("toast.micBlocked");
    case "NotFoundError":
    case "OverconstrainedError":
      return t("toast.micMissing");
    case "NotReadableError":
    case "AbortError":
      return t("toast.micBusy");
    default:
      return t("toast.micDenied", { error: errorText(error) });
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
  $("btnMaster").addEventListener("click", () => void toggleMasterChat());
  $("btnPending").addEventListener("click", () => void openPendingModal(deps));
  $("btnLang").addEventListener("click", () => applyLanguage(lang() === "ru" ? "en" : "ru", true));
}

/** Switch UI language, redraw everything and remember the choice. */
/** Re-render the notes the app wrote, in the language now selected. */
function retranslateEntries() {
  for (const entry of store.entries) {
    const meta = entry.meta as { key?: string; params?: Record<string, string | number> } | null;
    if (meta?.key) entry.text = t(meta.key, meta.params ?? {});
  }
}

function applyLanguage(next: Lang, persist = false) {
  setLang(next);
  applyStatic();
  retranslateEntries();
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
      // A letter carries its own recipient; a chat reply belongs to whoever is
      // open.
      const meta = (entry.meta ?? {}) as { man_id?: string };
      const manId = meta.man_id ?? store.activeManId;
      if (!store.activeModelId || !manId) {
        toast(t("toast.pickMan"), "error");
        return;
      }
      try {
        await api.appendMessage({
          model_id: store.activeModelId,
          man_id: manId,
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

  // Leaving a dossier returns to the profile's own chat, which is a different
  // conversation rather than the same one with less context.
  $("btnDeselectMan").addEventListener("click", () => void selectMan(null));

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

// ---------------------------------------------------------------------------
// context menus
// ---------------------------------------------------------------------------

/** Copy an action that only makes sense when something is selected. */
function selectionEntry(root: Element): MenuEntry[] {
  const selected = selectionWithin(root);
  return selected
    ? [{ label: t("ctx.copySelection"), onSelect: () => copyText(selected) }, "separator"]
    : [];
}

function profileEntries(modelId: string): MenuEntry[] {
  const profile = store.profiles.find((p) => p.id === modelId);
  if (!profile) return [];
  return [
    { label: t("ctx.openChat"), onSelect: () => void selectProfile(modelId) },
    {
      label: t("ctx.openProfile"),
      onSelect: async () => {
        if (modelId !== store.activeModelId) await selectProfile(modelId);
        void openProfileForm(deps, activeProfile());
      },
    },
    "separator",
    { label: t("ctx.copyName"), onSelect: () => copyText(profile.name) },
    { label: t("ctx.copyId"), onSelect: () => copyText(profile.id) },
    "separator",
    { label: t("ctx.addProfile"), onSelect: () => void openProfileForm(deps, null) },
    {
      label: t("ctx.deleteProfile"),
      danger: true,
      onSelect: async () => {
        const ok = await confirmDialog({
          title: t("profile.deleteTitle"),
          body: t("profile.deleteBody", { name: profile.name }),
          confirmLabel: t("common.delete"),
          danger: true,
        });
        if (!ok) return;
        try {
          await api.deleteProfile(modelId);
          if (store.activeModelId === modelId) {
            store.activeModelId = null;
            store.activeManId = null;
            store.men = [];
            store.entries = [];
          }
          await refresh();
          toast(t("toast.profileDeleted"), "success");
        } catch (error) {
          toast(errorText(error), "error");
        }
      },
    },
  ];
}

function manEntries(manId: string): MenuEntry[] {
  const man = store.men.find((m) => m.id === manId);
  if (!man) return [];
  return [
    // Two different things, and naming both "open" made one of them look
    // broken: his chat is what the rail switches to, his dossier is a card.
    { label: t("ctx.openChat"), onSelect: () => void selectMan(manId) },
    {
      label: t("ctx.openDossier"),
      onSelect: async () => {
        if (manId !== store.activeManId) await selectMan(manId);
        void openManForm(deps, activeMan());
      },
    },
    "separator",
    { label: t("ctx.copyName"), onSelect: () => copyText(man.name) },
    { label: t("ctx.copyId"), onSelect: () => copyText(man.id) },
    "separator",
    {
      label: t("ctx.addMan"),
      onSelect: () => {
        if (!store.activeModelId) {
          toast(t("toast.pickProfile"), "error");
          return;
        }
        void openManForm(deps, null);
      },
    },
    {
      label: t("ctx.deleteMan"),
      danger: true,
      onSelect: async () => {
        const ok = await confirmDialog({
          title: t("man.deleteTitle"),
          body: t("man.deleteBody", { name: man.name }),
          confirmLabel: t("common.delete"),
          danger: true,
        });
        if (!ok || !store.activeModelId) return;
        try {
          await api.deleteMan(store.activeModelId, manId);
          if (store.activeManId === manId) await selectMan(null);
          await selectProfile(store.activeModelId);
          toast(t("toast.manDeleted"), "success");
        } catch (error) {
          toast(errorText(error), "error");
        }
      },
    },
  ];
}

function messageEntries(bubble: Element, entryId: string | undefined): MenuEntry[] {
  const entry = store.entries.find((e) => e.id === entryId);
  const text = entry?.text ?? bubble.textContent?.trim() ?? "";
  const entries: MenuEntry[] = [
    ...selectionEntry(bubble),
    { label: t("ctx.copyText"), disabled: !text, onSelect: () => copyText(text) },
  ];
  if (entry?.sender === "assistant" && !entry.transient) {
    entries.push({
      label: t("chat.asOutgoing"),
      onSelect: () => void logAsOutgoing(entry.text),
    });
  }
  if (entry?.meta && Object.keys(entry.meta).length > 0) {
    entries.push({
      label: t("ctx.copyJson"),
      onSelect: () => copyText(JSON.stringify(entry.meta, null, 2)),
    });
  }
  entries.push("separator", {
    label: t("ctx.clearLog"),
    danger: true,
    disabled: !store.activeModelId,
    onSelect: async () => {
      const ok = await confirmDialog({
        title: t("ctx.clearLog"),
        body: t("ctx.confirmClearLog"),
        confirmLabel: t("common.delete"),
        danger: true,
      });
      if (!ok || !store.activeModelId) return;
      await api.clearAgentLog(store.activeModelId);
      store.entries = [];
      renderChat();
    },
  });
  return entries;
}

async function logAsOutgoing(text: string) {
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
      text,
    });
    toast(t("chat.logged"), "success");
  } catch (error) {
    toast(errorText(error), "error");
  }
}

function bindContextMenus() {
  document.addEventListener("contextmenu", (event) => {
    const target = event.target as HTMLElement | null;
    if (!target) return;

    // Text fields get the editing menu, including inside modals.
    const field = target.closest<HTMLInputElement | HTMLTextAreaElement>("input, textarea");
    if (field && !field.disabled) {
      event.preventDefault();
      const entries = editingEntries(field);
      if (field.id === "composerInput") {
        entries.push("separator", {
          label: t("ctx.dictate"),
          onSelect: () => void toggleDictation(),
        });
      }
      openContextMenu(event.clientX, event.clientY, entries);
      return;
    }

    let entries: MenuEntry[] = [];
    const bubble = target.closest(".bubble");
    const profileRow = target.closest<HTMLElement>("[data-profile]");
    const manRow = target.closest<HTMLElement>("[data-man]");

    if (bubble) {
      entries = messageEntries(bubble, target.closest<HTMLElement>("[data-entry]")?.dataset.entry);
    } else if (profileRow?.dataset.profile) {
      entries = [...selectionEntry(profileRow), ...profileEntries(profileRow.dataset.profile)];
    } else if (manRow?.dataset.man) {
      entries = [...selectionEntry(manRow), ...manEntries(manRow.dataset.man)];
    } else {
      // Anywhere else: offer a copy when there is a selection to copy.
      const selected = window.getSelection()?.toString().trim() ?? "";
      if (selected) entries = [{ label: t("ctx.copy"), onSelect: () => copyText(selected) }];
    }

    if (entries.length === 0) return;
    event.preventDefault();
    openContextMenu(event.clientX, event.clientY, entries);
  });
}

/**
 * A note from the app itself, remembered by key.
 *
 * The text is rendered now so the message reads immediately, and the key is
 * kept so it is rendered again — in the new language — when the interface
 * switches.
 */
function systemNote(key: string, params: Record<string, string | number> = {}) {
  return makeEntry("system", t(key, params), { key, params });
}

// ---------------------------------------------------------------------------
// context: the gauge, /clear and /compact
// ---------------------------------------------------------------------------

/** Redraw the "how full is the model's context" gauge for the open dossier. */
async function refreshContextGauge() {
  const gauge = $("ctxGauge");
  // Whichever chat is open is the one measured: the master carries its own
  // conversation into every turn, a profile chat carries the correspondence.
  if (!store.master && !store.activeModelId) {
    gauge.hidden = true;
    return;
  }
  try {
    const stats = store.master
      ? await api.masterContextStats()
      : await api.contextStats(store.activeModelId!, store.activeManId);
    store.context = stats;
    const percent = Math.min(100, Math.round(stats.ratio * 100));
    gauge.hidden = false;
    gauge.classList.toggle("warn", stats.ratio >= (store.settings?.auto_compact_at ?? 0.85));
    $("ctxFill").style.width = `${percent}%`;
    $("ctxLabel").textContent = `${percent}%`;
    gauge.title = t(
      store.master ? "composer.contextDetailChat" : "composer.contextDetail",
      {
        used: stats.used_tokens,
        window: stats.window_tokens,
        live: stats.live_messages,
        total: stats.total_messages,
      },
    );
  } catch {
    gauge.hidden = true;
  }
}

/**
 * Commands typed into the composer.
 *
 * They act on what the model reads, never on what it has remembered: a dossier,
 * its facts and every stored message survive both of these.
 */
async function runSlashCommand(raw: string): Promise<boolean> {
  const [name] = raw.trim().slice(1).split(/\s+/);
  const command = name.toLowerCase();
  if (!["clear", "compact", "help"].includes(command)) return false;

  const input = $("composerInput") as HTMLTextAreaElement;
  input.value = "";
  input.dispatchEvent(new Event("input"));

  if (command === "help") {
    pushEntry(systemNote("cmd.help"));
    renderChat();
    return true;
  }

  if (!store.activeModelId) {
    toast(t("toast.pickProfile"), "error");
    return true;
  }

  try {
    if (command === "clear") {
      // The chat the operator is looking at, plus the correspondence context
      // when a dossier is open. Dossiers, facts and stored messages are not
      // touched by either.
      if (store.master) {
        await api.clearMasterLog();
        store.entries = [];
        pushEntry(systemNote("cmd.clearedChat"));
        renderChat();
        return true;
      }
      if (!store.temporary) await api.clearAgentLog(store.activeModelId, store.activeManId);
      store.entries = [];
      if (store.activeManId) {
        const stats = await api.clearContext(store.activeModelId, store.activeManId);
        pushEntry(systemNote("cmd.cleared", { n: stats.total_messages }));
      } else {
        pushEntry(systemNote("cmd.clearedChat"));
      }
    } else {
      store.busy = true;
      renderScope();
      pushEntry(makeEntry("system", t("cmd.compacting"), { key: "cmd.compacting" }, true));
      renderChat();
      if (store.activeManId) {
        const stats = await api.compactContext(store.activeModelId, store.activeManId);
        store.entries = store.entries.filter((e) => !e.transient);
        pushEntry(
          makeEntry(
            "system",
            t("cmd.compacted", { live: stats.live_messages, total: stats.total_messages }),
          ),
        );
      } else {
        // No dossier open: fold the copilot chat itself into one summary.
        const summary = await summariseChat();
        store.entries = store.entries.filter((e) => !e.transient);
        if (!store.temporary) await api.clearAgentLog(store.activeModelId, store.activeManId);
        store.entries = [];
        pushEntry(makeEntry("system", summary));
      }
    }
  } catch (error) {
    store.entries = store.entries.filter((e) => !e.transient);
    toast(errorText(error), "error");
  } finally {
    store.busy = false;
    renderScope();
  }
  renderChat();
  void refreshContextGauge();
  return true;
}

/**
 * Fold the visible copilot chat into a few lines. Used by /compact when no
 * dossier is open — there the chat itself is what has grown long.
 */
async function summariseChat(): Promise<string> {
  const transcript = store.entries
    .filter((e) => !e.transient && e.text.trim())
    .map((e) => `${e.sender.toUpperCase()}: ${e.text}`)
    .join("\n");
  if (!transcript) return t("cmd.nothingToCompact");

  const output = await api.runAgent({
    model_id: store.activeModelId!,
    man_id: null,
    mode: "act",
    security: store.security,
    message: `${t("cmd.summariseInstruction")}\n\n${transcript}`,
    temporary: true,
    thinking_effort: store.thinking || undefined,
  });
  return output.reply.trim() || t("cmd.nothingToCompact");
}

/**
 * Open a throwaway chat, or close it and return to the saved one.
 *
 * A temporary chat is not written to the log at all: the copilot still reads
 * the dossier and still writes facts, notes and messages, but the conversation
 * itself leaves no trace.
 */
async function toggleTemporaryChat() {
  store.temporary = !store.temporary;
  $("btnTemporary").classList.toggle("active", store.temporary);
  store.entries = [];

  if (store.temporary) {
    pushEntry(systemNote("cmd.temporaryStarted"));
  } else {
    if (store.activeModelId) {
      const log = await api.getAgentLog(store.activeModelId, store.activeManId);
      store.entries = log.entries.map((entry) => ({ ...entry }));
    }
    toast(t("cmd.temporaryEnded"), "info");
  }
  renderChat();
}

/**
 * The master chat: one conversation that spans every profile.
 *
 * It is the same agent loop with a wider reach — it can search across
 * profiles, create one that does not exist yet, and file men under it. Writes
 * still obey the security level, and a folder grant still needs an answer.
 */
async function toggleMasterChat() {
  store.master = !store.master;
  $("btnMaster").classList.toggle("active", store.master);
  store.entries = [];

  void refreshContextGauge();
  if (store.master) {
    try {
      const log = await api.getMasterLog();
      store.entries = log.entries.slice(-120).map((entry) => ({ ...entry }));
    } catch (error) {
      console.error("master log", error);
    }
    if (store.entries.length === 0) pushEntry(systemNote("master.hello"));
  } else {
    await loadChat();
  }
  renderAll();
}

async function sendToMaster(text: string) {
  store.busy = true;
  ($("btnSend") as HTMLButtonElement).disabled = true;
  pushEntry(makeEntry("user", text));
  renderChat();
  renderScope();

  try {
    const output = await api.masterChat({
      message: text,
      security: store.security,
      thinking_effort: store.thinking || undefined,
      temporary: store.temporary,
    });
    store.entries = store.entries.filter((e) => !e.transient);
    pushEntry(
      makeEntry("assistant", output.reply, {
        steps: output.steps as unknown as RunStep[],
        usage: output.usage,
        key_index: output.key_index,
        turns: output.turns,
      }),
    );
    if (output.pending.length > 0) {
      toast(t("toast.pendingCount", { n: output.pending.length }), "info");
    }
    store.profiles = await api.listProfiles();
    store.pending = await api.pendingList();
    if (store.activeModelId) store.men = await api.listMen(store.activeModelId);
  } catch (error) {
    store.entries = store.entries.filter((e) => !e.transient);
    pushEntry(makeEntry("system", t("chat.error", { message: errorText(error) })));
    toast(errorText(error), "error");
  } finally {
    store.busy = false;
    ($("btnSend") as HTMLButtonElement).disabled = false;
    renderAll();
    void refreshContextGauge();
  }
}

/**
 * Write to the open dossier, or to everyone the rail is showing.
 *
 * Each letter is a separate request so it is written for its man rather than
 * averaged across the list; a long round is confirmed first, because it costs
 * one call per recipient.
 */
async function writeLetters(brief: string) {
  if (!store.activeModelId) {
    toast(t("toast.pickProfile"), "error");
    return;
  }
  if (store.busy) return;

  const recipients = store.activeManId ? [store.activeManId] : visibleMen().map((m) => m.id);
  if (recipients.length === 0) {
    toast(t("letters.noRecipients"), "error");
    return;
  }
  if (recipients.length > 5) {
    const ok = await confirmDialog({
      title: t("letters.confirmTitle"),
      body: t("letters.confirmBody", { n: recipients.length }),
      confirmLabel: t("letters.write"),
    });
    if (!ok) return;
  }

  const input = $("composerInput") as HTMLTextAreaElement;
  input.value = "";
  input.style.height = "auto";
  store.busy = true;
  ($("btnSend") as HTMLButtonElement).disabled = true;
  if (brief) pushEntry(makeEntry("user", brief));
  pushEntry(makeEntry("system", t("letters.writing", { n: recipients.length }), null, true));
  renderChat();
  renderScope();

  try {
    const output = await api.writeLetters({
      model_id: store.activeModelId,
      man_ids: store.activeManId ? recipients : [],
      brief,
      channel: store.channel,
      thinking_effort: store.thinking || undefined,
      temporary: store.temporary,
    });
    store.entries = store.entries.filter((e) => !e.transient);
    for (const letter of output.letters) {
      pushEntry(
        makeEntry("assistant", letter.error || letter.text, {
          letter: true,
          man_id: letter.man_id,
          recipient: letter.name,
          failed: Boolean(letter.error),
          usage: letter.usage,
        }),
      );
    }
  } catch (error) {
    store.entries = store.entries.filter((e) => !e.transient);
    pushEntry(makeEntry("system", t("chat.error", { message: errorText(error) })));
    toast(errorText(error), "error");
  } finally {
    store.busy = false;
    ($("btnSend") as HTMLButtonElement).disabled = false;
    renderAll();
  }
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

  $("btnTemporary").addEventListener("click", () => void toggleTemporaryChat());

  ($("channelSelect") as HTMLSelectElement).addEventListener("change", (event) => {
    store.channel = (event.target as HTMLSelectElement).value as "chat" | "letter";
  });

  const micMenu = $("btnMicMenu");
  micMenu.addEventListener("click", () => void openMicrophoneMenu(micMenu));

  const speech = $("speechLang") as HTMLSelectElement;
  speech.addEventListener("change", () => {
    void persistSettings({ speech_language: speech.value });
  });

  const thinking = $("thinkingSelect") as HTMLSelectElement;
  thinking.addEventListener("change", () => {
    store.thinking = thinking.value;
    // Kept on the provider so the choice survives a restart.
    const provider = activeProvider();
    if (provider) void saveProviderThinking(provider.id, thinking.value);
  });
}

function activeProvider() {
  return store.settings?.providers.find((p) => p.id === store.settings?.active_provider) ?? null;
}

async function saveProviderThinking(providerId: string, effort: string) {
  if (!store.settings) return;
  const providers = store.settings.providers.map((p) =>
    p.id === providerId ? { ...p, thinking_effort: effort } : p,
  );
  await persistSettings({ providers });
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

/**
 * The bubble a run is currently filling.
 *
 * Tool calls used to land as separate centred rows, which pushed the actual
 * answer around and looked nothing like the finished message. Now a run opens
 * one assistant bubble and the steps accumulate inside it, so the layout while
 * working is the layout afterwards.
 */
function liveEntry(): UiEntry {
  const existing = store.entries.find((e) => e.transient && e.sender === "assistant");
  if (existing) return existing;
  const entry = makeEntry("assistant", "", { steps: [], live: true, thoughts: "" }, true);
  pushEntry(entry);
  return entry;
}

function bindAgentEvents() {
  void onAgentEvent((payload) => {
    const kind = String(payload.kind ?? "");
    const entry = liveEntry();
    const meta = entry.meta as {
    steps?: RunStep[];
    note?: string;
    live?: boolean;
    thoughts?: string;
    thinkingSince?: number;
  };

    if (kind === "delta") {
      // The answer as it is written. `live` stays set until the run ends, so
      // the spinner keeps turning under the growing text.
      entry.text += String(payload.text ?? "");
    } else if (kind === "thought") {
      meta.thoughts = (meta.thoughts ?? "") + String(payload.text ?? "");
      if (!meta.thinkingSince) meta.thinkingSince = Date.now();
    } else if (kind === "no_stream") {
      meta.note = t("chat.noStream");
    } else if (kind === "step") {
      const step = payload.step as RunStep | undefined;
      if (!step) return;
      meta.steps = [...(meta.steps ?? []), step];
    } else if (kind === "llm_retry") {
      meta.note = `${t("chat.key", { n: Number(payload.key_index ?? 0) + 1 })}: ${payload.verdict}`;
    } else if (kind === "compacting") {
      meta.note = t("cmd.autoCompacting", {
        used: Number(payload.used ?? 0),
        window: Number(payload.window ?? 0),
      });
    } else if (kind === "llm_wait") {
      meta.note = String(payload.message ?? "");
    } else {
      return;
    }
    renderChat();
  });
}

async function boot() {
  bindModalDismiss();
  bindTopbar();
  bindPanels();
  bindComposer();
  bindTabs();
  bindAgentEvents();
  bindContextMenus();

  try {
    const data = await api.bootstrap();
    store.info = data.info;
    store.settings = data.settings;
    store.profiles = data.profiles;
    store.pending = data.pending;
    store.mode = data.settings.agent_mode;
    store.security = data.settings.security_level;
    applyLanguage(data.settings.ui_language === "en" ? "en" : "ru");

    const speech = $("speechLang") as HTMLSelectElement;
    speech.value = data.settings.speech_language || (data.settings.ui_language === "en" ? "en" : "ru");
    const thinking = $("thinkingSelect") as HTMLSelectElement;
    store.thinking =
      data.settings.providers.find((p) => p.id === data.settings.active_provider)
        ?.thinking_effort ?? "";
    thinking.value = store.thinking;
    setIndexCounts(data.index.models.map((m) => [m.id, m.men.length] as [string, number]));

    void refreshLocalModels();

    const preferred = data.settings.active_model_id ?? data.profiles[0]?.id ?? null;
    if (preferred) await selectProfile(preferred, false);

    renderAll();

    if (data.profiles.length === 0) {
      pushEntry(systemNote("hint.firstRun"));
      renderChat();
    } else if (!activeProviderReady()) {
      pushEntry(systemNote("hint.noKey"));
      renderChat();
    }
  } catch (error) {
    toast(t("toast.bootFailed", { error: errorText(error) }), "error");
  }
}

void boot();
