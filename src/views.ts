import { $, avatarHtml, escapeHtml, formatDate } from "./dom";
import { contactWord, keyWord, t } from "./i18n";
import { activeMan, activeProfile, store, visibleMen, visibleProfiles } from "./store";
import type { RunStep, Usage } from "./types";

export function renderTopbar() {
  const settings = store.settings;
  const label = $("providerLabel");
  const dot = $("providerDot");

  if (!settings) {
    label.textContent = t("provider.loading");
    return;
  }
  const provider =
    settings.providers.find((p) => p.id === settings.active_provider) ?? settings.providers[0];
  if (provider) {
    label.textContent = provider.key_count
      ? t("provider.keys", {
          model: provider.model,
          count: provider.key_count,
          word: keyWord(provider.key_count),
        })
      : t("provider.noKey", { label: provider.label });
    dot.className = provider.key_count ? "dot" : "dot off";
  } else {
    label.textContent = t("provider.unset");
    dot.className = "dot off";
  }

  const count = $("pendingCount");
  count.textContent = String(store.pending.length);
  count.className = store.pending.length ? "count" : "count zero";

  document.querySelectorAll<HTMLButtonElement>("#modeControl .segmented-btn").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.mode === store.mode);
  });
  document.querySelectorAll<HTMLButtonElement>("#securityControl .segmented-btn").forEach((btn) => {
    btn.classList.toggle("active", btn.dataset.security === store.security);
  });
}

const indexCounts = new Map<string, number>();

export function setIndexCounts(pairs: Array<[string, number]>) {
  indexCounts.clear();
  pairs.forEach(([id, count]) => indexCounts.set(id, count));
}

export function renderProfiles() {
  const container = $("profileList");
  const profiles = visibleProfiles();
  if (profiles.length === 0) {
    container.innerHTML = `<div class="empty-hint">
      ${t("empty.noProfiles")}<br />${t("empty.createOrSeed")}
    </div>`;
    return;
  }
  container.innerHTML = profiles
    .map((p) => {
      const men = p.id === store.activeModelId ? store.men.length : (indexCounts.get(p.id) ?? 0);
      const meta = [
        p.site || t("common.noSite"),
        t("profile.contacts", { n: men, word: contactWord(men) }),
      ];
      return `<div class="row-card ${p.id === store.activeModelId ? "active" : ""}" data-profile="${escapeHtml(p.id)}">
        <div class="row-main">
          ${avatarHtml(p.name, p.avatar)}
          <div class="row-text">
            <div class="row-title">${escapeHtml(p.name)}${p.age ? `, ${p.age}` : ""}</div>
            <div class="row-sub">${escapeHtml(meta.join(" · "))}</div>
          </div>
        </div>
      </div>`;
    })
    .join("");
}

export function renderScope() {
  const profile = activeProfile();
  const dot = $("scopeDot");
  dot.className = `dot ${store.busy ? "busy" : profile ? "" : "idle"}`;

  if (store.master) {
    $("scopeLabel").textContent = t("master.scope");
    $("scopePath").textContent = t("master.scopePath");
    return;
  }
  if (!profile) {
    $("scopeLabel").textContent = t("scope.none");
    $("scopePath").textContent = "—";
    return;
  }
  const man = activeMan();
  $("scopeLabel").textContent = man ? `${profile.name} → ${man.name}` : profile.name;
  $("scopePath").textContent = `profiles/${profile.id}/`;
}

export function renderMen() {
  const back = document.getElementById("btnDeselectMan");
  if (back) back.hidden = !store.activeManId;

  const container = $("menList");
  if (!store.activeModelId) {
    container.innerHTML = `<div class="empty-hint">${t("empty.pickProfile")}</div>`;
    return;
  }
  const men = visibleMen();
  if (men.length === 0) {
    container.innerHTML = `<div class="empty-hint">${t("empty.noMen")}<br />${t("empty.addMan")}</div>`;
    return;
  }
  container.innerHTML = men
    .map((m) => {
      const tags = m.tags
        .slice(0, 4)
        .map((tag) => `<span class="tag">${escapeHtml(tag)}</span>`)
        .join("");
      const gifts = m.gifts.length
        ? `<span class="tag gift">${escapeHtml(t("man.gifts.short", { n: m.gifts.length }))}</span>`
        : "";
      const sub = [m.location, `ID ${m.id}`, m.age ? t("man.years", { n: m.age }) : ""]
        .filter(Boolean)
        .join(" · ");
      return `<div class="row-card ${m.id === store.activeManId ? "active" : ""}" data-man="${escapeHtml(m.id)}">
        <div class="row-main">
          ${avatarHtml(m.name, m.avatar)}
          <div class="row-text">
            <div class="row-title">${escapeHtml(m.name)}</div>
            <div class="row-sub">${escapeHtml(sub)}</div>
          </div>
        </div>
        ${m.status ? `<div class="row-note">${escapeHtml(m.status)}</div>` : ""}
        <div class="tags-row">
          ${m.stage ? `<span class="tag stage">${escapeHtml(m.stage)}</span>` : ""}
          ${tags}${gifts}
          ${m.last_contact ? `<span class="tag">${formatDate(m.last_contact)}</span>` : ""}
        </div>
      </div>`;
    })
    .join("");
}

