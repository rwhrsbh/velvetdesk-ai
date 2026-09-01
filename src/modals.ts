import { api, errorText } from "./api";
import { $, closeModal, confirmDialog, escapeHtml, formatDate, openModal, toast } from "./dom";
import { activeMan, activeProfile, store } from "./store";
import type { DoctorReport, KeyStatus, Man, PendingAction, Profile } from "./types";

export interface ModalDeps {
  /** reload profiles + men + settings + pending and re-render */
  refresh: () => Promise<void>;
  selectProfile: (modelId: string) => Promise<void>;
  selectMan: (manId: string | null) => Promise<void>;
}

// ---------------------------------------------------------------------------
// Keys & providers
// ---------------------------------------------------------------------------

export async function openKeysModal(deps: ModalDeps) {
  const settings = store.settings;
  if (!settings) return;
  let providerId = settings.active_provider ?? settings.providers[0]?.id ?? "";

  const draw = async () => {
    const provider = settings.providers.find((p) => p.id === providerId) ?? settings.providers[0];
    if (!provider) return;
    let keys: KeyStatus[] = [];
    try {
      keys = await api.listKeys(provider.id);
    } catch (error) {
      toast(errorText(error), "error");
    }

    const card = openModal(`
      <h3>🔑 Провайдеры и ротация ключей</h3>
      <div class="modal-sub">
        Ключи лежат только на этом устройстве (<code>secrets.json</code> рядом с данными).
        Пул перебирает ключи по кругу; 429 / 403 / 5xx уводят ключ в кулдаун с backoff 1→2→4с.
      </div>

      <div class="field">
        <label>Активный провайдер</label>
        <select class="field-input" id="providerSelect">
          ${settings.providers
            .map(
              (p) =>
                `<option value="${escapeHtml(p.id)}" ${p.id === provider.id ? "selected" : ""}>${escapeHtml(
                  p.label,
                )} (${escapeHtml(p.kind)})</option>`,
            )
            .join("")}
        </select>
      </div>

      <div class="field-grid">
        <div class="field"><label>Base URL</label>
          <input class="field-input" id="baseUrl" value="${escapeHtml(provider.base_url)}" /></div>
        <div class="field"><label>Модель</label>
          <input class="field-input" id="modelName" value="${escapeHtml(provider.model)}" /></div>
        <div class="field"><label>API version (Gemini)</label>
          <input class="field-input" id="apiVersion" value="${escapeHtml(provider.api_version)}" /></div>
        <div class="field"><label>Temperature</label>
          <input class="field-input" id="temperature" type="number" step="0.05" min="0" max="2" value="${provider.temperature}" /></div>
      </div>

      <div class="field">
        <label>Доп. заголовки (по одному в строке, <code>Key: Value</code>)</label>
        <textarea class="field-area" id="headers" placeholder="HTTP-Referer: https://example.com">${escapeHtml(
          provider.extra_headers.map(([k, v]) => `${k}: ${v}`).join("\n"),
        )}</textarea>
      </div>

      <div class="field">
        <label>Пул ключей (${keys.length})</label>
        <div id="keyList">
          ${
            keys.length === 0
              ? `<div class="empty-hint">Ключей нет — агент не сможет ходить в API.</div>`
              : keys
                  .map(
                    (k) => `<div class="list-row">
                      <div>
                        <div>${escapeHtml(k.masked)}</div>
                        <div class="meta">ok ${k.successes} · fail ${k.failures} ${
                          k.cooling_seconds > 0 ? `· кулдаун ${k.cooling_seconds}s` : ""
                        }${k.last_error ? ` · ${escapeHtml(k.last_error)}` : ""}</div>
                      </div>
                      <button class="btn-icon" data-remove-key="${k.index}">✕</button>
                    </div>`,
                  )
                  .join("")
          }
        </div>
        <div style="display:flex;gap:8px;margin-top:8px">
          <input class="field-input" id="newKey" placeholder="Вставь API-ключ" />
          <button class="btn-send" id="btnAddKey">Добавить</button>
        </div>
      </div>

      <div class="field">
        <label>Общие правила стиля (добавляются в каждый системный промпт)</label>
        <textarea class="field-area" id="globalRules">${escapeHtml(settings.global_style_rules)}</textarea>
      </div>

      <div class="field-grid">
        <div class="field"><label>Глубина истории</label>
          <input class="field-input" id="historyLimit" type="number" min="5" max="200" value="${settings.history_limit}" /></div>
        <div class="field"><label>Макс. циклов инструментов (AUTO)</label>
          <input class="field-input" id="maxTurns" type="number" min="1" max="20" value="${settings.max_tool_turns}" /></div>
      </div>

      <div class="modal-actions">
        <button class="btn-send secondary" id="btnTest">Проверить связь</button>
        <button class="btn-send secondary" data-act="close">Закрыть</button>
        <button class="btn-send" id="btnSaveProvider">Сохранить</button>
      </div>
    `);

    card.querySelector<HTMLSelectElement>("#providerSelect")?.addEventListener("change", (event) => {
      providerId = (event.target as HTMLSelectElement).value;
      void draw();
    });

    card.querySelectorAll<HTMLButtonElement>("[data-remove-key]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        try {
          await api.removeKey(provider.id, Number(btn.dataset.removeKey));
          await deps.refresh();
          await draw();
        } catch (error) {
          toast(errorText(error), "error");
        }
      });
    });

    const addKey = async () => {
      const field = card.querySelector<HTMLInputElement>("#newKey");
      const value = field?.value.trim();
      if (!value) return;
      try {
        await api.addKey(provider.id, value);
        toast("Ключ добавлен", "success");
        await deps.refresh();
        await draw();
      } catch (error) {
        toast(errorText(error), "error");
      }
    };
    card.querySelector<HTMLButtonElement>("#btnAddKey")?.addEventListener("click", addKey);
    card.querySelector<HTMLInputElement>("#newKey")?.addEventListener("keydown", (event) => {
      if (event.key === "Enter") void addKey();
    });

    card.querySelector<HTMLButtonElement>("#btnTest")?.addEventListener("click", async () => {
      toast("Проверяю связь…");
      try {
        const result = await api.testProvider();
        toast(`Ответ: ${String(result.text ?? "")} (ключ #${Number(result.key_index) + 1})`, "success");
      } catch (error) {
        toast(errorText(error), "error");
      }
    });

    card.querySelector<HTMLButtonElement>("#btnSaveProvider")?.addEventListener("click", async () => {
      const next = structuredClone(settings);
      const target = next.providers.find((p) => p.id === provider.id);
      if (target) {
        target.base_url = card.querySelector<HTMLInputElement>("#baseUrl")!.value.trim();
        target.model = card.querySelector<HTMLInputElement>("#modelName")!.value.trim();
        target.api_version = card.querySelector<HTMLInputElement>("#apiVersion")!.value.trim();
        target.temperature = Number(card.querySelector<HTMLInputElement>("#temperature")!.value);
        target.extra_headers = card
          .querySelector<HTMLTextAreaElement>("#headers")!
          .value.split("\n")
          .map((line) => line.trim())
          .filter(Boolean)
          .map((line) => {
            const idx = line.indexOf(":");
            return [line.slice(0, idx).trim(), line.slice(idx + 1).trim()] as [string, string];
          })
          .filter(([k, v]) => k && v);
      }
      next.active_provider = provider.id;
      next.global_style_rules = card.querySelector<HTMLTextAreaElement>("#globalRules")!.value;
      next.history_limit = Number(card.querySelector<HTMLInputElement>("#historyLimit")!.value);
      next.max_tool_turns = Number(card.querySelector<HTMLInputElement>("#maxTurns")!.value);
      try {
        await api.saveSettings(next);
        toast("Настройки сохранены", "success");
        await deps.refresh();
        closeModal();
      } catch (error) {
        toast(errorText(error), "error");
      }
    });

    card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);
  };

  await draw();
}

