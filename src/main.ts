import { applyStatic, lang, setLang, t, type Lang } from "./i18n";
import { openUrl } from "@tauri-apps/plugin-opener";
import { api, errorText, onAgentEvent } from "./api";
import type { ModalDeps } from "./deps";
import { $, bindModalDismiss, closeModal, confirmDialog, escapeHtml, openModal, toast } from "./dom";
import {
  copyText,
  editingEntries,
  openContextMenu,
  selectionWithin,
  type MenuEntry,
} from "./context-menu";
import { dressSelect, syncDressedSelects } from "./dropdown";
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
  type Attachment,
  type QueuedMessage,
  type UiEntry,
} from "./store";
import type { AgentMode, RunStep, SecurityLevel, Settings, UpdateInfo } from "./types";
import {
  renderAll,
  renderChat,
  renderMen,
  renderProfiles,
  renderAttachments,
  renderQueue,
  renderSelection,
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
  leaveOverlayChat();
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
  if (!store.activeModelId) return;
  // Messages picked in one chat mean nothing in the next one.
  cancelSelection();
  try {
    const log = await api.getAgentLog(store.activeModelId, store.activeManId);
    store.entries = log.entries.slice(-120);
  } catch (error) {
    console.error("chat load failed", error);
    store.entries = [];
  }

  // The correspondence tells the chat which drafts have already been filed, so
  // it can stop offering to file them twice.
  restoreParked(currentTarget());

  store.thread = null;
  if (store.activeManId) {
    try {
      store.thread = await api.getChat(store.activeModelId, store.activeManId);
    } catch (error) {
      console.error("thread load failed", error);
    }
  }
}

/**
 * Close a temporary or master chat when the operator opens a saved one.
 *
 * Both of those live in memory over whatever is selected behind them, and
 * leaving one open while switching made the same conversation appear in every
 * profile and every dossier — the messages followed the operator around.
 */
function leaveOverlayChat() {
  if (!store.temporary && !store.master) return;
  const wasTemporary = store.temporary;
  store.temporary = false;
  store.master = false;
  $("btnTemporary").classList.remove("active");
  $("btnMaster").classList.remove("active");
  if (wasTemporary) toast(t("cmd.temporaryEnded"), "info");
}

