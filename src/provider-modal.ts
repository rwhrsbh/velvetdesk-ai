import { api, errorText, onModelEvent } from "./api";
import type { ModalDeps } from "./deps";
import { closeModal, escapeHtml, openModal, toast } from "./dom";
import { t } from "./i18n";
import { unloadModel } from "./local-whisper";
import { store } from "./store";
import type {
  KeyStatus,
  LocalModel,
  ModelCatalog,
  ModelInfo,
  TrustedRoot,
  ModelProgress,
  ProviderConfig,
  Settings,
} from "./types";

/** Cached per provider so re-opening the dialog does not re-hit the API. */
const catalogs = new Map<string, ModelCatalog>();

const BASE_URL_PRESETS = [
  "https://api.groq.com/openai/v1",
  "https://openrouter.ai/api/v1",
  "https://api.openai.com/v1",
  "https://api.deepseek.com/v1",
  "https://api.together.xyz/v1",
  "https://api.mistral.ai/v1",
  // Local servers: Ollama, LM Studio, whisper.cpp server, faster-whisper-server.
  "http://localhost:11434/v1",
  "http://localhost:1234/v1",
  "http://localhost:8080/v1",
  "http://localhost:8000/v1",
];

/** The catalogue note lives in the dictionaries, keyed by model id. */
function modelNote(model: LocalModel): string {
  const key = `local.${model.id}.note`;
  const translated = t(key);
  return translated === key ? model.note : translated;
}

/**
 * The model list, filtered as the operator types.
 *
 * A gateway like OpenRouter answers with hundreds of entries, which is
 * unusable as a dropdown; free models come first because that is what someone
 * without a budget is looking for.
 */
function modelRows(models: ModelInfo[], selected: string, query: string): string {
  const needle = query.trim().toLowerCase();
  const matches = models.filter(
    (m) => !needle || m.id.toLowerCase().includes(needle) || m.label.toLowerCase().includes(needle),
  );
  if (matches.length === 0) return `<div class="empty-hint">${t("keys.noModels")}</div>`;

  return matches
    .slice(0, 200)
    .map(
      (m) =>
        `<button type="button" class="model-row${m.id === selected ? " active" : ""}" data-model="${escapeHtml(m.id)}">` +
        `<span class="model-name">${escapeHtml(m.label)}</span>` +
        (m.free ? `<span class="tag free">${t("keys.free")}</span>` : "") +
        (m.audio ? `<span class="tag">${t("keys.audioTag")}</span>` : "") +
        `</button>`,
    )
    .join("");
}

function formatSize(bytes: number): string {
  return `${Math.round(bytes / 1_048_576)} MB`;
}

/** Live download progress, written straight into the open dialog. */
function watchDownloads() {
  void onModelEvent((payload) => {
    const progress = payload as unknown as ModelProgress;
    const line = document.querySelector<HTMLElement>(`[data-progress="${progress.model_id}"]`);
    if (!line) return;
    const pct = progress.total ? Math.round((progress.received / progress.total) * 100) : 0;
    const done = formatSize(progress.received);
    const size = progress.total ? ` / ${formatSize(progress.total)} · ${pct}%` : "";
    line.textContent = `${progress.file_index + 1}/${progress.file_count} · ${done}${size}`;
  });
}

let watching = false;

