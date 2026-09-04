import { $, avatarHtml, escapeHtml, formatDate } from "./dom";
import { contactWord, keyWord, t } from "./i18n";
import {
  activeMan,
  activeProfile,
  alreadyFiled,
  attachmentUrl,
  store,
  visibleMen,
  visibleProfiles,
} from "./store";
import type { Attachment } from "./store";
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

/** What a step says, translated when the core named it. */
export function stepText(step: {
  key?: string;
  params?: Record<string, string | number>;
  summary: string;
}): string {
  if (!step.key) return step.summary;
  const translated = t(step.key, step.params ?? {});
  return translated === step.key ? step.summary : translated;
}

/**
 * One step of a run, openable.
 *
 * Closed it is a line: which tool ran and what it did. Open it shows the
 * substance — the fields a write changed, before and after; the text it wrote;
 * or, for a read, what came back. That is the difference between a log an
 * operator scrolls past and one they can check.
 */
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

  const params = (step.params ?? {}) as { text?: string; before?: string };
  const detail = (step.detail ?? {}) as {
    changes?: { field: string; before: string; after: string }[];
    result?: string;
  };

  const written = typeof params.text === "string" ? params.text.trim() : "";
  const changes = Array.isArray(detail.changes) ? detail.changes : [];

  const parts: string[] = [];

  // What the write changed, field by field.
  if (changes.length > 0) {
    parts.push(
      `<div class="step-diff">` +
        changes
          .map(
            (change) =>
              `<div class="diff-row"><span class="diff-field">${escapeHtml(change.field)}</span>` +
              (change.before
                ? `<span class="diff-before">${escapeHtml(change.before)}</span>`
                : "") +
              `<span class="diff-after">${escapeHtml(change.after)}</span></div>`,
          )
          .join("") +
        `</div>`,
    );
  }

  // The text it wrote, in full — a letter, a note, a rewritten persona.
  if (written && changes.length === 0) {
    if (params.before?.trim()) {
      parts.push(
        `<div class="step-was"><span class="step-label">${escapeHtml(t("chat.wasBefore"))}</span>` +
          `<div class="step-body">${escapeHtml(params.before.trim())}</div></div>`,
      );
    }
    parts.push(`<div class="step-body">${escapeHtml(written)}</div>`);
  }

  // What a read came back with.
  if (parts.length === 0 && detail.result) {
    parts.push(`<div class="step-body mono">${escapeHtml(prettyJson(detail.result))}</div>`);
  }

  const head =
    `<span class="step-caret">›</span>` +
    tool +
    `<span class="step-text">${escapeHtml(stepText(step))}</span>` +
    badge;

  if (parts.length === 0) {
    return `<div class="step ${cls} step-plain">${head}</div>`;
  }
  return (
    `<details class="step ${cls}">` +
    `<summary class="step-head">${head}</summary>` +
    parts.join("") +
    `</details>`
  );
}

/** Lay a returned payload out so it can be read rather than decoded. */
function prettyJson(raw: string): string {
  try {
    return JSON.stringify(JSON.parse(raw), null, 2);
  } catch {
    return raw;
  }
}