async function selectMan(manId: string | null) {
  leaveOverlayChat();
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

const deps: ModalDeps = {
  refresh,
  selectProfile: (id) => selectProfile(id),
  selectMan,
  checkUpdate: () => offerUpdate(true),
};

// ---------------------------------------------------------------------------
// agent run
// ---------------------------------------------------------------------------

function activeProviderReady(): boolean {
  const provider = store.settings?.providers.find(
    (p) => p.id === store.settings?.active_provider,
  );
  return Boolean(provider && provider.key_count > 0);
}

/**
 * Picking messages, the way a messenger does it.
 *
 * The first message is picked from its right-click menu — dragging across a
 * bubble to copy its text must never mark it for deletion. Once picking has
 * started the whole row is a target and a drag paints across rows. A bar over
 * the composer counts what is picked, Escape drops the lot. Deleting takes them
 * out of the operator's chat, and — when they
 * were filed into a man's correspondence — offers to take them out of there
 * too, which is what actually keeps them out of the next request.
 */
function startSelection(entryId: string) {
  store.selecting = true;
  store.selected = [entryId];
  renderChat();
  renderSelection();
}

function toggleSelected(entryId: string) {
  store.selected = store.selected.includes(entryId)
    ? store.selected.filter((id) => id !== entryId)
    : [...store.selected, entryId];
  renderChat();
  renderSelection();
}

function cancelSelection() {
  if (!store.selecting) return;
  store.selecting = false;
  store.selected = [];
  renderChat();
  renderSelection();
}

/**
 * The filed messages that say the same thing as the picked entries.
 *
 * A draft rarely reaches the correspondence byte for byte — a greeting gets
 * trimmed, a line break moves — so two texts count as the same message when
 * one contains the other and there is enough of it to be sure.
 */
function filedCounterparts(): string[] {
  if (!store.thread) return [];
  const flatten = (text: string) => text.replace(/\s+/g, " ").trim().toLowerCase();
  const picked = store.entries
    .filter((entry) => store.selected.includes(entry.id))
    .map((entry) => flatten(entry.text))
    .filter(Boolean);

  return store.thread.messages
    .filter((message) => {
      const filed = flatten(message.text);
      if (!filed) return false;
      return picked.some((text) => {
        if (text === filed) return true;
        const shorter = Math.min(text.length, filed.length);
        return shorter >= 40 && (text.includes(filed) || filed.includes(text));
      });
    })
    .map((message) => message.id);
}

async function deleteSelected() {
  const ids = [...store.selected];
  if (ids.length === 0) return;

  const filed = filedCounterparts();
  const card = openModal(
    `<h3>${t("chat.deleteTitle")}</h3>` +
      `<div class="modal-sub">${escapeHtml(t("chat.deleteBody", { n: ids.length }))}</div>` +
      (filed.length > 0
        ? `<label class="toggle wide"><input type="checkbox" id="alsoThread" checked />` +
          `<span>${escapeHtml(t("chat.deleteAlsoThread", { n: filed.length }))}</span></label>`
        : "") +
      `<div class="modal-actions">` +
      `<button class="btn btn-secondary" data-act="cancel">${t("common.cancel")}</button>` +
      `<button class="btn btn-danger" data-act="ok">${t("common.delete")}</button></div>`,
  );
  card.querySelector('[data-act="cancel"]')?.addEventListener("click", closeModal);
  card.querySelector('[data-act="ok"]')?.addEventListener("click", () => {
    const alsoThread =
      filed.length > 0 && (card.querySelector("#alsoThread") as HTMLInputElement | null)?.checked;
    closeModal();
    void removeMessages(ids, alsoThread ? filed : []);
  });
}

async function removeMessages(ids: string[], filed: string[]) {
  try {
    if (store.master) {
      const log = await api.deleteMasterEntries(ids);
      store.entries = log.entries.slice(-120);
    } else if (store.activeModelId) {
      // A run that was never written down (a temporary chat) has nothing to
      // delete on disk; dropping it from the screen is the whole job.
      if (store.temporary) {
        store.entries = store.entries.filter((entry) => !ids.includes(entry.id));
      } else {
        const log = await api.deleteAgentEntries(store.activeModelId, store.activeManId, ids);
        store.entries = log.entries.slice(-120);
      }
      if (filed.length > 0 && store.activeManId) {
        store.thread = await api.deleteChatMessages(
          store.activeModelId,
          store.activeManId,
          filed,
        );
      }
    }
    toast(t("toast.messagesDeleted", { n: ids.length + filed.length }), "success");
  } catch (error) {
    toast(errorText(error), "error");
  } finally {
    cancelSelection();
    void refreshContextGauge();
  }
}

/**
 * Offer the newest release.
 *
 * The check reads the release list and nothing else; the download happens in
 * the operator's browser, and only after they say so. A version turned down is
 * remembered, so the same offer is not made twice.
 */
async function offerUpdate(manual: boolean) {
  let info: UpdateInfo;
  try {
    info = await api.checkUpdate();
  } catch (error) {
    if (manual) toast(errorText(error), "error");
    return;
  }

  if (!info.newer) {
    if (manual) toast(t("update.upToDate", { version: info.current }), "success");
    return;
  }
  if (!manual && store.settings?.update_skipped === info.version) return;

  const notes = info.notes.trim().split("\n").slice(0, 12).join("\n");
  const card = openModal(
    `<h3>${escapeHtml(t("update.title", { version: info.version }))}</h3>` +
      `<div class="modal-sub">${escapeHtml(t("update.sub", { current: info.current }))}</div>` +
      (notes ? `<div class="code-block">${escapeHtml(notes)}</div>` : "") +
      `<div class="modal-actions">` +
      `<button class="btn btn-secondary" data-act="skip">${t("update.skip")}</button>` +
      `<button class="btn btn-secondary" data-act="close">${t("update.later")}</button>` +
      `<button class="btn btn-primary" data-act="get">${t("update.get")}</button></div>`,
  );
  card.querySelector('[data-act="close"]')?.addEventListener("click", closeModal);
  card.querySelector('[data-act="skip"]')?.addEventListener("click", () => {
    closeModal();
    void persistSettings({ update_skipped: info.version });
  });
  card.querySelector('[data-act="get"]')?.addEventListener("click", () => {
    closeModal();
    // The installer is fetched and run by the operator, in their own browser.
    void openUrl(info.download ?? info.page).catch((error) => toast(errorText(error), "error"));
  });
}

/** Which conversation a message belongs to. */
interface ChatTarget {
  modelId: string | null;
  manId: string | null;
  master: boolean;
}

/** The chat on screen right now. */
function currentTarget(): ChatTarget {
  return { modelId: store.activeModelId, manId: store.activeManId, master: store.master };
}

function sameTarget(a: ChatTarget, b: ChatTarget): boolean {
  return a.master === b.master && a.modelId === b.modelId && a.manId === b.manId;
}

function targetKey(target: ChatTarget): string {
  return target.master ? "master" : `${target.modelId ?? ""}:${target.manId ?? ""}`;
}

/** What to call the chat in a notice, when the operator is looking elsewhere. */
function targetName(target: ChatTarget): string {
  if (target.master) return t("master.scope");
  const profile = store.profiles.find((p) => p.id === target.modelId);
  const man = store.men.find((m) => m.id === target.manId);
  const parts = [profile?.name ?? target.modelId ?? "", man?.name ?? ""].filter(Boolean);
  return parts.join(" — ");
}

/**
 * Answers that arrived while the operator was in another chat.
 *
 * A run keeps going when the operator switches away, and its answer belongs to
 * the chat it was asked in, not the one now on screen. Stored answers are put
 * back the moment that chat is opened again — which also covers the temporary
 * chats the core deliberately never writes to the log.
 */
const parked = new Map<string, UiEntry[]>();

/** The run in flight, and the chat it answers to. */
let runTarget: ChatTarget | null = null;

/** Show an entry in its own chat, or keep it until that chat is open again. */
function deliver(target: ChatTarget, entry: UiEntry): boolean {
  if (sameTarget(target, currentTarget())) {
    pushEntry(entry);
    return true;
  }
  const key = targetKey(target);
  parked.set(key, [...(parked.get(key) ?? []), entry]);
  toast(t("toast.answerElsewhere", { chat: targetName(target) }), "info");
  return false;
}

/** Put back whatever finished while this chat was closed. */
function restoreParked(target: ChatTarget) {
  const key = targetKey(target);
  const waiting = parked.get(key);
  if (!waiting || waiting.length === 0) return;
  parked.delete(key);
  // The log already holds what the core wrote down, and reopening the chat has
  // just loaded it. Only what the log does not know about — temporary chats,
  // errors, system notes — needs putting back, so anything already on screen
  // word for word is left alone rather than shown twice.
  const recent = store.entries.slice(-40);
  for (const entry of waiting) {
    const known = recent.some(
      (existing) =>
        existing.id === entry.id ||
        (existing.sender === entry.sender && existing.text.trim() === entry.text.trim()),
    );
    if (known) continue;
    store.entries.push(entry);
  }
}

/** How many pictures one message may carry, and how big each may be. */
const MAX_ATTACHMENTS = 8;
const MAX_IMAGE_BYTES = 8 * 1024 * 1024;

/** A file's bytes as base64, without the `data:` prefix the model does not want. */
function readBase64(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(reader.error);
    reader.onload = () => {
      const url = String(reader.result);
      resolve(url.slice(url.indexOf(",") + 1));
    };
    reader.readAsDataURL(file);
  });
}

