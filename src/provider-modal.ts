import { api, errorText } from "./api";
import type { ModalDeps } from "./deps";
import { closeModal, escapeHtml, openModal, toast } from "./dom";
import { t } from "./i18n";
import { store } from "./store";
import type { KeyStatus, ModelCatalog, ProviderConfig, Settings } from "./types";

/** Cached per provider so re-opening the dialog does not re-hit the API. */
const catalogs = new Map<string, ModelCatalog>();

const BASE_URL_PRESETS = [
  "https://openrouter.ai/api/v1",
  "https://api.openai.com/v1",
  "https://api.deepseek.com/v1",
  "https://api.groq.com/openai/v1",
  "https://api.together.xyz/v1",
  "http://localhost:11434/v1",
  "http://localhost:1234/v1",
];

export async function openKeysModal(deps: ModalDeps) {
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
    const catalog = catalogs.get(p.id) ?? null;
    const isGemini = p.kind === "gemini";

    const modelOptions = catalog
      ? catalog.models
          .map(
            (m) =>
              `<option value="${escapeHtml(m.id)}" ${m.id === p.model ? "selected" : ""}>${escapeHtml(
                m.label,
              )}</option>`,
          )
          .join("")
      : "";

    const audioOptions = catalog
      ? catalog.models
          .filter((m) => m.audio)
          .map(
            (m) =>
              `<option value="${escapeHtml(m.id)}" ${
                m.id === p.transcribe_model ? "selected" : ""
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
              ? `<select class="field-input" id="modelSelect">${modelOptions}</select>`
              : `<input class="field-input" id="modelManual" value="${escapeHtml(p.model)}" placeholder="${t("keys.modelPlaceholder")}" />`
          }
          <button class="btn btn-secondary" id="btnReloadModels" ${keys.length ? "" : "disabled"}>
            ${t("keys.reload")}
          </button>
        </div>
      </div>

      <div class="field">
        <label>${t("keys.voice")}</label>
        ${
          audioOptions
            ? `<select class="field-input" id="speechSelect">
                 <option value="">${t("keys.sameModel")}</option>${audioOptions}
               </select>`
            : `<input class="field-input" id="speechManual" value="${escapeHtml(
                p.transcribe_model,
              )}" placeholder="${
                isGemini ? t("keys.voiceHintGemini") : t("keys.voiceHintOpenai")
              }" />`
        }
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

    card.querySelector<HTMLSelectElement>("#modelSelect")?.addEventListener("change", (event) => {
      void persist({ model: (event.target as HTMLSelectElement).value }, true);
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
        card.querySelector<HTMLSelectElement>("#modelSelect")?.value ??
        card.querySelector<HTMLInputElement>("#modelManual")?.value.trim() ??
        p.model;
      const speech =
        card.querySelector<HTMLSelectElement>("#speechSelect")?.value ??
        card.querySelector<HTMLInputElement>("#speechManual")?.value.trim() ??
        "";

      settings.global_style_rules =
        card.querySelector<HTMLTextAreaElement>("#globalRules")?.value ?? settings.global_style_rules;
      settings.history_limit =
        Number(card.querySelector<HTMLInputElement>("#historyLimit")?.value) || settings.history_limit;
      settings.max_tool_turns =
        Number(card.querySelector<HTMLInputElement>("#maxTurns")?.value) || settings.max_tool_turns;

      await persist({
        model,
        transcribe_model: speech,
        base_url: card.querySelector<HTMLInputElement>("#baseUrl")?.value.trim() ?? p.base_url,
        api_version: card.querySelector<HTMLInputElement>("#apiVersion")?.value.trim() ?? p.api_version,
        temperature: Number(card.querySelector<HTMLInputElement>("#temperature")?.value ?? p.temperature),
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