// ---------------------------------------------------------------------------
// Doctor
// ---------------------------------------------------------------------------

function doctorHtml(report: DoctorReport): string {
  return `
    <div class="modal-sub">
      Проверено: моделей ${report.models_checked} · досье ${report.men_checked} · переписок ${report.chats_checked}
      ${report.fixes_applied ? ` · исправлено ${report.fixes_applied}` : ""}
    </div>
    <div class="code-block">
      ${report.issues
        .map(
          (issue) => `<div class="doctor-line">
            <span class="lvl ${issue.level}">${issue.level.toUpperCase()}</span>
            <span>${escapeHtml(issue.message)}${issue.fixed ? " — исправлено" : ""}<br />
              <span style="color:var(--text-tertiary)">${escapeHtml(issue.path)}</span></span>
          </div>`,
        )
        .join("")}
    </div>`;
}

export async function openDoctorModal(deps: ModalDeps) {
  const card = openModal(`<h3>🩺 Доктор</h3><div class="modal-sub">Сканирую…</div>`);
  try {
    const report = await api.doctorScan();
    card.innerHTML = `
      <h3>🩺 Доктор — схемы и целостность</h3>
      ${doctorHtml(report)}
      <div class="modal-actions">
        <button class="btn-send secondary" data-act="close">Закрыть</button>
        <button class="btn-send" id="btnFix">Запустить авто-исправление</button>
      </div>`;
    card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);
    card.querySelector<HTMLButtonElement>("#btnFix")?.addEventListener("click", async () => {
      try {
        const fixed = await api.doctorFix();
        card.innerHTML = `<h3>🩺 Доктор — исправление</h3>${doctorHtml(fixed)}
          <div class="modal-actions"><button class="btn-send" data-act="close">Готово</button></div>`;
        card
          .querySelector<HTMLButtonElement>('[data-act="close"]')
          ?.addEventListener("click", closeModal);
        await deps.refresh();
        toast(`Исправлено записей: ${fixed.fixes_applied}`, "success");
      } catch (error) {
        toast(errorText(error), "error");
      }
    });
  } catch (error) {
    card.innerHTML = `<h3>🩺 Доктор</h3><div class="modal-sub">${escapeHtml(errorText(error))}</div>
      <div class="modal-actions"><button class="btn-send" data-act="close">Закрыть</button></div>`;
    card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);
  }
}