/**
 * Attach pictures to the message being written.
 *
 * Screenshots of a profile, photos he sent — whatever the operator is asking
 * about. They are held in memory until the message goes out, so nothing heavy
 * is written to the log.
 */
async function addAttachments(files: File[]) {
  for (const file of files) {
    if (!file.type.startsWith("image/")) continue;
    if (store.attachments.length >= MAX_ATTACHMENTS) {
      toast(t("toast.attachLimit", { n: MAX_ATTACHMENTS }), "error");
      break;
    }
    if (file.size > MAX_IMAGE_BYTES) {
      toast(t("toast.attachTooBig", { name: file.name || "image" }), "error");
      continue;
    }
    try {
      store.attachments.push({
        id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
        name: file.name || t("composer.pastedImage"),
        mime: file.type,
        data: await readBase64(file),
      });
    } catch (error) {
      console.error("attachment read failed", error);
      toast(t("toast.attachFailed"), "error");
    }
  }
  renderAttachments();
}

/**
 * Look at one picture full size, and copy it back out.
 *
 * The clipboard only takes PNG, so anything else is redrawn once on a canvas
 * before it goes; that also means a copied picture pastes into any other app.
 */
function openImage(src: string, name: string) {
  const card = openModal(
    `<h3>${escapeHtml(name || t("composer.pastedImage"))}</h3>` +
      `<div class="image-view"><img src="${src}" alt="${escapeHtml(name)}" /></div>` +
      `<div class="modal-actions">` +
      `<button class="btn btn-secondary" data-act="copy-image">${t("ctx.copy")}</button>` +
      `<button class="btn btn-primary" data-act="close">${t("common.close")}</button></div>`,
  );
  card.querySelector('[data-act="close"]')?.addEventListener("click", closeModal);
  card.querySelector('[data-act="copy-image"]')?.addEventListener("click", () => {
    void copyImage(src);
  });
}

