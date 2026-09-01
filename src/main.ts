import { api, errorText, onAgentEvent } from "./api";
import { $, bindModalDismiss, promptDialog, toast } from "./dom";
import {
  openDoctorModal,
  openKeysModal,
  openManEditor,
  openMasterModal,
  openPendingModal,
  openProfileEditor,
  type ModalDeps,
} from "./modals";
import { activeProfile, makeEntry, pushEntry, store } from "./store";
import type { AgentMode, RunStep, SecurityLevel } from "./types";
import { renderAll, renderChat, renderMen, renderProfiles, renderScope, renderTopbar, setIndexCounts } from "./views";

// ---------------------------------------------------------------------------
// data loading
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
  if (manId) {
    try {
      const man = await api.getMan(store.activeModelId!, manId);
      store.men = store.men.map((m) => (m.id === man.id ? man : m));
    } catch (error) {
      toast(errorText(error), "error");
    }
  }
  renderMen();
  renderScope();
}

async function persistSettings(patch: Partial<NonNullable<typeof store.settings>>) {
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

async function sendMessage() {
  const input = $("chatInput") as HTMLTextAreaElement;
  const text = input.value.trim();
  if (!text) return;
  if (!store.activeModelId) {
    toast("Сначала выбери модель", "error");
    return;
  }
  if (store.busy) return;

  const provider = store.settings?.providers.find((p) => p.id === store.settings?.active_provider);
  if (!provider || provider.key_count === 0) {
    toast("Добавь API-ключ в 🔑 перед запуском агента", "error");
    return;
  }

  store.busy = true;
  ($("btnSend") as HTMLButtonElement).disabled = true;
  input.value = "";
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

    // Drop the live step rows; the persisted entry carries the full trace.
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
      toast(`${output.pending.length} действий ждут подтверждения`, "info");
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
  $("btnDoctor").addEventListener("click", () => void openDoctorModal(deps));
  $("btnMaster").addEventListener("click", () => void openMasterModal(deps));
  $("btnPending").addEventListener("click", () => void openPendingModal(deps));
}

function bindPanels() {
  $("profileList").addEventListener("click", (event) => {
    const target = event.target as HTMLElement;
    if (target.dataset.act === "seed") {
      event.preventDefault();
      void api
        .seedDemo()
        .then(refresh)
        .then(() => toast("Демо-профиль создан", "success"))
        .catch((error) => toast(errorText(error), "error"));
      return;
    }
    const card = target.closest<HTMLElement>("[data-profile]");
    if (card?.dataset.profile) void selectProfile(card.dataset.profile);
  });

  $("menList").addEventListener("click", (event) => {
    const card = (event.target as HTMLElement).closest<HTMLElement>("[data-man]");
    if (!card?.dataset.man) return;
    if (card.dataset.man === store.activeManId) {
      void openManEditor(deps, card.dataset.man);
    } else {
      void selectMan(card.dataset.man);
    }
  });

  $("chatMessages").addEventListener("click", async (event) => {
    const btn = (event.target as HTMLElement).closest<HTMLButtonElement>("[data-act]");
    if (!btn) return;
    const entry = store.entries.find((e) => e.id === btn.dataset.entry);
    if (!entry) return;

    if (btn.dataset.act === "copy") {
      await navigator.clipboard.writeText(entry.text);
      toast("Скопировано", "success");
    }

    if (btn.dataset.act === "send-as-outgoing") {
      if (!store.activeModelId || !store.activeManId) {
        toast("Выбери мужчину, чтобы записать сообщение в историю", "error");
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
        toast("Записано в переписку", "success");
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

  $("btnAddProfile").addEventListener("click", async () => {
    const name = await promptDialog({ title: "Новая модель", label: "Имя модели" });
    if (!name) return;
    try {
      const profile = await api.createProfile({ name });
      await refresh();
      await selectProfile(profile.id);
      toast(`Профиль ${profile.name} создан`, "success");
    } catch (error) {
      toast(errorText(error), "error");
    }
  });

  $("btnAddMan").addEventListener("click", async () => {
    if (!store.activeModelId) {
      toast("Сначала выбери модель", "error");
      return;
    }
    const name = await promptDialog({ title: "Новый мужчина", label: "Имя" });
    if (!name) return;
    try {
      const man = await api.createMan(store.activeModelId, { name });
      store.men = await api.listMen(store.activeModelId);
      await selectMan(man.id);
      await loadIndexCounts();
      renderAll();
    } catch (error) {
      toast(errorText(error), "error");
    }
  });

  $("btnEditProfile").addEventListener("click", () => void openProfileEditor(deps));
  $("btnManCard").addEventListener("click", () => void openManEditor(deps));
}

function bindComposer() {
  const input = $("chatInput") as HTMLTextAreaElement;
  $("btnSend").addEventListener("click", () => void sendMessage());
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

function bindMobileTabs() {
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
          `Ключ #${Number(payload.key_index ?? 0) + 1} отвалился (${payload.verdict}), пробую следующий…`,
          null,
          true,
        ),
      );
      renderChat();
    } else if (kind === "llm_wait") {
      pushEntry(makeEntry("system", String(payload.message ?? "жду ключи…"), null, true));
      renderChat();
    }
  });
}

async function boot() {
  bindModalDismiss();
  bindTopbar();
  bindPanels();
  bindComposer();
  bindMobileTabs();
  bindAgentEvents();

  try {
    const data = await api.bootstrap();
    store.info = data.info;
    store.settings = data.settings;
    store.profiles = data.profiles;
    store.pending = data.pending;
    store.mode = data.settings.agent_mode;
    store.security = data.settings.security_level;
    setIndexCounts(data.index.models.map((m) => [m.id, m.men.length] as [string, number]));

    const preferred = data.settings.active_model_id ?? data.profiles[0]?.id ?? null;
    if (preferred) await selectProfile(preferred, false);

    renderAll();

    if (data.profiles.length === 0) {
      pushEntry(
        makeEntry(
          "system",
          "Профилей пока нет. Создай модель слева или загрузи демо-профиль, чтобы посмотреть, как это работает.",
        ),
      );
      renderChat();
    }
    if (!activeProfile()) renderScope();
  } catch (error) {
    toast(`Не удалось запустить ядро: ${errorText(error)}`, "error");
  }
}

void boot();