export function renderChat() {
  const container = $("messages");
  const keep = container.scrollTop;
  const stick = container.scrollTop + container.clientHeight >= container.scrollHeight - 48;
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
        /** which model answered, when a fallback took over */
        model?: string;
        /** pictures the operator sent with this message */
        images?: Attachment[];
        /** the provider's answer as it arrived */
        raw?: string;
        /** letters carry their recipient */
        letter?: boolean;
        recipient?: string;
        failed?: boolean;
      };
      const steps = Array.isArray(meta.steps) ? meta.steps.map(stepHtml).join("") : "";
      const extras: string[] = [];
      if (meta.mode) extras.push(String(meta.mode).toUpperCase());
      if (meta.model) extras.push(t("chat.viaModel", { model: meta.model }));
      if (typeof meta.key_index === "number") extras.push(t("chat.key", { n: meta.key_index + 1 }));
      if (typeof meta.turns === "number" && meta.turns > 1)
        extras.push(t("chat.turns", { n: meta.turns }));

      // Offering to file a draft that is already in the thread invites the
      // duplicate it would create.
      const filed = alreadyFiled(entry.text);
      const actions =
        entry.sender === "assistant" && !entry.transient
          ? `<div class="msg-actions">
               <button data-act="copy" data-entry="${escapeHtml(entry.id)}">${t("chat.copy")}</button>
               ${
                 filed
                   ? `<span class="msg-filed">${escapeHtml(t("chat.alreadyLogged"))}</span>`
                   : `<button data-act="send-as-outgoing" data-entry="${escapeHtml(entry.id)}">${t("chat.asOutgoing")}</button>`
               }
               ${
                 meta.raw
                   ? `<button data-act="raw" data-entry="${escapeHtml(entry.id)}">${t("chat.raw")}</button>`
                   : ""
               }

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

      const recipient = meta.letter
        ? `<div class="letter-to${meta.failed ? " failed" : ""}">${escapeHtml(
            t("letters.to", { name: meta.recipient ?? "" }),
          )}</div>`
        : "";

      // What was attached stays visible in the message it went out with.
      const shots = meta.images?.length
        ? `<div class="msg-thumbs">${thumbsHtml(meta.images, false)}</div>`
        : "";

      const picked = store.selecting && store.selected.includes(entry.id);

      return (
        `<div class="msg ${entry.sender}${picked ? " picked" : ""}" data-entry="${escapeHtml(entry.id)}">` +
        `<div class="bubble">${recipient}${thinking}${shots}<span class="bubble-text">${escapeHtml(entry.text)}</span>` +
        `${steps}${working}${usageLine(meta.usage, extras)}${actions}</div></div>`
      );
    })
    .join("");

  // Following the conversation means staying at the bottom; reading further up
  // — or dragging a selection across old messages — means staying where you
  // are, so a redraw does not yank the chat away.
  if (stick) {
    container.scrollTop = container.scrollHeight;
  } else {
    container.scrollTop = keep;
  }
}

/** A row of thumbnails: what is attached, or what a message went out with. */
function thumbsHtml(items: Attachment[], removable: boolean): string {
  return items
    .map(
      (item) =>
        `<figure class="thumb" title="${escapeHtml(item.name)}">` +
        `<img src="${attachmentUrl(item)}" alt="${escapeHtml(item.name)}" />` +
        (removable
          ? `<button class="thumb-drop" data-attach="${escapeHtml(item.id)}" ` +
            `title="${escapeHtml(t("composer.attachDrop"))}">×</button>`
          : "") +
        `</figure>`,
    )
    .join("");
}

/** What will go out with the next message. */
export function renderAttachments() {
  const box = $("attachments");
  if (store.attachments.length === 0) {
    box.innerHTML = "";
    box.hidden = true;
    return;
  }
  box.hidden = false;
  box.innerHTML = thumbsHtml(store.attachments, true);
}

/**
 * What is lined up behind the current run.
 *
 * Each line is a message the operator typed while the model was still working;
 * it goes out on its own as soon as the run before it ends, and can be dropped
 * until then.
 */
/** While messages are being picked: how many, and what can be done with them. */
export function renderSelection() {
  const bar = $("selectBar");
  const container = $("messages");
  container.classList.toggle("selecting", store.selecting);
  if (!store.selecting) {
    bar.innerHTML = "";
    bar.hidden = true;
    return;
  }
  bar.hidden = false;
  bar.innerHTML =
    `<span class="select-count">${escapeHtml(
      t("chat.selectedCount", { n: store.selected.length }),
    )}</span>` +
    `<button class="btn btn-secondary" data-act="select-cancel">${t("common.cancel")}</button>` +
    `<button class="btn btn-danger" data-act="select-delete"${
      store.selected.length === 0 ? " disabled" : ""
    }>${t("common.delete")}</button>`;
}

export function renderQueue() {
  const box = $("queue");
  // Another chat's queue belongs above another chat's composer.
  const here = store.queue.filter(
    (item) =>
      item.target.master === store.master &&
      item.target.modelId === store.activeModelId &&
      item.target.manId === store.activeManId,
  );
  if (here.length === 0) {
    box.innerHTML = "";
    box.hidden = true;
    return;
  }
  box.hidden = false;
  box.innerHTML = here
    .map(
      (item, index) =>
        `<div class="queue-item" title="${escapeHtml(t("composer.queuedHint"))}">` +
        `<span class="queue-mark">↵</span>` +
        `<span class="queue-text">${escapeHtml(item.text)}</span>` +
        (item.attachments.length > 0
          ? `<span class="queue-count">${escapeHtml(
              t("composer.attachCount", { n: item.attachments.length }),
            )}</span>`
          : "") +
        `<button class="queue-drop" data-queue="${index}" ` +
        `title="${escapeHtml(t("composer.queueDrop"))}">×</button>` +
        `</div>`,
    )
    .join("");
}

export function renderAll() {
  renderTopbar();
  renderProfiles();
  renderScope();
  renderMen();
  renderChat();
  renderQueue();
  renderAttachments();
  renderSelection();
}