/** Put a picture on the clipboard, as the PNG every app understands. */
async function copyImage(src: string) {
  try {
    const image = new Image();
    image.src = src;
    await image.decode();
    const canvas = document.createElement("canvas");
    canvas.width = image.naturalWidth;
    canvas.height = image.naturalHeight;
    canvas.getContext("2d")?.drawImage(image, 0, 0);
    const blob = await new Promise<Blob | null>((resolve) =>
      canvas.toBlob((result) => resolve(result), "image/png"),
    );
    if (!blob) throw new Error("no blob");
    await navigator.clipboard.write([new ClipboardItem({ "image/png": blob })]);
    toast(t("chat.copied"), "success");
  } catch (error) {
    console.error("image copy failed", error);
    toast(t("toast.copyImageFailed"), "error");
  }
}

/** Add one picture that arrived as bytes rather than a file. */
function addRaw(name: string, mime: string, data: string) {
  if (store.attachments.length >= MAX_ATTACHMENTS) {
    toast(t("toast.attachLimit", { n: MAX_ATTACHMENTS }), "error");
    return;
  }
  store.attachments.push({
    id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
    name,
    mime,
    data,
  });
  renderAttachments();
}

/**
 * Attach a picture that came in as a link.
 *
 * Dragging an image out of a browser hands over its address, not the file, and
 * the site it came from will usually refuse a request made from inside the app.
 * The core downloads it instead. A `data:` link already carries the bytes.
 */
async function addFromUrl(url: string) {
  if (url.startsWith("data:")) {
    const match = /^data:([^;,]+)[^,]*,(.*)$/s.exec(url);
    if (!match || !match[1].startsWith("image/")) return;
    addRaw(t("composer.pastedImage"), match[1], match[2]);
    return;
  }
  try {
    const image = await api.fetchImage(url);
    addRaw(image.name, image.mime, image.data);
  } catch (error) {
    toast(errorText(error), "error");
  }
}

/** Every picture a paste or a drop is carrying, whichever way it packed them. */
function imageFiles(data: DataTransfer | null): File[] {
  if (!data) return [];
  const files = Array.from(data.files ?? []);
  // A screenshot copied from another window arrives as an item, not a file.
  for (const item of Array.from(data.items ?? [])) {
    if (item.kind !== "file" || !item.type.startsWith("image/")) continue;
    const file = item.getAsFile();
    if (file && !files.some((known) => known.size === file.size && known.type === file.type)) {
      files.push(file);
    }
  }
  return files.filter((file) => file.type.startsWith("image/"));
}

/** The address of a picture inside a paste or a drop, when there is no file. */
function imageLink(data: DataTransfer | null): string | null {
  if (!data) return null;
  const html = data.getData("text/html");
  const src = html ? /<img[^>]+src="([^"]+)"/i.exec(html)?.[1] : null;
  if (src) return src;
  const uri = data.getData("text/uri-list").split("\n")[0]?.trim();
  if (uri && !uri.startsWith("#")) return uri;
  const text = data.getData("text/plain").trim();
  return /^(https?:|data:image\/)/.test(text) ? text : null;
}

/** Empty the composer and let it shrink back to one line. */
function clearComposer() {
  const input = $("composerInput") as HTMLTextAreaElement;
  input.value = "";
  input.style.height = "auto";
}

/**
 * Enter, or the button.
 *
 * A run in flight no longer swallows what comes next: the message lines up
 * above the composer and goes out on its own the moment the run ends, so the
 * operator can keep typing instead of watching a spinner.
 */