export async function openKeysModal(deps: ModalDeps) {
  if (!watching) {
    watchDownloads();
    watching = true;
  }
  if (!store.settings) return;
  let settings: Settings = structuredClone(store.settings);
  let providerId = settings.active_provider ?? settings.providers[0]?.id ?? "";

  const provider = (): ProviderConfig =>
    settings.providers.find((p) => p.id === providerId) ?? settings.providers[0];

  /** Persist the form, then reload models with the stored keys. */
  const persist = async (patch: Partial<ProviderConfig>, quiet = false) => {
    const target = settings.providers.find((p) => p.id === providerId);
    if (target) Object.assign(target, patch);
    settings.active_provider = providerId;
    try {
      settings = await api.saveSettings(settings);
      store.settings = settings;
      if (!quiet) toast(t("toast.saved"), "success");
    } catch (error) {
      toast(errorText(error), "error");
    }
  };

  const loadModels = async (force = false): Promise<ModelCatalog | null> => {
    const p = provider();
    if (!force && catalogs.has(p.id)) return catalogs.get(p.id)!;
    if (p.key_count === 0) return null;
    const catalog = await api.listProviderModels(p.id);
    catalogs.set(p.id, catalog);
    // The backend stores the version that answered; mirror it locally.
    const fresh = await api.getSettings();
    settings = fresh;
    store.settings = fresh;
    return catalog;
  };

  const draw = async () => {
    const p = provider();
    let keys: KeyStatus[] = [];
    try {
      keys = await api.listKeys(p.id);
    } catch (error) {
      toast(errorText(error), "error");
    }
    const isLocal = settings.speech_engine === "local";
    let localModels: LocalModel[] = [];
    try {
      localModels = await api.listLocalModels();
    } catch (error) {
      console.error("local models", error);
    }
    const catalog = catalogs.get(p.id) ?? null;
    const isGemini = p.kind === "gemini";

    const modelOptions = catalog ? modelRows(catalog.models, p.model, "") : "";

    // Dictation may run through a different provider than the chat one, so an
    // operator on a text-only endpoint can still dictate via Gemini or Groq.
    const speechProvider =
      settings.providers.find((item) => item.id === settings.speech_provider) ?? p;
    const speechCatalog = catalogs.get(speechProvider.id) ?? null;
    const audioOptions = speechCatalog
      ? speechCatalog.models
          .filter((m) => m.audio)
          .map(
            (m) =>
              `<option value="${escapeHtml(m.id)}" ${
                m.id === speechProvider.transcribe_model ? "selected" : ""
              }>${escapeHtml(m.label)}</option>`,
          )
          .join("")
      : "";

    const card = openModal(`
      <h3>${t("keys.title")}</h3>
      <div class="modal-sub">
        ${t("keys.sub")}
      </div>

      <div class="field">
        <label>${t("keys.provider")}</label>
        <div class="segmented-control wide" id="providerTabs">
          ${settings.providers
            .map(
              (item) =>
                `<button class="segmented-btn ${item.id === p.id ? "active" : ""}" data-provider="${escapeHtml(
                  item.id,
                )}">${escapeHtml(item.label)}${item.key_count ? ` · ${item.key_count}` : ""}</button>`,
            )
            .join("")}
        </div>
      </div>

      <div class="field">
        <label>${t("keys.step1", { n: keys.length })}</label>
        <div id="keyList">
          ${
            keys.length === 0
              ? `<div class="empty-hint">${
                  isGemini
                    ? t("keys.noKeysGemini")
                    : t("keys.noKeysOpenai")
                }</div>`
              : keys
                  .map(
                    (k) => `<div class="list-row">
                      <div>
                        <div>${escapeHtml(k.masked)}</div>
                        <div class="meta">${t("keys.keyStats", { ok: k.successes, fail: k.failures })}${
                          k.cooling_seconds > 0 ? t("keys.cooldown", { n: k.cooling_seconds }) : ""
                        }${k.last_error ? ` · ${escapeHtml(k.last_error)}` : ""}</div>
                      </div>
                      <button class="btn-icon" data-remove-key="${k.index}" title="${t("keys.remove")}">✕</button>
                    </div>`,
                  )
                  .join("")
          }
        </div>
        <div class="row-inline">
          <input class="field-input" id="newKey" placeholder="${t("keys.addPlaceholder")}" autocomplete="off" />
          <button class="btn btn-primary" id="btnAddKey">${t("keys.add")}</button>
        </div>
      </div>

      <div class="field">
        <label>${t("keys.step2")}
          <span class="hint-inline" id="modelHint">${
            catalog
              ? `${t("keys.found", { n: catalog.models.length })}${
                  isGemini ? t("keys.apiVersion", { v: escapeHtml(catalog.api_version) }) : ""
                }`
              : keys.length
                ? t("keys.pressReload")
                : t("keys.addKeyFirst")
          }</span>
        </label>
        <div class="row-inline">
          ${
            catalog
              ? `<input class="field-input" id="modelSearch" placeholder="${t("keys.modelSearch")}" autocomplete="off" />`
              : `<input class="field-input" id="modelManual" value="${escapeHtml(p.model)}" placeholder="${t("keys.modelPlaceholder")}" />`
          }
          <button class="btn btn-secondary" id="btnReloadModels" ${keys.length ? "" : "disabled"}>
            ${t("keys.reload")}
          </button>
        </div>
        ${catalog ? `<div class="model-list" id="modelList">${modelOptions}</div>` : ""}
      </div>

      <div class="field">
        <label>${t("keys.folders")}
          <span class="hint-inline">${t("keys.foldersHint")}</span>
        </label>
        <div class="folder-list" id="folderList"></div>
        <button class="btn btn-secondary btn-wide" id="btnAddFolder">${t("keys.addFolder")}</button>
      </div>

      <div class="field">
        <label>${t("keys.voice")}
          <span class="hint-inline">${t("keys.voiceWhere")}</span>
        </label>
        <div class="segmented-control wide" id="speechEngine">
          <button class="segmented-btn ${isLocal ? "" : "active"}" data-engine="provider">
            ${t("keys.engineProvider")}
          </button>
          <button class="segmented-btn ${isLocal ? "active" : ""}" data-engine="local">
            ${t("keys.engineLocal")}
          </button>
        </div>
      </div>

      <div class="field" ${isLocal ? 'style="display:none"' : ""} id="cloudSpeech">
        <select class="field-input" id="speechProvider">
          <option value="">${t("keys.voiceSameProvider")}</option>
          ${settings.providers
            .map(
              (item) =>
                `<option value="${escapeHtml(item.id)}" ${
                  item.id === settings.speech_provider ? "selected" : ""
                }>${escapeHtml(item.label)}${item.key_count ? "" : t("keys.voiceNoKey")}</option>`,
            )
            .join("")}
        </select>
        <div class="row-inline">
          ${
            audioOptions
              ? `<select class="field-input" id="speechSelect">
                   <option value="">${t("keys.sameModel")}</option>${audioOptions}
                 </select>`
              : `<input class="field-input" id="speechManual" value="${escapeHtml(
                  speechProvider.transcribe_model,
                )}" placeholder="${
                  speechProvider.kind === "gemini"
                    ? t("keys.voiceHintGemini")
                    : t("keys.voiceHintOpenai")
                }" />`
          }
        </div>
        <div class="hint-inline">${t("keys.voiceHelp")}</div>
      </div>

      <div class="field" ${isLocal ? "" : 'style="display:none"'} id="localSpeech">
        <div class="hint-inline" style="margin-bottom:8px">${t("keys.localHelp")}</div>
        ${localModels
          .map(
            (m) => `<div class="list-row" data-model-row="${escapeHtml(m.id)}">
              <div>
                <div>
                  <label class="toggle">
                    <input type="radio" name="localModel" value="${escapeHtml(m.id)}"
                      ${m.id === settings.local_speech_model ? "checked" : ""}
                      ${m.installed ? "" : "disabled"} />
                    ${escapeHtml(m.label)}
                  </label>
                </div>
                <div class="meta">${escapeHtml(modelNote(m))} \u00b7 ${formatSize(m.size_bytes)}</div>
                <div class="meta" data-progress="${escapeHtml(m.id)}">${
                  m.installed ? t("keys.modelReady") : ""
                }</div>
              </div>
              <div style="display:flex;gap:6px">
                ${
                  m.installed
                    ? `<button class="btn btn-danger" data-delete-model="${escapeHtml(m.id)}">${t("common.delete")}</button>`
                    : `<button class="btn btn-secondary" data-download-model="${escapeHtml(m.id)}">${t("keys.download")}</button>`
                }
              </div>
            </div>`,
          )
          .join("")}
      </div>

      <details class="advanced">
        <summary>${t("keys.advanced")}</summary>
        <div class="field">
          <label>${t("keys.baseUrl")}</label>
          <input class="field-input" id="baseUrl" list="basePresets" value="${escapeHtml(p.base_url)}" />
          <datalist id="basePresets">
            ${BASE_URL_PRESETS.map((u) => `<option value="${u}"></option>`).join("")}
          </datalist>
        </div>
        <div class="field-grid">
          <div class="field">
            <label>${t("keys.temperature", { v: p.temperature.toFixed(2) })}</label>
            <input type="range" id="temperature" min="0" max="2" step="0.05" value="${p.temperature}" />
          </div>
          <div class="field">
            <label>${t("keys.version")}</label>
            <input class="field-input" id="apiVersion" value="${escapeHtml(p.api_version)}" />
          </div>
        </div>
        <div class="field">
          <label>${t("keys.headers")}</label>
          <textarea class="field-area" id="headers" placeholder="HTTP-Referer: https://example.com">${escapeHtml(
            p.extra_headers.map(([k, v]) => `${k}: ${v}`).join("\n"),
          )}</textarea>
        </div>
        <div class="field-grid">
          <div class="field">
            <label>${t("keys.thinkingBudget")}</label>
            <input class="field-input" id="thinkingBudget" type="number" min="-1" max="32768" step="128"
                   placeholder="${t("keys.thinkingBudgetHint")}"
                   value="${p.thinking_budget ?? ""}" />
          </div>
          <div class="field">
            <label>${t("keys.contextTokens")}</label>
            <input class="field-input" id="contextTokens" type="number" min="1024" step="1024"
                   placeholder="${t("keys.contextAuto")}" value="${p.context_tokens ?? ""}" />
          </div>
        </div>
        <div class="field-grid">
          <div class="field">
            <label>${t("keys.dialect")}</label>
            <select class="field-input" id="reasoningDialect" ${isGemini ? "disabled" : ""}>
              ${["auto", "openai", "openrouter", "qwen"]
                .map(
                  (d) =>
                    `<option value="${d}" ${p.reasoning_dialect === d ? "selected" : ""}>${
                      d === "auto" ? t("keys.dialectAuto") : d
                    }</option>`,
                )
                .join("")}
            </select>
          </div>
          <div class="field">
            <label>${t("keys.autoCompact")}</label>
            <input class="field-input" id="autoCompact" type="number" min="30" max="99"
                   value="${Math.round((settings.auto_compact_at ?? 0.85) * 100)}" />
          </div>
        </div>
        <div class="field-grid">
          <div class="field"><label>${t("keys.history")}</label>
            <input class="field-input" id="historyLimit" type="number" min="5" max="200" value="${settings.history_limit}" /></div>
          <div class="field"><label>${t("keys.turns")}</label>
            <input class="field-input" id="maxTurns" type="number" min="1" max="20" value="${settings.max_tool_turns}" /></div>
        </div>
        <div class="field">
          <label>${t("keys.rules")}</label>
          <textarea class="field-area" id="globalRules">${escapeHtml(settings.global_style_rules)}</textarea>
        </div>
      </details>

      <div class="modal-actions">
        <button class="btn btn-secondary" id="btnTest">${t("keys.test")}</button>
        <button class="btn btn-secondary" data-act="close">${t("common.close")}</button>
        <button class="btn btn-primary" id="btnSaveProvider">${t("common.save")}</button>
      </div>
    `);

    card.querySelectorAll<HTMLButtonElement>("[data-provider]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        providerId = btn.dataset.provider!;
        await persist({}, true);
        await draw();
      });
    });

    card.querySelectorAll<HTMLButtonElement>("[data-remove-key]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        try {
          await api.removeKey(p.id, Number(btn.dataset.removeKey));
          await deps.refresh();
          settings = store.settings ?? settings;
          await draw();
        } catch (error) {
          toast(errorText(error), "error");
        }
      });
    });

    const reloadModels = async (force: boolean) => {
      const hint = card.querySelector<HTMLElement>("#modelHint");
      if (hint) hint.textContent = t("keys.asking");
      try {
        const catalog = await loadModels(force);
        if (!catalog) {
          if (hint) hint.textContent = t("keys.addKeyFirst");
          return;
        }
        toast(t("toast.modelsFound", { n: catalog.models.length }), "success");
        await draw();
      } catch (error) {
        if (hint) hint.textContent = t("keys.reloadFailed");
        toast(errorText(error), "error");
      }
    };

    card
      .querySelector<HTMLButtonElement>("#btnReloadModels")
      ?.addEventListener("click", () => void reloadModels(true));

    const addKey = async () => {
      const field = card.querySelector<HTMLInputElement>("#newKey");
      const value = field?.value.trim();
      if (!value) return;
      try {
        await api.addKey(p.id, value);
        if (field) field.value = "";
        toast(t("toast.keyAdded"), "success");
        await deps.refresh();
        settings = store.settings ?? settings;
        // Fresh key may unlock a different catalogue, so always force.
        await reloadModels(true);
      } catch (error) {
        toast(errorText(error), "error");
      }
    };
    card.querySelector<HTMLButtonElement>("#btnAddKey")?.addEventListener("click", () => void addKey());
    card.querySelector<HTMLInputElement>("#newKey")?.addEventListener("keydown", (raw) => {
      if ((raw as KeyboardEvent).key === "Enter") void addKey();
    });

    card.querySelectorAll<HTMLButtonElement>("#speechEngine .segmented-btn").forEach((btn) => {
      btn.addEventListener("click", async () => {
        settings.speech_engine = btn.dataset.engine === "local" ? "local" : "provider";
        settings = await api.saveSettings(settings);
        store.settings = settings;
        await draw();
      });
    });

    card.querySelectorAll<HTMLInputElement>('input[name="localModel"]').forEach((radio) => {
      radio.addEventListener("change", async () => {
        settings.local_speech_model = radio.value;
        settings = await api.saveSettings(settings);
        store.settings = settings;
      });
    });

    card.querySelectorAll<HTMLButtonElement>("[data-download-model]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        const id = btn.dataset.downloadModel!;
        const line = card.querySelector<HTMLElement>(`[data-progress="${id}"]`);
        btn.disabled = true;
        if (line) line.textContent = t("keys.downloading");
        try {
          const model = await api.downloadLocalModel(id);
          toast(t("toast.modelReady", { name: model.label }), "success");
          // First downloaded model becomes the active one.
          if (!settings.local_speech_model) {
            settings.local_speech_model = model.id;
            settings = await api.saveSettings(settings);
            store.settings = settings;
          }
          await draw();
        } catch (error) {
          if (line) line.textContent = "";
          btn.disabled = false;
          toast(errorText(error), "error");
        }
      });
    });

    card.querySelectorAll<HTMLButtonElement>("[data-delete-model]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        try {
          await api.deleteLocalModel(btn.dataset.deleteModel!);
          if (settings.local_speech_model === btn.dataset.deleteModel) {
            settings.local_speech_model = "";
            settings = await api.saveSettings(settings);
            store.settings = settings;
          }
          unloadModel();
          await draw();
        } catch (error) {
          toast(errorText(error), "error");
        }
      });
    });

    card.querySelector<HTMLSelectElement>("#speechProvider")?.addEventListener("change", async (event) => {
      const chosen = (event.target as HTMLSelectElement).value;
      settings.speech_provider = chosen || null;
      settings = await api.saveSettings(settings);
      store.settings = settings;
      // Fetch the model list of the speech provider so its picker is usable.
      const target = settings.providers.find((item) => item.id === (chosen || providerId));
      if (target && target.key_count > 0 && !catalogs.has(target.id)) {
        try {
          catalogs.set(target.id, await api.listProviderModels(target.id));
        } catch (error) {
          toast(errorText(error), "error");
        }
      }
      await draw();
    });

    // Picking a model: the list is filtered live and the choice is remembered
    // on the element the save step reads.
    let chosenModel = p.model;
    const list = card.querySelector<HTMLElement>("#modelList");
    const search = card.querySelector<HTMLInputElement>("#modelSearch");
    const redrawModels = () => {
      if (!list || !catalog) return;
      list.innerHTML = modelRows(catalog.models, chosenModel, search?.value ?? "");
    };
    search?.addEventListener("input", redrawModels);
    list?.addEventListener("click", (event) => {
      const row = (event.target as HTMLElement).closest<HTMLElement>("[data-model]");
      if (!row?.dataset.model) return;
      chosenModel = row.dataset.model;
      redrawModels();
    });

    // Folders an agent may read and write. Revoking one takes effect on the
    // next tool call; nothing on disk is touched either way.
    const drawFolders = async () => {
      const box = card.querySelector<HTMLElement>("#folderList");
      if (!box) return;
      let roots: TrustedRoot[] = [];
      try {
        roots = await api.listTrustedRoots();
      } catch (error) {
        console.error("trusted roots", error);
      }
      box.innerHTML = roots.length
        ? roots
            .map(
              (root) =>
                `<div class="folder-row"><span class="folder-path">${escapeHtml(root.path)}</span>` +
                `<span class="tag">${root.writable ? t("keys.folderRw") : t("keys.folderRo")}</span>` +
                `<button class="btn-icon" data-revoke="${escapeHtml(root.path)}" title="${t("keys.revoke")}">✕</button></div>`,
            )
            .join("")
        : `<div class="empty-hint">${t("keys.noFolders")}</div>`;
    };
    void drawFolders();

    card.querySelector<HTMLElement>("#folderList")?.addEventListener("click", async (event) => {
      const path = (event.target as HTMLElement).closest<HTMLElement>("[data-revoke]")?.dataset
        .revoke;
      if (!path) return;
      await api.revokeFolder(path);
      await drawFolders();
    });

    card.querySelector<HTMLButtonElement>("#btnAddFolder")?.addEventListener("click", async () => {
      const { open } = await import("@tauri-apps/plugin-dialog");
      const picked = await open({ directory: true, multiple: false });
      if (typeof picked !== "string") return;
      try {
        await api.trustFolder(picked, true);
        await drawFolders();
        toast(t("keys.folderAdded"), "success");
      } catch (error) {
        toast(errorText(error), "error");
      }
    });

    const tempInput = card.querySelector<HTMLInputElement>("#temperature");
    tempInput?.addEventListener("input", () => {
      const label = card.querySelector<HTMLElement>("#tempValue");
      if (label) label.textContent = Number(tempInput.value).toFixed(2);
    });

    card.querySelector<HTMLButtonElement>("#btnTest")?.addEventListener("click", async () => {
      toast(t("toast.checking"));
      try {
        const result = await api.testProvider();
        toast(
          t("toast.probeOk", {
            text: String(result.text ?? ""),
            n: Number(result.key_index) + 1,
          }),
          "success",
        );
      } catch (error) {
        toast(errorText(error), "error");
      }
    });

    card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);

    card.querySelector<HTMLButtonElement>("#btnSaveProvider")?.addEventListener("click", async () => {
      const model =
        card.querySelector<HTMLInputElement>("#modelManual")?.value.trim() || chosenModel;
      const speech =
        card.querySelector<HTMLSelectElement>("#speechSelect")?.value ??
        card.querySelector<HTMLInputElement>("#speechManual")?.value.trim() ??
        "";
      const speechProviderId =
        card.querySelector<HTMLSelectElement>("#speechProvider")?.value ?? "";
      settings.speech_provider = speechProviderId || null;
      // The dictation model belongs to whichever provider handles speech.
      const speechTarget =
        settings.providers.find((item) => item.id === (speechProviderId || providerId)) ?? null;
      if (speechTarget) speechTarget.transcribe_model = speech;

      settings.global_style_rules =
        card.querySelector<HTMLTextAreaElement>("#globalRules")?.value ?? settings.global_style_rules;
      settings.history_limit =
        Number(card.querySelector<HTMLInputElement>("#historyLimit")?.value) || settings.history_limit;
      settings.max_tool_turns =
        Number(card.querySelector<HTMLInputElement>("#maxTurns")?.value) || settings.max_tool_turns;
      const compactPercent = Number(card.querySelector<HTMLInputElement>("#autoCompact")?.value);
      if (compactPercent >= 30 && compactPercent <= 99) {
        settings.auto_compact_at = compactPercent / 100;
      }

      const numberOrNull = (id: string): number | null => {
        const raw = card.querySelector<HTMLInputElement>(id)?.value.trim() ?? "";
        if (!raw) return null;
        const value = Number(raw);
        return Number.isFinite(value) ? value : null;
      };

      await persist({
        model,
        base_url: card.querySelector<HTMLInputElement>("#baseUrl")?.value.trim() ?? p.base_url,
        api_version: card.querySelector<HTMLInputElement>("#apiVersion")?.value.trim() ?? p.api_version,
        temperature: Number(card.querySelector<HTMLInputElement>("#temperature")?.value ?? p.temperature),
        thinking_budget: numberOrNull("#thinkingBudget"),
        context_tokens: numberOrNull("#contextTokens"),
        reasoning_dialect:
          card.querySelector<HTMLSelectElement>("#reasoningDialect")?.value ?? p.reasoning_dialect,
        extra_headers: (card.querySelector<HTMLTextAreaElement>("#headers")?.value ?? "")
          .split("\n")
          .map((line) => line.trim())
          .filter(Boolean)
          .map((line) => {
            const idx = line.indexOf(":");
            return [line.slice(0, idx).trim(), line.slice(idx + 1).trim()] as [string, string];
          })
          .filter(([k, v]) => k && v),
      });
      await deps.refresh();
      closeModal();
    });
  };

  await draw();
  // Auto-populate the model list the first time a configured provider is opened.
  if (provider()?.key_count && !catalogs.has(provider().id)) {
    try {
      await loadModels(false);
      await draw();
    } catch {
      /* offline or bad key — the operator sees the manual field */
    }
  }
}
