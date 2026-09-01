import { $, avatarHtml, escapeHtml, formatDate } from "./dom";
import { activeMan, activeProfile, store, visibleMen, visibleProfiles } from "./store";
import type { RunStep, Usage } from "./types";

export function renderTopbar() {
  const settings = store.settings;
  const providerBadge = $("providerBadge");
  const keyBadge = $("keyBadge");

  if (!settings) {
    providerBadge.textContent = "загрузка…";
    return;
  }
  const provider = settings.providers.find((p) => p.id === settings.active_provider) ?? settings.providers[0];
  if (provider) {
    providerBadge.textContent = `${provider.label} · ${provider.model}`;
    keyBadge.textContent = `ключей: ${provider.key_count}`;
    keyBadge.className = provider.key_count > 0 ? "badge green" : "badge red";
  } else {
    providerBadge.textContent = "провайдер не настроен";
    keyBadge.className = "badge red";
  }

  $("pendingCount").textContent = String(store.pending.length);
  $("currentModeLabel").textContent = store.mode.toUpperCase();
  $("currentSecLabel").textContent = store.security.toUpperCase();

  document.querySelectorAll<HTMLButtonElement>("#modeControl .segmented-btn").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.mode === store.mode);
  });
  document.querySelectorAll<HTMLButtonElement>("#securityControl .segmented-btn").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.security === store.security);
  });
}

export function renderProfiles() {
  const container = $("profileList");
  const profiles = visibleProfiles();
  if (profiles.length === 0) {
    container.innerHTML = `<div class="empty-hint">
      Нет профилей.<br />Нажми ＋ или <a href="#" data-act="seed" style="color:var(--accent-blue)">загрузи демо-профиль</a>.
    </div>`;
    return;
  }
  container.innerHTML = profiles
    .map((p) => {
      const count = p.id === store.activeModelId ? store.men.length : countFromIndex(p.id);
      return `<div class="profile-card ${p.id === store.activeModelId ? "active" : ""}" data-profile="${escapeHtml(
        p.id,
      )}">
        ${avatarHtml(p.name, p.avatar)}
        <div class="profile-info">
          <div class="profile-name">${escapeHtml(p.name)}${p.age ? `, ${p.age}` : ""}</div>
          <div class="profile-meta">${escapeHtml(p.site || "без сайта")} · ${count} контактов</div>
        </div>
      </div>`;
    })
    .join("");
}

const indexCounts = new Map<string, number>();

export function setIndexCounts(pairs: Array<[string, number]>) {
  indexCounts.clear();
  pairs.forEach(([id, count]) => indexCounts.set(id, count));
}

function countFromIndex(modelId: string): number {
  return indexCounts.get(modelId) ?? 0;
}

export function renderScope() {
  const profile = activeProfile();
  const dot = $("scopeDot");
  dot.className = `status-dot ${store.busy ? "busy" : profile ? "" : "idle"}`;

  if (!profile) {
    $("activeScopeLabel").textContent = "Профиль не выбран";
    $("scopePath").textContent = "—";
    return;
  }
  const man = activeMan();
  $("activeScopeLabel").textContent = man
    ? `${profile.name} → ${man.name}`
    : `${profile.name} (ID: ${profile.id})`;
  $("scopePath").textContent = `profiles/${profile.id}/`;
}

export function renderMen() {
  const container = $("menList");
  if (!store.activeModelId) {
    container.innerHTML = `<div class="empty-hint">Выбери модель слева.</div>`;
    return;
  }
  const men = visibleMen();
  if (men.length === 0) {
    container.innerHTML = `<div class="empty-hint">Нет мужчин в работе.<br />Нажми ＋ или вставь письмо в мастер-агент.</div>`;
    return;
  }
  container.innerHTML = men
    .map((m) => {
      const gifts = m.gifts
        .slice(-3)
        .map((g) => `<span class="mini-pill gift">🎁 ${escapeHtml(g.title)}</span>`)
        .join("");
      const tags = m.tags
        .slice(0, 5)
        .map((t) => `<span class="mini-pill">${escapeHtml(t)}</span>`)
        .join("");
      return `<div class="man-card ${m.id === store.activeManId ? "active" : ""}" data-man="${escapeHtml(m.id)}">
        <div class="man-card-top">
          ${avatarHtml(m.name, m.avatar, "small")}
          <div class="man-details">
            <div class="man-name">${escapeHtml(m.name)}${m.age ? ` (${m.age})` : ""}</div>
            <div class="man-tag">${escapeHtml(m.location || "—")} · ID: ${escapeHtml(m.id)}</div>
          </div>
        </div>
        ${m.status ? `<div class="man-status">${escapeHtml(m.status)}</div>` : ""}
        <div class="tags-row">
          ${m.stage ? `<span class="mini-pill stage">${escapeHtml(m.stage)}</span>` : ""}
          ${tags}${gifts}
          <span class="mini-pill">⏱ ${formatDate(m.last_contact)}</span>
        </div>
      </div>`;
    })
    .join("");
}

function usageLine(usage: Usage | undefined, extra: string[] = []): string {
  if (!usage) return "";
  const bits = [
    `tokens ${usage.total_tokens || usage.prompt_tokens + usage.completion_tokens}`,
    ...extra,
  ];
  return `<div class="usage-line">${escapeHtml(bits.join(" · "))}</div>`;
}

function stepHtml(step: RunStep): string {
  const cls =
    step.kind.includes("error") ? "error" : step.kind.includes("pending") ? "pending" : "";
  const badge = step.kind.includes("pending")
    ? "ожидает подтверждения"
    : step.kind.includes("error")
      ? "ошибка"
      : "выполнено";
  return `<div class="tool-call-box ${cls}">
    <div class="tool-header"><span>⚡ ${escapeHtml(step.tool ?? step.kind)}</span><span>${badge}</span></div>
    <div>${escapeHtml(step.summary)}</div>
  </div>`;
}

export function renderChat() {
  const container = $("chatMessages");
  if (!store.activeModelId) {
    container.innerHTML = `<div class="message-row system"><div class="msg-bubble">
      VelvetDesk запущен. Создай профиль модели, чтобы включить изолированный агент.
    </div></div>`;
    return;
  }

  container.innerHTML = store.entries
    .map((entry) => {
      const meta = (entry.meta ?? {}) as {
        steps?: RunStep[];
        usage?: Usage;
        mode?: string;
        pending?: number;
        key_index?: number;
        turns?: number;
      };
      const steps = Array.isArray(meta.steps) ? meta.steps.map(stepHtml).join("") : "";
      const extras: string[] = [];
      if (meta.mode) extras.push(String(meta.mode).toUpperCase());
      if (typeof meta.key_index === "number") extras.push(`key #${meta.key_index + 1}`);
      if (typeof meta.turns === "number" && meta.turns > 1) extras.push(`${meta.turns} turns`);

      const actions =
        entry.sender === "assistant" && !entry.transient
          ? `<div class="msg-actions">
               <button data-act="copy" data-entry="${escapeHtml(entry.id)}">Копировать</button>
               <button data-act="send-as-outgoing" data-entry="${escapeHtml(entry.id)}">В историю как отправленное</button>
             </div>`
          : "";

      return `<div class="message-row ${entry.sender}">
        <div class="msg-bubble">${escapeHtml(entry.text)}${steps}${usageLine(meta.usage, extras)}${actions}</div>
      </div>`;
    })
    .join("");
  container.scrollTop = container.scrollHeight;
}

export function renderAll() {
  renderTopbar();
  renderProfiles();
  renderScope();
  renderMen();
  renderChat();
}