async function sendMessage() {
  const typed = ($("composerInput") as HTMLTextAreaElement).value.trim();
  const attached = store.attachments;

  if (store.busy) {
    if (!typed && attached.length === 0) return;
    store.queue.push({ text: typed, attachments: attached, target: currentTarget() });
    store.attachments = [];
    clearComposer();
    renderQueue();
    renderAttachments();
    return;
  }

  store.attachments = [];
  clearComposer();
  renderAttachments();
  await dispatchMessage(typed, attached, currentTarget());

  // Everything typed while that ran, in the order it was typed and into the
  // chat it was typed in — switching away does not redirect it.
  while (store.queue.length > 0 && !store.busy) {
    const next = store.queue.shift() as QueuedMessage;
    renderQueue();
    await dispatchMessage(next.text, next.attachments, next.target as ChatTarget);
  }
}

/** Send one message the way the open mode wants it sent. */
async function dispatchMessage(
  typed: string,
  attached: Attachment[] = [],
  target: ChatTarget = currentTarget(),
) {
  if (typed.startsWith("/") && (await runSlashCommand(typed))) return;

  // Letters are not a conversation: the brief goes to one man or to a whole
  // list, and each letter comes back as its own card.
  if (store.mode === "letters" && !target.master) {
    await writeLetters(typed, target);
    return;
  }

  if (target.master) {
    if (!typed) return;
    await sendToMaster(typed, attached, target);
    return;
  }
  const text = typed;
  if (!text) return;
  if (!target.modelId) {
    toast(t("toast.pickProfile"), "error");
    return;
  }
  if (!activeProviderReady()) {
    toast(t("toast.needKey"), "error");
    void openKeysModal(deps);
    return;
  }

  store.busy = true;
  runTarget = target;
  markSending(true);
  deliver(target, makeEntry("user", text, attached.length > 0 ? { images: attached } : null));
  renderChat();
  renderScope();

  try {
    const output = await api.runAgent({
      model_id: target.modelId,
      man_id: target.manId,
      mode: store.mode,
      security: store.security,
      message: text,
      channel: store.channel,
      log_incoming: store.logIncoming,
      thinking_effort: store.thinking || undefined,
      temporary: store.temporary,
      images: attached.map(({ mime, data }) => ({ mime, data })),
    });

    const streamed = store.entries.find((e) => e.transient && e.sender === "assistant");
    const thoughts =
      output.thoughts || ((streamed?.meta as { thoughts?: string })?.thoughts ?? "");
    store.entries = store.entries.filter((e) => !e.transient);
    // A reply the app wrote itself arrives as a key, so it reads in the
    // interface language rather than the one the core was written in.
    const reply = output.reply_key ? t(output.reply_key) : output.reply;
    deliver(
      target,
      makeEntry("assistant", reply, {
        steps: output.steps as unknown as RunStep[],
        usage: output.usage,
        mode: output.mode,
        model: output.model,
        key_index: output.key_index,
        turns: output.turns,
        raw: output.raw,
        thoughts,
      }),
    );
    store.thoughts = "";

    if (output.pending.length > 0) {
      toast(t("toast.pendingCount", { n: output.pending.length }), "info");
    }
    if (store.activeModelId) store.men = await api.listMen(store.activeModelId);
    store.pending = await api.pendingList();
    if (store.activeManId && store.activeModelId) {
      store.thread = await api.getChat(store.activeModelId, store.activeManId);
    }
  } catch (error) {
    store.entries = store.entries.filter((e) => !e.transient);
    deliver(target, makeEntry("system", t("chat.error", { message: errorText(error) })));
    toast(errorText(error), "error");
  } finally {
    store.busy = false;
    runTarget = null;
    markSending(false);
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
  syncDressedSelects();
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

  $("selectBar").addEventListener("click", (event) => {
    const act = (event.target as HTMLElement).closest<HTMLElement>("[data-act]")?.dataset.act;
    if (act === "select-cancel") cancelSelection();
    if (act === "select-delete") void deleteSelected();
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

    if (btn.dataset.act === "raw") {
      const meta = (entry.meta ?? {}) as { raw?: string };
      const card = openModal(
        `<h3>${t("chat.rawTitle")}</h3>` +
          `<div class="modal-sub">${t("chat.rawSub")}</div>` +
          `<div class="code-block raw-payload">${escapeHtml(meta.raw ?? "")}</div>` +
          `<div class="modal-actions">` +
          `<button class="btn btn-secondary" data-act="copy-raw">${t("ctx.copy")}</button>` +
          `<button class="btn btn-primary" data-act="close">${t("common.close")}</button></div>`,
      );
      card.querySelector('[data-act="close"]')?.addEventListener("click", closeModal);
      card.querySelector('[data-act="copy-raw"]')?.addEventListener("click", () => {
        void copyText(meta.raw ?? "");
        toast(t("chat.copied"), "success");
      });
      return;
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
        // Filed: the button that offered it has nothing left to offer.
        if (store.thread && manId === store.activeManId) {
          store.thread = await api.getChat(store.activeModelId, manId);
          renderChat();
        }
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
  if (entry && !entry.transient) {
    const picked = store.selecting && store.selected.includes(entry.id);
    entries.push({
      label: picked ? t("chat.unselect") : t("chat.select"),
      onSelect: () => (store.selecting ? toggleSelected(entry.id) : startSelection(entry.id)),
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
      // The chat is what grows: the model is handed everything said so far and
      // the summary it writes replaces it, here and in the log.
      const before = store.entries.filter((e) => !e.transient).length;
      const log = await api.compactChat(store.activeModelId, store.activeManId);
      store.entries = log.entries.map((entry) => ({ ...entry }));
      renderChat();
      toast(t("cmd.compacted", { before, after: store.entries.length }), "success");

      // With a dossier open the correspondence has its own context, folded the
      // same way.
      if (store.activeManId) {
        await api.compactContext(store.activeModelId, store.activeManId);
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
 * Open a throwaway chat, or close it and return to the saved one.
 *
 * A temporary chat is not written to the log at all: the copilot still reads
 * the dossier and still writes facts, notes and messages, but the conversation
 * itself leaves no trace.
 */
async function toggleTemporaryChat() {
  store.temporary = !store.temporary;
  // The two overlay chats are alternatives, never both at once.
  if (store.temporary) store.master = false;
  $("btnTemporary").classList.toggle("active", store.temporary);
  $("btnMaster").classList.toggle("active", store.master);
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
  if (store.master) store.temporary = false;
  $("btnMaster").classList.toggle("active", store.master);
  $("btnTemporary").classList.toggle("active", store.temporary);
  store.entries = [];

  void refreshContextGauge();
  if (store.master) {
    try {
      const log = await api.getMasterLog();
      store.entries = log.entries.slice(-120).map((entry) => ({ ...entry }));
    } catch (error) {
      console.error("master log", error);
    }
    restoreParked(currentTarget());
    cancelSelection();
    if (store.entries.length === 0) pushEntry(systemNote("master.hello"));
  } else {
    await loadChat();
  }
  renderAll();
}

async function sendToMaster(
  text: string,
  attached: Attachment[] = [],
  target: ChatTarget = currentTarget(),
) {
  store.busy = true;
  runTarget = target;
  markSending(true);
  deliver(target, makeEntry("user", text, attached.length > 0 ? { images: attached } : null));
  renderChat();
  renderScope();

  try {
    const output = await api.masterChat({
      message: text,
      security: store.security,
      thinking_effort: store.thinking || undefined,
      temporary: store.temporary,
      images: attached.map(({ mime, data }) => ({ mime, data })),
    });
    store.entries = store.entries.filter((e) => !e.transient);
    deliver(
      target,
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
    deliver(target, makeEntry("system", t("chat.error", { message: errorText(error) })));
    toast(errorText(error), "error");
  } finally {
    store.busy = false;
    runTarget = null;
    markSending(false);
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
async function writeLetters(brief: string, target: ChatTarget = currentTarget()) {
  if (!target.modelId) {
    toast(t("toast.pickProfile"), "error");
    return;
  }
  if (store.busy) return;

  const recipients = target.manId ? [target.manId] : visibleMen().map((m) => m.id);
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

  store.busy = true;
  runTarget = target;
  markSending(true);
  if (brief) deliver(target, makeEntry("user", brief));
  pushEntry(makeEntry("system", t("letters.writing", { n: recipients.length }), null, true));
  renderChat();
  renderScope();

  try {
    const output = await api.writeLetters({
      model_id: target.modelId,
      man_ids: target.manId ? recipients : [],
      brief,
      channel: store.channel,
      thinking_effort: store.thinking || undefined,
      temporary: store.temporary,
    });
    store.entries = store.entries.filter((e) => !e.transient);
    for (const letter of output.letters) {
      deliver(
        target,
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
    runTarget = null;
    markSending(false);
    renderAll();
  }
}

/** The send button stays live while a run goes — it says so, and it queues. */
function markSending(busy: boolean) {
  $("btnSend").textContent = busy ? t("composer.queue") : t("composer.send");
}

function bindComposer() {
  const input = $("composerInput") as HTMLTextAreaElement;
  $("btnSend").addEventListener("click", () => void sendMessage());

  // Pictures: picked, pasted or dropped onto the composer.
  const attachInput = $("attachInput") as HTMLInputElement;
  $("btnAttach").addEventListener("click", () => attachInput.click());
  attachInput.addEventListener("change", () => {
    const files = Array.from(attachInput.files ?? []);
    attachInput.value = "";
    void addAttachments(files);
  });
  $("attachments").addEventListener("click", (event) => {
    const target = event.target as HTMLElement;
    const drop = target.closest<HTMLElement>("[data-attach]");
    if (drop) {
      store.attachments = store.attachments.filter((item) => item.id !== drop.dataset.attach);
      renderAttachments();
      return;
    }
    const image = target.closest<HTMLImageElement>(".thumb img");
    if (image) openImage(image.src, image.alt);
  });
  // Paste from the right-click menu: the clipboard is read there, and the
  // pictures it held arrive here.
  input.addEventListener("images-pasted", (event) => {
    void addAttachments((event as CustomEvent<File[]>).detail);
  });

  input.addEventListener("paste", (event) => {
    const files = imageFiles(event.clipboardData);
    if (files.length > 0) {
      event.preventDefault();
      void addAttachments(files);
      return;
    }
    // Copying an image inside a browser puts a link on the clipboard, and only
    // a link; the picture itself has to be fetched.
    const link = imageLink(event.clipboardData);
    if (!link || !/^data:image\//.test(link)) return;
    event.preventDefault();
    void addFromUrl(link);
  });

  const composer = document.querySelector(".composer") as HTMLElement;
  composer.addEventListener("dragover", (event) => {
    if (!event.dataTransfer?.types.includes("Files")) return;
    event.preventDefault();
    composer.classList.add("dropping");
  });
  composer.addEventListener("dragleave", () => composer.classList.remove("dropping"));
  composer.addEventListener("drop", (event) => {
    composer.classList.remove("dropping");
    const files = imageFiles(event.dataTransfer);
    if (files.length > 0) {
      event.preventDefault();
      void addAttachments(files);
      return;
    }
    const link = imageLink(event.dataTransfer);
    if (!link) return;
    event.preventDefault();
    void addFromUrl(link);
  });

  // A queued line can be taken back until it goes out.
  $("queue").addEventListener("click", (event) => {
    const drop = (event.target as HTMLElement).closest<HTMLElement>("[data-queue]");
    if (!drop) return;
    store.queue.splice(Number(drop.dataset.queue), 1);
    renderQueue();
  });
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

  // The row of controls under the composer opens the app's own menus rather
  // than the platform's popup.
  for (const id of ["speechLang", "thinkingSelect", "channelSelect"]) {
    dressSelect($(id) as HTMLSelectElement);
  }

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

/**
 * Picking with the mouse, once picking has started.
 *
 * The first message is picked from its right-click menu; from then on the chat
 * behaves like a list. Only the height of the pointer matters — a message is
 * picked when the cursor passes its line, whether the pointer is over the
 * bubble, in the empty column beside it, or in the gap between two rows — and
 * holding the button while moving sweeps everything between where the drag
 * began and where it is now, so a fast drag skips nothing. Dragging past the
 * top or bottom edge scrolls the chat along. Letting go leaves the selection
 * as it stands; going back over the same rows takes them out again.
 */
function bindSelectionPainting() {
  const container = $("messages");

  /** Whether the drag is adding rows or taking them out. */
  let adding = true;
  /** Where the drag began, and what was picked before it started. */
  let anchor = -1;
  let before: string[] = [];
  let pointerY = 0;
  let frame = 0;

  // Only the message rows: the buttons inside a bubble carry the same
  // attribute, and counting them as rows makes every index wrong.
  const rows = () => Array.from(container.querySelectorAll<HTMLElement>(".msg[data-entry]"));

  /** The row the pointer is level with — the nearest one when it is in a gap. */
  function rowAt(y: number): number {
    const list = rows();
    if (list.length === 0) return -1;
    let nearest = 0;
    let distance = Infinity;
    for (let index = 0; index < list.length; index += 1) {
      const box = list[index].getBoundingClientRect();
      if (y >= box.top && y <= box.bottom) return index;
      const gap = y < box.top ? box.top - y : y - box.bottom;
      if (gap < distance) {
        distance = gap;
        nearest = index;
      }
    }
    return nearest;
  }

  /** Everything between the anchor and here, added to or taken out of before. */
  function sweep(to: number) {
    if (anchor < 0 || to < 0) return;
    const list = rows();
    const from = Math.min(anchor, to);
    const till = Math.max(anchor, to);
    const touched = list
      .slice(from, till + 1)
      .map((row) => row.dataset.entry ?? "")
      .filter(Boolean);

    const next = adding
      ? [...before, ...touched.filter((id) => !before.includes(id))]
      : before.filter((id) => !touched.includes(id));
    if (next.length === store.selected.length && next.every((id, i) => id === store.selected[i])) {
      return;
    }
    store.selected = next;
    renderChat();
    renderSelection();
  }

  /**
   * Dragging into the last strip of the chat keeps it moving under the pointer.
   *
   * The strip is narrow and the speed follows how far into it the pointer has
   * gone, so resting the cursor near the top row does not run the selection
   * away to the first message; a real reach past the edge scrolls briskly.
   */
  function follow() {
    if (anchor < 0) return;
    const box = container.getBoundingClientRect();
    const margin = 24;
    let over = 0;
    if (pointerY < box.top + margin) over = pointerY - (box.top + margin);
    else if (pointerY > box.bottom - margin) over = pointerY - (box.bottom - margin);

    if (over !== 0) {
      const step = Math.sign(over) * Math.min(18, 1 + Math.abs(over) / 6);
      const was = container.scrollTop;
      container.scrollTop += step;
      // At either end there is nothing left to reveal, so nothing to re-sweep.
      if (container.scrollTop !== was) sweep(rowAt(pointerY));
    }
    frame = requestAnimationFrame(follow);
  }

  function stop() {
    anchor = -1;
    before = [];
    if (frame) cancelAnimationFrame(frame);
    frame = 0;
  }

  container.addEventListener("pointerdown", (event) => {
    if (!store.selecting || event.button !== 0) return;
    // Buttons inside a message keep doing their own job.
    if ((event.target as HTMLElement).closest("button, a, summary")) return;
    const index = rowAt(event.clientY);
    const id = rows()[index]?.dataset.entry;
    if (!id) return;

    event.preventDefault();
    adding = !store.selected.includes(id);
    anchor = index;
    before = [...store.selected];
    pointerY = event.clientY;
    sweep(index);
    frame = requestAnimationFrame(follow);
  });

  window.addEventListener("pointermove", (event) => {
    if (anchor < 0) return;
    pointerY = event.clientY;
    sweep(rowAt(pointerY));
  });

  for (const type of ["pointerup", "pointercancel"]) {
    window.addEventListener(type, stop);
  }
}

/** A picture inside a message opens full size, and can be copied from there. */
function bindMessageImages() {
  $("messages").addEventListener("click", (event) => {
    const image = (event.target as HTMLElement).closest<HTMLImageElement>(".msg-thumbs img");
    if (!image) return;
    openImage(image.src, image.alt);
  });
}

/** A picture dropped outside the composer must not replace the app window. */
function bindWindowDrops() {
  for (const type of ["dragover", "drop"]) {
    window.addEventListener(type, (event) => {
      const inside = (event.target as HTMLElement | null)?.closest?.(".composer");
      if (!inside) event.preventDefault();
    });
  }
}

function bindAgentEvents() {
  void onAgentEvent((payload) => {
    const kind = String(payload.kind ?? "");

    // The operator may have walked to another chat while this run goes on. Its
    // progress belongs to the chat it was started in, so nothing is drawn into
    // the one now on screen; the answer itself is put back on return.
    if (runTarget && !sameTarget(runTarget, currentTarget())) return;

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
  bindMessageImages();
  bindSelectionPainting();
  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") cancelSelection();
  });
  bindWindowDrops();
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

    // A quiet look at the release page a moment after the window is usable.
    if (data.settings.update_check) {
      window.setTimeout(() => void offerUpdate(false), 4000);
    }

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