// ---------------------------------------------------------------------------
// Master agent
// ---------------------------------------------------------------------------

export async function openMasterModal(deps: ModalDeps) {
  const card = openModal(`
    <h3>🌐 Мастер-агент</h3>
    <div class="modal-sub">
      Глобальный поиск по всем профилям и авто-маршрутизация сырого текста:
      мастер определит, чьё это письмо, найдёт досье и создаст новое, если нужно.
    </div>
    <div class="field">
      <label>Сырой текст / запрос</label>
      <textarea class="field-area" id="masterInput" placeholder="Вставь письмо, кусок анкеты или просто имя…"></textarea>
    </div>
    <label class="mini-pill checkbox" style="margin-bottom:10px">
      <input type="checkbox" id="autoCreate" checked /> создавать досье, если совпадений нет
    </label>
    <div id="masterResult"></div>
    <div class="modal-actions">
      <button class="btn-send secondary" id="btnSearchOnly">Только поиск</button>
      <button class="btn-send secondary" data-act="close">Закрыть</button>
      <button class="btn-send" id="btnRoute">Маршрутизировать</button>
    </div>`);

  const result = card.querySelector<HTMLDivElement>("#masterResult")!;
  const input = card.querySelector<HTMLTextAreaElement>("#masterInput")!;
  input.focus();

  card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);

  card.querySelector<HTMLButtonElement>("#btnSearchOnly")?.addEventListener("click", async () => {
    const query = input.value.trim();
    if (!query) return;
    try {
      const hits = await api.globalSearch(query);
      result.innerHTML = hits.length
        ? hits
            .map(
              (hit) => `<div class="list-row">
                <div>
                  <div>${escapeHtml(hit.man_name)} · ${escapeHtml(hit.model_name)}</div>
                  <div class="meta">${escapeHtml(hit.snippet.slice(0, 120))}</div>
                </div>
                <button class="btn-send secondary" data-open="${escapeHtml(hit.model_id)}|${escapeHtml(
                  hit.man_id,
                )}">Открыть</button>
              </div>`,
            )
            .join("")
        : `<div class="empty-hint">Ничего не найдено.</div>`;
      bindOpenButtons(result, deps);
    } catch (error) {
      toast(errorText(error), "error");
    }
  });

  card.querySelector<HTMLButtonElement>("#btnRoute")?.addEventListener("click", async () => {
    const raw = input.value.trim();
    if (!raw) return;
    result.innerHTML = `<div class="empty-hint">Мастер думает…</div>`;
    try {
      const decision = await api.masterRoute(
        raw,
        card.querySelector<HTMLInputElement>("#autoCreate")!.checked,
      );
      const steps = decision.steps
        .map((s) => `<div class="meta">• ${escapeHtml(s.summary)}</div>`)
        .join("");
      result.innerHTML = `<div class="list-row">
          <div>
            <div>${escapeHtml(decision.reason || "решение принято")}</div>
            <div class="meta">модель: ${escapeHtml(decision.model_id ?? "—")} · досье: ${escapeHtml(
              decision.man_id ?? "—",
            )} · уверенность ${(decision.confidence * 100).toFixed(0)}%</div>
            ${steps}
          </div>
          ${
            decision.model_id
              ? `<button class="btn-send" data-open="${escapeHtml(decision.model_id)}|${escapeHtml(
                  decision.man_id ?? "",
                )}">Перейти</button>`
              : ""
          }
        </div>`;
      bindOpenButtons(result, deps);
      await deps.refresh();
    } catch (error) {
      result.innerHTML = `<div class="empty-hint">${escapeHtml(errorText(error))}</div>`;
    }
  });
}

function bindOpenButtons(container: HTMLElement, deps: ModalDeps) {
  container.querySelectorAll<HTMLButtonElement>("[data-open]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const [modelId, manId] = (btn.dataset.open ?? "").split("|");
      closeModal();
      if (modelId) await deps.selectProfile(modelId);
      if (manId) await deps.selectMan(manId);
    });
  });
}

// ---------------------------------------------------------------------------
// Pending approvals
// ---------------------------------------------------------------------------

function diffBlock(action: PendingAction): string {
  return `<div class="diff-grid">
    <div class="code-block before">${escapeHtml(JSON.stringify(action.before, null, 2) ?? "null")}</div>
    <div class="code-block after">${escapeHtml(JSON.stringify(action.after, null, 2) ?? "null")}</div>
  </div>`;
}

export async function openPendingModal(deps: ModalDeps) {
  const draw = async () => {
    const pending = await api.pendingList();
    store.pending = pending;
    const card = openModal(`
      <h3>🛡 Очередь подтверждений (${pending.length})</h3>
      <div class="modal-sub">Каждая запись показывает «до / после». Ничего не пишется на диск без твоего «применить».</div>
      ${
        pending.length === 0
          ? `<div class="empty-hint">Очередь пуста.</div>`
          : pending
              .map(
                (action) => `<div class="list-row" style="flex-direction:column;align-items:stretch">
                  <div style="display:flex;justify-content:space-between;gap:10px;align-items:center">
                    <div>
                      <div>${escapeHtml(action.summary)}</div>
                      <div class="meta">${escapeHtml(action.tool)} · ${escapeHtml(action.risk)} · ${escapeHtml(
                        action.model_id,
                      )} · ${formatDate(action.created_at)}</div>
                    </div>
                    <div style="display:flex;gap:6px">
                      <button class="btn-send secondary" data-reject="${escapeHtml(action.id)}">Отклонить</button>
                      <button class="btn-send" data-approve="${escapeHtml(action.id)}">Применить</button>
                    </div>
                  </div>
                  ${diffBlock(action)}
                </div>`,
              )
              .join("")
      }
      <div class="modal-actions">
        ${pending.length ? `<button class="btn-send secondary" id="btnClearAll">Очистить всё</button>` : ""}
        <button class="btn-send" data-act="close">Закрыть</button>
      </div>`);

    card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);
    card.querySelector<HTMLButtonElement>("#btnClearAll")?.addEventListener("click", async () => {
      await api.pendingClear();
      await deps.refresh();
      await draw();
    });

    card.querySelectorAll<HTMLButtonElement>("[data-approve]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        try {
          await api.pendingApprove(btn.dataset.approve!);
          toast("Применено", "success");
          await deps.refresh();
          await draw();
        } catch (error) {
          toast(errorText(error), "error");
        }
      });
    });

    card.querySelectorAll<HTMLButtonElement>("[data-reject]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        await api.pendingReject(btn.dataset.reject!);
        await deps.refresh();
        await draw();
      });
    });
  };

  await draw();
}

// ---------------------------------------------------------------------------
// Profile editor
// ---------------------------------------------------------------------------

export async function openProfileEditor(deps: ModalDeps) {
  const profile = activeProfile();
  if (!profile) {
    toast("Сначала выбери модель", "error");
    return;
  }
  const card = openModal(`
    <h3>Персона — ${escapeHtml(profile.name)}</h3>
    <div class="modal-sub">Всё, что здесь написано, уходит в системный промпт агента для этого профиля.</div>
    <div class="field-grid">
      <div class="field"><label>Имя</label><input class="field-input" id="pName" value="${escapeHtml(
        profile.name,
      )}" /></div>
      <div class="field"><label>Возраст</label><input class="field-input" id="pAge" type="number" value="${
        profile.age ?? ""
      }" /></div>
      <div class="field"><label>Сайт</label><input class="field-input" id="pSite" value="${escapeHtml(
        profile.site,
      )}" /></div>
      <div class="field"><label>Аватар (URL)</label><input class="field-input" id="pAvatar" value="${escapeHtml(
        profile.avatar,
      )}" /></div>
    </div>
    <div class="field"><label>Био</label><textarea class="field-area" id="pBio">${escapeHtml(
      profile.bio,
    )}</textarea></div>
    <div class="field"><label>Инструкции персоны</label><textarea class="field-area" id="pPrompt">${escapeHtml(
      profile.system_prompt_override,
    )}</textarea></div>
    <div class="field"><label>Правила тона (по строке)</label><textarea class="field-area" id="pTone">${escapeHtml(
      profile.tone_rules.join("\n"),
    )}</textarea></div>
    <div class="field"><label>Запрещённые фразы (по строке)</label><textarea class="field-area" id="pBanned">${escapeHtml(
      profile.banned_phrases.join("\n"),
    )}</textarea></div>
    <div class="field"><label>Языки (через запятую)</label><input class="field-input" id="pLangs" value="${escapeHtml(
      profile.languages.join(", "),
    )}" /></div>
    <div class="field"><label>Факты профиля</label>
      <div class="code-block">${
        profile.facts.length
          ? profile.facts.map((f) => `${escapeHtml(f.key)}: ${escapeHtml(f.value)}`).join("\n")
          : "пусто"
      }</div>
    </div>
    <div class="modal-actions">
      <button class="btn-send danger" id="btnDeleteProfile">Удалить профиль</button>
      <button class="btn-send secondary" data-act="close">Отмена</button>
      <button class="btn-send" id="btnSaveProfile">Сохранить</button>
    </div>`);

  card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);

  card.querySelector<HTMLButtonElement>("#btnSaveProfile")?.addEventListener("click", async () => {
    const next: Profile = {
      ...profile,
      name: card.querySelector<HTMLInputElement>("#pName")!.value.trim() || profile.name,
      age: Number(card.querySelector<HTMLInputElement>("#pAge")!.value) || null,
      site: card.querySelector<HTMLInputElement>("#pSite")!.value.trim(),
      avatar: card.querySelector<HTMLInputElement>("#pAvatar")!.value.trim(),
      bio: card.querySelector<HTMLTextAreaElement>("#pBio")!.value,
      system_prompt_override: card.querySelector<HTMLTextAreaElement>("#pPrompt")!.value,
      tone_rules: splitLines(card.querySelector<HTMLTextAreaElement>("#pTone")!.value),
      banned_phrases: splitLines(card.querySelector<HTMLTextAreaElement>("#pBanned")!.value),
      languages: card
        .querySelector<HTMLInputElement>("#pLangs")!
        .value.split(",")
        .map((s) => s.trim())
        .filter(Boolean),
    };
    try {
      await api.saveProfile(next);
      toast("Профиль сохранён", "success");
      await deps.refresh();
      closeModal();
    } catch (error) {
      toast(errorText(error), "error");
    }
  });

  card.querySelector<HTMLButtonElement>("#btnDeleteProfile")?.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "Удалить профиль?",
      body: `Папка <code>profiles/${escapeHtml(profile.id)}/</code> со всеми досье и перепиской будет удалена безвозвратно.`,
      confirmLabel: "Удалить",
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteProfile(profile.id);
      store.activeModelId = null;
      store.activeManId = null;
      await deps.refresh();
      toast("Профиль удалён", "success");
    } catch (error) {
      toast(errorText(error), "error");
    }
  });
}

function splitLines(value: string): string[] {
  return value
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);
}

// ---------------------------------------------------------------------------
// Man card
// ---------------------------------------------------------------------------

export async function openManEditor(deps: ModalDeps, manId?: string) {
  const man = manId ? store.men.find((m) => m.id === manId) ?? null : activeMan();
  if (!man) {
    toast("Сначала выбери мужчину", "error");
    return;
  }
  let thread = { messages: [] as { role: string; text: string; ts: string }[] };
  try {
    thread = (await api.getChat(man.model_id, man.id)) as unknown as typeof thread;
  } catch {
    /* new dossier without history */
  }

  const card = openModal(`
    <h3>${escapeHtml(man.name)} · ID ${escapeHtml(man.id)}</h3>
    <div class="modal-sub">Досье живёт в <code>profiles/${escapeHtml(man.model_id)}/men/${escapeHtml(
      man.id,
    )}.json</code></div>

    <div class="field-grid">
      <div class="field"><label>Имя</label><input class="field-input" id="mName" value="${escapeHtml(
        man.name,
      )}" /></div>
      <div class="field"><label>Возраст</label><input class="field-input" id="mAge" type="number" value="${
        man.age ?? ""
      }" /></div>
      <div class="field"><label>Локация</label><input class="field-input" id="mLoc" value="${escapeHtml(
        man.location,
      )}" /></div>
      <div class="field"><label>Стадия</label><input class="field-input" id="mStage" value="${escapeHtml(
        man.stage,
      )}" /></div>
    </div>
    <div class="field"><label>Статус</label><input class="field-input" id="mStatus" value="${escapeHtml(
      man.status,
    )}" /></div>
    <div class="field"><label>Следующий шаг</label><input class="field-input" id="mNext" value="${escapeHtml(
      man.next_action,
    )}" /></div>
    <div class="field-grid">
      <div class="field"><label>Метки (через запятую)</label><input class="field-input" id="mTags" value="${escapeHtml(
        man.tags.join(", "),
      )}" /></div>
      <div class="field"><label>Аватар (URL)</label><input class="field-input" id="mAvatar" value="${escapeHtml(
        man.avatar,
      )}" /></div>
    </div>
    <div class="field"><label>Триггеры (по строке)</label><textarea class="field-area" id="mTriggers">${escapeHtml(
      man.triggers.join("\n"),
    )}</textarea></div>
    <div class="field"><label>Запреты (по строке)</label><textarea class="field-area" id="mBounds">${escapeHtml(
      man.boundaries.join("\n"),
    )}</textarea></div>

    <div class="field"><label>Факты (${man.facts.length})</label>
      <div class="code-block">${
        man.facts.length
          ? man.facts.map((f) => `${escapeHtml(f.key)}: ${escapeHtml(f.value)}`).join("\n")
          : "пусто"
      }</div>
    </div>
    <div class="field"><label>Подарки (${man.gifts.length})</label>
      <div class="code-block">${
        man.gifts.length
          ? man.gifts
              .map(
                (g) =>
                  `${formatDate(g.date)} — ${escapeHtml(g.title)}${g.value ? ` (${g.value})` : ""}`,
              )
              .join("\n")
          : "пусто"
      }</div>
    </div>
    <div class="field"><label>Заметки (${man.notes.length})</label>
      <div class="code-block">${
        man.notes.length
          ? man.notes.map((n) => `${formatDate(n.created_at)} — ${escapeHtml(n.text)}`).join("\n")
          : "пусто"
      }</div>
    </div>
    <div class="field"><label>Переписка (${thread.messages.length})</label>
      <div class="code-block">${
        thread.messages.length
          ? thread.messages
              .slice(-40)
              .map((m) => `${m.role === "incoming" ? "ОН" : m.role === "outgoing" ? "ОНА" : "•"}: ${escapeHtml(m.text)}`)
              .join("\n\n")
          : "пусто"
      }</div>
    </div>

    <div class="modal-actions">
      <button class="btn-send danger" id="btnDeleteMan">Удалить досье</button>
      <button class="btn-send secondary" data-act="close">Отмена</button>
      <button class="btn-send" id="btnSaveMan">Сохранить</button>
    </div>`);

  card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);

  card.querySelector<HTMLButtonElement>("#btnSaveMan")?.addEventListener("click", async () => {
    const next: Man = {
      ...man,
      name: card.querySelector<HTMLInputElement>("#mName")!.value.trim() || man.name,
      age: Number(card.querySelector<HTMLInputElement>("#mAge")!.value) || null,
      location: card.querySelector<HTMLInputElement>("#mLoc")!.value.trim(),
      stage: card.querySelector<HTMLInputElement>("#mStage")!.value.trim(),
      status: card.querySelector<HTMLInputElement>("#mStatus")!.value.trim(),
      next_action: card.querySelector<HTMLInputElement>("#mNext")!.value.trim(),
      avatar: card.querySelector<HTMLInputElement>("#mAvatar")!.value.trim(),
      tags: card
        .querySelector<HTMLInputElement>("#mTags")!
        .value.split(",")
        .map((s) => s.trim())
        .filter(Boolean),
      triggers: splitLines(card.querySelector<HTMLTextAreaElement>("#mTriggers")!.value),
      boundaries: splitLines(card.querySelector<HTMLTextAreaElement>("#mBounds")!.value),
    };
    try {
      await api.saveMan(next);
      toast("Досье сохранено", "success");
      await deps.refresh();
      closeModal();
    } catch (error) {
      toast(errorText(error), "error");
    }
  });

  card.querySelector<HTMLButtonElement>("#btnDeleteMan")?.addEventListener("click", async () => {
    const ok = await confirmDialog({
      title: "Удалить досье?",
      body: `Досье <b>${escapeHtml(man.name)}</b> и его переписка будут удалены безвозвратно.`,
      confirmLabel: "Удалить",
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteMan(man.model_id, man.id);
      await deps.selectMan(null);
      await deps.refresh();
      toast("Досье удалено", "success");
    } catch (error) {
      toast(errorText(error), "error");
    }
  });
}

export function bindWorkspaceShortcuts(handler: () => void) {
  $("workspace").addEventListener("dblclick", handler);
}
