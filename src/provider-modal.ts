import { api, errorText, onModelEvent } from "./api";
import type { ModalDeps } from "./deps";

import { closeModal, escapeHtml, openModal, toast } from "./dom";
import { t } from "./i18n";
import { unloadModel } from "./local-whisper";
import { store } from "./store";
import type {
  KeyStatus,
  PlanState,
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
  "https://integrate.api.nvidia.com/v1",
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
        `<div class="model-row${m.id === selected ? " active" : ""}">` +
        `<button type="button" class="model-pick" data-model="${escapeHtml(m.id)}">` +
        `<span class="model-name">${escapeHtml(m.label)}</span></button>` +
        (m.free ? `<span class="tag free">${t("keys.free")}</span>` : "") +
        (m.audio ? `<span class="tag">${t("keys.audioTag")}</span>` : "") +
        `<button type="button" class="btn-icon" data-chain-add="${escapeHtml(m.id)}" title="${t("keys.chainAdd")}">+</button>` +
        `</div>`,
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

/**
 * What is left, as a percentage of what the plan allows.
 *
 * Falls back to the raw number when the gateway is older than this build and
 * does not send the ceiling — better a number than a blank.
 */
function sharePercent(left: number, cap: number | null): string {
  if (!cap || cap <= 0) return String(Math.max(0, Math.round(left)));
  return `${Math.max(0, Math.min(100, Math.round((left / cap) * 100)))}%`;
}

/** How much of a limited allowance is left, as a bar. */
function meterBar(used: number, cap: number): string {
  const share = cap > 0 ? Math.min(1, used / cap) : 0;
  const low = share > 0.8 ? " low" : "";
  return `<div class="meter${low}"><span style="width:${Math.round(share * 100)}%"></span></div>`;
}

/**
 * The subscription, drawn as what it is worth rather than as a form.
 *
 * On the free plan this is the sales pitch and the meter that makes it land:
 * the operator sees what today has cost them and what the ceiling is. With a
 * licence it is a receipt — the plan, the date, and the one field that
 * replaces it when it runs out. Either way there is nothing here to fill in
 * except a licence key, because everything else about the cloud provider —
 * models, fallbacks, voice, keys — lives on the server.
 */
function planPanel(plan: PlanState | null, hasKey: boolean): string {
  if (!plan) return `<div class="plan-card"><div class="meta">${t("keys.cloudChecking")}</div></div>`;

  const paid = plan.plan !== "free";
  const field = `
    <div class="row-inline">
      <input class="field-input" id="licenseKey" placeholder="${t("plan.keyPlaceholder")}" autocomplete="off" />
      <button class="btn btn-primary" id="btnLicense">${t(hasKey ? "plan.replace" : "plan.activate")}</button>
    </div>
    <div class="meta" id="cloudStatus">${t("keys.cloudChecking")}</div>`;

  if (!paid) {
    const cap = plan.limits.requests_per_day ?? 0;
    const left = plan.requests_left ?? 0;
    return `<div class="plan-card free">
      <div class="plan-head">
        <span class="plan-name">${t("plan.freeTitle")}</span>
        <span class="plan-badge muted">${t("plan.freeBadge")}</span>
      </div>
      <div class="meta">${t("plan.todayUsed", { used: plan.used_today, cap, left })}</div>
      ${meterBar(plan.used_today, cap)}
      <div class="meta">${t("plan.freeProfiles", {
        used: plan.profiles_used,
        cap: plan.limits.profiles ?? 0,
        men: plan.limits.men_per_profile ?? 0,
      })}</div>
      <ul class="plan-perks">
        <li>${t("plan.perkUnlimited")}</li>
        <li>${t("plan.perkNoKeys")}</li>
        <li>${t("plan.perkVoice")}</li>
        <li>${t("plan.perkSync")}</li>
      </ul>
      ${plan.problem === "license.expired" ? `<div class="meta">${t("plan.expiredData")}</div>` : ""}
      ${field}
    </div>`;
  }

  return `<div class="plan-card">
    <div class="plan-head">
      <span class="plan-name">${t("plan.paidTitle", { tier: plan.tier })}</span>
      <span class="plan-badge">${
        plan.expires_at ? t("plan.daysLeft", { n: Math.max(0, plan.days_left) }) : t("keys.cloudForever")
      }</span>
    </div>
    <div class="meta">${t("plan.paidWhat", { devices: plan.limits.devices })}</div>
    <div class="meta" id="syncStatus">${t("keys.syncChecking")}</div>
    <div class="meta" id="cloudTopUp" hidden></div>
    <div class="row-inline">
      <button class="btn btn-secondary" id="btnSyncNow">${t("keys.syncNow")}</button>
      <button class="btn btn-secondary" id="btnSyncForget">${t("keys.syncForget")}</button>
    </div>
    ${field}
  </div>`;
}

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

  /**
   * Persist the form, then reload models with the stored keys.
   *
   * Saving a provider's settings does not make it the one the app works
   * through: opening the subscription tab to paste a licence used to switch
   * the whole app onto a provider with no licence in it, and the next
   * restart came up on an empty cloud provider instead of the operator's own
   * keys. Choosing is `choose` below, and it is deliberate.
   */
  const persist = async (patch: Partial<ProviderConfig>, quiet = false) => {
    const target = settings.providers.find((p) => p.id === providerId);
    if (target) Object.assign(target, patch);
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
    let plan: PlanState | null = null;
    try {
      plan = await api.planState();
    } catch (error) {
      console.error("plan", error);
    }
    // The cloud provider is billed rather than keyed: the "key" is a
    // licence, and what matters about it is what is left on it.
    const isCloud = p.id === "velvetdesk-cloud";

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

      ${isCloud ? planPanel(plan, keys.length > 0) : ""}

      ${
        isCloud
          ? ""
          : `<div class="field">
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
        <label>${t("keys.chain")}
          <span class="hint-inline">${t("keys.chainHint")}</span>
        </label>
        <div class="chain-list" id="chainList"></div>
      </div>`
      }

      <div class="field">
        <label>${t("keys.folders")}
          <span class="hint-inline">${t("keys.foldersHint")}</span>
        </label>
        <div class="folder-list" id="folderList"></div>
        <button class="btn btn-secondary btn-wide" id="btnAddFolder">${t("keys.addFolder")}</button>
      </div>

      <div class="field">
        <label>${t("keys.voice")}
          <span class="hint-inline">${isCloud ? t("keys.voiceIncluded") : t("keys.voiceWhere")}</span>
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
        ${
          isCloud
            ? `<div class="hint-inline">${t("keys.voiceThroughCloud")}</div>`
            : `<select class="field-input" id="speechProvider">
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
        <div class="hint-inline">${t("keys.voiceHelp")}</div>`
        }
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
        ${
          isCloud
            ? ""
            : `<div class="field">
          <label>${t("keys.baseUrl")}</label>
          <input class="field-input" id="baseUrl" list="basePresets" value="${escapeHtml(p.base_url)}" />
          <datalist id="basePresets">
            ${BASE_URL_PRESETS.map((u) => `<option value="${u}"></option>`).join("")}
          </datalist>
        </div>`
        }
        <div class="field-grid">
          <div class="field">
            <label>${t("keys.temperature", { v: p.temperature.toFixed(2) })}</label>
            <input type="range" id="temperature" min="0" max="2" step="0.05" value="${p.temperature}" />
          </div>
          ${
            isCloud
              ? ""
              : `<div class="field">
            <label>${t("keys.version")}</label>
            <input class="field-input" id="apiVersion" value="${escapeHtml(p.api_version)}" />
          </div>`
          }
        </div>
        ${
          isCloud
            ? ""
            : `<div class="field">
          <label>${t("keys.headers")}</label>
          <textarea class="field-area" id="headers" placeholder="HTTP-Referer: https://example.com">${escapeHtml(
            p.extra_headers.map(([k, v]) => `${k}: ${v}`).join("\n"),
          )}</textarea>
        </div>`
        }
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
          <div class="field">
            <label>${t("keys.maxOutput")}</label>
            <input class="field-input" id="maxOutput" type="number" min="256" step="256"
                   placeholder="${t("keys.maxOutputHint")}" value="${p.max_output_tokens ?? ""}" />
          </div>
        </div>
        <div class="field-grid">
          <div class="field">
            <label>${t("keys.dialect")}</label>
            <select class="field-input" id="reasoningDialect" ${isGemini ? "disabled" : ""}>
              ${["auto", "openai", "openrouter", "qwen", "nvidia"]
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
        <div class="field">
          <label class="toggle wide">
            <input type="checkbox" id="updateCheck"${settings.update_check ? " checked" : ""} />
            <span>${t("keys.updateCheck")}</span>
          </label>
          <button class="btn btn-secondary" id="btnCheckUpdate" type="button">${t("keys.updateNow")}</button>
        </div>
      </details>

      <div class="modal-actions">
        <button class="btn btn-secondary" id="btnTest">${t("keys.test")}</button>
        <button class="btn btn-secondary" data-act="close">${t("common.close")}</button>
        <button class="btn btn-primary" id="btnSaveProvider">${t("common.save")}</button>
      </div>
    `);

    // The licence is stored where every other provider's key is stored, so
    // nothing in the app has to learn a second way of keeping a secret.
    card.querySelector("#btnLicense")?.addEventListener("click", async () => {
      const input = card.querySelector<HTMLInputElement>("#licenseKey");
      const button = card.querySelector<HTMLButtonElement>("#btnLicense");
      const token = input?.value.trim() ?? "";
      if (!token) return;
      if (button) button.disabled = true;
      try {
        // The key is checked before it is stored — by its signature, or by
        // the gateway when this build carries no key to check against — so
        // "accepted" means the subscription is actually on.
        const plan = await api.activateLicense(token);
        // A licence that works is the operator saying they want the
        // subscription; anything else would leave them wondering why the
        // app still talks to their old keys.
        if (plan.plan !== "free") settings.active_provider = p.id;
        await persist({}, true);
        catalogs.delete(p.id);
        await deps.refresh();
        toast(
          plan.plan === "free" ? t("plan.stillFree") : t("plan.activatedAs", { tier: plan.tier }),
          plan.plan === "free" ? "error" : "success",
        );
        await draw();
      } catch (error) {
        toast(errorText(error), "error");
      } finally {
        if (button) button.disabled = false;
      }
    });

    /** Licence and credits, once the gateway has answered — or said nothing. */
    async function showCloudStatus() {
      const line = card.querySelector<HTMLElement>("#cloudStatus");
      if (!line) return;
      try {
        const status = await api.cloudStatus();
        const parts: string[] = [];
        if (status.problem) {
          // The backend names the problem; the wording of it lives here, in
          // whichever language the app is running.
          const wording: Record<string, string> = {
            "license.missing": "keys.cloudMissing",
            "license.expired": "keys.cloudExpired",
            "license.invalid": "keys.cloudInvalid",
            "license.refused": "keys.cloudRefused",
          };
          const [name, ...rest] = status.problem.split(":");
          const said = rest.join(":").trim();
          // A refusal the gateway explained is shown as it explained it —
          // "all ten devices are registered" is actionable, "refused" is not.
          parts.push(said || t(wording[name] ?? "keys.cloudInvalid"));
        } else if (status.valid) {
          parts.push(
            t("keys.cloudValid", {
              tier: status.tier,
              date: status.expires_at
                ? new Date(status.expires_at * 1000).toLocaleDateString()
                : t("keys.cloudForever"),
            }),
          );
        }
        if (status.credits_left_5h !== null && status.credits_left_week !== null) {
          // A share, not a count: "5988 credits" places nobody, "100%" does.
          parts.push(
            t("keys.cloudCredits", {
              n5: sharePercent(status.credits_left_5h, status.credits_5h),
              nw: sharePercent(status.credits_left_week, status.credits_week),
            }),
          );
        } else if (!status.problem) {
          parts.push(t("keys.cloudOffline"));
        }
        if ((status.credits_extra ?? 0) > 0) {
          parts.push(t("keys.cloudExtra", { n: Math.round(status.credits_extra!) }));
        }
        // The way to more credits is shown before anybody hits the wall, and
        // only where it exists: on pro the answer is the bigger plan, not a
        // top-up, and saying so is more use than a button that refuses.
        const more = card.querySelector<HTMLElement>("#cloudTopUp");
        if (more) {
          more.hidden = false;
          more.innerHTML = status.can_top_up
            ? `<a href="${escapeHtml(status.topup_url)}" target="_blank" rel="noreferrer">${t(
                "plan.buyCredits",
              )}</a>`
            : t("plan.buyOnBusiness");
        }
        line.textContent = parts.join(" · ");
      } catch (error) {
        line.textContent = errorText(error);
      }
    }
    /** Pairing and the last round, as a line under the licence. */
    async function showSyncState() {
      const line = card.querySelector<HTMLElement>("#syncStatus");
      if (!line) return;
      try {
        const state = await api.syncState();
        const parts: string[] = [];
        if (!state.allowed) {
          parts.push(t("keys.syncPaidOnly"));
        } else if (state.from_license) {
          // Nothing was typed in and nothing needs to be: the second machine
          // joins by being given the same licence.
          parts.push(t("keys.syncByLicense", { n: state.devices }));
        } else {
          parts.push(state.paired ? t("keys.syncPaired") : t("keys.syncUnpaired"));
        }
        if (state.last?.finished_at) {
          parts.push(
            t("keys.syncLast", {
              date: new Date(state.last.finished_at).toLocaleString(),
              inn: state.last.pulled,
              out: state.last.pushed,
            }),
          );
          if (state.last.conflicts > 0) {
            // A conflict is not a failure, but it is the one thing worth
            // going and looking at: the losing copy is in backups/.
            parts.push(t("keys.syncConflicts", { n: state.last.conflicts }));
          }
        }
        line.textContent = parts.join(" · ");
      } catch (error) {
        line.textContent = errorText(error);
      }
    }

    card.querySelector("#btnSyncNow")?.addEventListener("click", async () => {
      try {
        const report = await api.syncNow();
        toast(
          t("keys.syncDone", { inn: report.pulled, out: report.pushed }),
          report.conflicts > 0 ? "info" : "success",
        );
        await showSyncState();
        await deps.refresh();
      } catch (error) {
        toast(errorText(error), "error");
      }
    });

    card.querySelector("#btnSyncForget")?.addEventListener("click", async () => {
      try {
        await api.syncForget();
        toast(t("keys.syncForgotten"), "success");
        await showSyncState();
      } catch (error) {
        toast(errorText(error), "error");
      }
    });

    if (isCloud) {
      void showCloudStatus();
      void showSyncState();
    }

    card.querySelectorAll<HTMLButtonElement>("[data-provider]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        providerId = btn.dataset.provider!;
        // A provider that has nothing to work with — no keys, no licence —
        // is shown but not switched to. Pasting one makes it the choice.
        const target = settings.providers.find((item) => item.id === providerId);
        if (target && target.key_count > 0) settings.active_provider = providerId;
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

    // The chain: which model answers when the one above will not.
    let chain = [...(p.model_chain ?? [])];
    const chainBox = card.querySelector<HTMLElement>("#chainList");
    const drawChain = () => {
      if (!chainBox) return;
      chainBox.innerHTML = chain.length
        ? chain
            .map(
              (id, index) =>
                `<div class="chain-row"><span class="chain-order">${index + 2}</span>` +
                `<span class="model-name">${escapeHtml(id)}</span>` +
                `<button class="btn-icon" data-chain-up="${index}" title="${t("keys.chainUp")}">↑</button>` +
                `<button class="btn-icon" data-chain-out="${index}" title="${t("keys.chainRemove")}">✕</button></div>`,
            )
            .join("")
        : `<div class="empty-hint">${t("keys.chainEmpty")}</div>`;
    };
    drawChain();

    chainBox?.addEventListener("click", (event) => {
      const target = event.target as HTMLElement;
      const up = target.closest<HTMLElement>("[data-chain-up]")?.dataset.chainUp;
      const out = target.closest<HTMLElement>("[data-chain-out]")?.dataset.chainOut;
      if (up !== undefined) {
        const index = Number(up);
        if (index > 0) [chain[index - 1], chain[index]] = [chain[index], chain[index - 1]];
      } else if (out !== undefined) {
        chain.splice(Number(out), 1);
      } else {
        return;
      }
      drawChain();
      // Reordering the fallback chain is an edit too.
      later();
    });

    list?.addEventListener("click", (event) => {
      const target = event.target as HTMLElement;
      const add = target.closest<HTMLElement>("[data-chain-add]")?.dataset.chainAdd;
      if (add) {
        if (!chain.includes(add) && add !== chosenModel) chain.push(add);
        drawChain();
        later();
        return;
      }
      const row = target.closest<HTMLElement>("[data-model]");
      if (!row?.dataset.model) return;
      chosenModel = row.dataset.model;
      chain = chain.filter((id) => id !== chosenModel);
      applyPublishedLimits(chosenModel);
      drawChain();
      redrawModels();
      // Choosing a model is an edit like any other: it saves itself.
      later();
    });

    /**
     * Fill the limits the provider publishes for the model just picked.
     *
     * Gemini lists an input and an output ceiling per model, OpenRouter lists
     * a context length and a completion ceiling; a plain OpenAI endpoint lists
     * neither. Nobody knows these numbers by heart, and the wrong ones are how
     * an answer ends mid-sentence — so picking a model fills them in, and a
     * number typed by hand is left exactly as it was typed.
     */
    const applyPublishedLimits = (modelId: string) => {
      const known = catalogs.get(provider().id)?.models.find((m) => m.id === modelId);
      if (!known) return;
      const context = card.querySelector<HTMLInputElement>("#contextTokens");
      const output = card.querySelector<HTMLInputElement>("#maxOutput");
      if (context && known.context_tokens && !context.value.trim()) {
        context.value = String(known.context_tokens);
      }
      if (output && known.max_output_tokens && !output.value.trim()) {
        output.value = String(known.max_output_tokens);
      }
    };

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

    card.querySelector<HTMLButtonElement>("#btnCheckUpdate")?.addEventListener("click", () => {
    void deps.checkUpdate();
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

    /**
     * Write the form down.
     *
     * Every field saves itself as it is changed — a setting the operator typed
     * and then closed the dialog on used to be lost — so this runs both from
     * the Save button and, quietly, a moment after any edit.
     */
    const apply = async (quiet: boolean) => {
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

      settings.update_check =
        card.querySelector<HTMLInputElement>("#updateCheck")?.checked ?? settings.update_check;
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

      await persist(
        {
        model,
        base_url: card.querySelector<HTMLInputElement>("#baseUrl")?.value.trim() ?? p.base_url,
        api_version: card.querySelector<HTMLInputElement>("#apiVersion")?.value.trim() ?? p.api_version,
        temperature: Number(card.querySelector<HTMLInputElement>("#temperature")?.value ?? p.temperature),
        model_chain: chain,
        thinking_budget: numberOrNull("#thinkingBudget"),
        context_tokens: numberOrNull("#contextTokens"),
        max_output_tokens: numberOrNull("#maxOutput"),
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
        },
        quiet,
      );
      await deps.refresh();
    };

    card.querySelector<HTMLButtonElement>("#btnSaveProvider")?.addEventListener("click", () => {
      void apply(false).then(closeModal);
    });

    // Anything touched in the dialog is saved on its own; typing settles first
    // so a half-written number is not stored on every keystroke.
    let pending = 0;
    const later = () => {
      window.clearTimeout(pending);
      pending = window.setTimeout(() => void apply(true), 600);
    };
    card.addEventListener("change", (event) => {
      if ((event.target as HTMLElement).closest("input, select, textarea")) later();
    });
    card.addEventListener("input", (event) => {
      if ((event.target as HTMLElement).closest("input, textarea")) later();
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