function usageLine(usage: Usage | undefined, extra: string[]): string {
  if (!usage) return "";
  const total = usage.total_tokens || usage.prompt_tokens + usage.completion_tokens;
  const bits = [t("chat.tokens", { n: total }), ...extra].filter(Boolean);
  return `<div class="usage">${escapeHtml(bits.join(" \u00b7 "))}</div>`;
}

/// One line per step: a coloured dot, what ran, what it did. The markup is
/// written without indentation on purpose — the bubble renders text as
/// `pre-wrap`, so any newline inside it would show up as blank space.
/** What a step says, translated when the core named it. */
export function stepText(step: { key?: string; params?: Record<string, string | number>; summary: string }): string {
  if (!step.key) return step.summary;
  const translated = t(step.key, step.params ?? {});
  return translated === step.key ? step.summary : translated;
}

function stepHtml(step: RunStep): string {
  const cls = step.kind.includes("error")
    ? "error"
    : step.kind.includes("pending")
      ? "pending"
      : "";
  const badge = step.kind.includes("pending")
    ? `<span class="step-badge">${escapeHtml(t("chat.pending"))}</span>`
    : "";
  const tool = step.tool ? `<span class="step-tool">${escapeHtml(step.tool)}</span>` : "";
  return (
    `<div class="step ${cls}">` +
    tool +
    `<span class="step-text">${escapeHtml(stepText(step))}</span>` +
    badge +
    `</div>`
  );
}

export function renderChat() {
  const container = $("messages");
  if (!store.activeModelId) {
    container.innerHTML = `<div class="msg system"><div class="bubble">${t("empty.needProfile")}</div></div>`;
    return;
  }

  container.innerHTML = store.entries
    .map((entry) => {
      const meta = (entry.meta ?? {}) as {
        steps?: RunStep[];
        usage?: Usage;
        mode?: string;
        key_index?: number;
        turns?: number;
        /** set while the run is still going */
        live?: boolean;
        note?: string;
        /** the model's own account of its reasoning */
        thoughts?: string;
        thinkingSince?: number;
      };
      const steps = Array.isArray(meta.steps) ? meta.steps.map(stepHtml).join("") : "";
      const extras: string[] = [];
      if (meta.mode) extras.push(String(meta.mode).toUpperCase());
      if (typeof meta.key_index === "number") extras.push(t("chat.key", { n: meta.key_index + 1 }));
      if (typeof meta.turns === "number" && meta.turns > 1)
        extras.push(t("chat.turns", { n: meta.turns }));

      const actions =
        entry.sender === "assistant" && !entry.transient
          ? `<div class="msg-actions">
               <button data-act="copy" data-entry="${escapeHtml(entry.id)}">${t("chat.copy")}</button>
               <button data-act="send-as-outgoing" data-entry="${escapeHtml(entry.id)}">${t("chat.asOutgoing")}</button>
             </div>`
          : "";

      // Reasoning is folded away: it is worth having, rarely worth reading.
      const thinking = meta.thoughts?.trim()
        ? `<details class="thoughts"><summary>${escapeHtml(
            t("chat.thoughts", { n: Math.max(1, Math.round((meta.thoughts.length / 400) * 1)) }),
          )}</summary><div class="thoughts-body">${escapeHtml(meta.thoughts.trim())}</div></details>`
        : "";

      const working = meta.live
        ? `<div class="working"><span class="spinner"></span>` +
          `<span>${escapeHtml(meta.note || t("chat.working"))}</span></div>`
        : "";

      return (
        `<div class="msg ${entry.sender}" data-entry="${escapeHtml(entry.id)}">` +
        `<div class="bubble">${thinking}<span class="bubble-text">${escapeHtml(entry.text)}</span>` +
        `${steps}${working}${usageLine(meta.usage, extras)}${actions}</div></div>`
      );
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
