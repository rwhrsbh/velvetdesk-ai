import { $, avatarHtml, escapeHtml, formatDate } from "./dom";
import { contactWord, keyWord, t } from "./i18n";
import {
  activeMan,
  activeProfile,
  alreadyFiled,
  dragging,
  attachmentUrl,
  splitDraft,
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
  if (provider && provider.id === "velvetdesk-cloud") {
    // The subscription is not a model anybody chose, and naming the model it
    // happens to answer with invites the question of how to change it. What
    // matters is whether the licence is in.
    label.textContent = provider.key_count
      ? t("provider.cloudOn")
      : t("provider.cloudOff");
    dot.className = provider.key_count ? "dot" : "dot off";
  } else if (provider) {
    label.textContent = provider.key_count
      ? t("provider.keys", {
          // A provider whose model has not been chosen yet says so, rather
          // than leaving the line starting with a dot and nothing before it.
          model: provider.model.trim() || t("provider.pickModel"),
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
  if (dragging) return;
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
      return `<div class="row-card ${p.id === store.activeModelId ? "active" : ""}" draggable="true" data-profile="${escapeHtml(p.id)}">
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
  if (dragging) return;
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
      return `<div class="row-card ${m.id === store.activeManId ? "active" : ""}" draggable="true" data-man="${escapeHtml(m.id)}">
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

/**
 * The little markdown a copilot actually writes.
 *
 * Answers come back with **bold**, bullet lists and `code` in them, and raw
 * asterisks in a letter look like a bug. This is deliberately not a markdown
 * parser: the text is escaped first and only then given tags, so nothing a
 * model writes can turn into markup of its own.
 */
export function markdown(text: string): string {
  const inline = (line: string) =>
    escapeHtml(line)
      .replace(/`([^`]+)`/g, "<code>$1</code>")
      .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
      .replace(/(^|[\s(])\*([^*\n]+)\*(?=[\s.,!?)]|$)/g, "$1<em>$2</em>")
      .replace(/(^|[\s(])_([^_\n]+)_(?=[\s.,!?)]|$)/g, "$1<em>$2</em>");

  const out: string[] = [];
  let list: "ul" | "ol" | null = null;

  const closeList = () => {
    if (list) out.push(`</${list}>`);
    list = null;
  };

  for (const raw of text.split("\n")) {
    const line = raw.trimEnd();
    const bullet = /^\s*[-*•]\s+(.*)$/.exec(line);
    const numbered = /^\s*(\d+)[.)]\s+(.*)$/.exec(line);
    const heading = /^#{1,6}\s+(.*)$/.exec(line);

    if (bullet) {
      if (list !== "ul") {
        closeList();
        out.push("<ul>");
        list = "ul";
      }
      out.push(`<li>${inline(bullet[1])}</li>`);
      continue;
    }
    if (numbered) {
      if (list !== "ol") {
        closeList();
        out.push("<ol>");
        list = "ol";
      }
      out.push(`<li>${inline(numbered[2])}</li>`);
      continue;
    }
    closeList();
    if (heading) {
      out.push(`<div class="md-head">${inline(heading[1])}</div>`);
      continue;
    }
    out.push(line.trim() ? `<div>${inline(line)}</div>` : `<div class="md-gap"></div>`);
  }
  closeList();
  return out.join("");
}

/** What a step says, translated when the core named it. */
export function stepText(step: {
  key?: string;
  params?: Record<string, string | number>;
  summary: string;
  detail?: unknown;
}): string {
  const key = modernKey(step);
  if (!key) return step.summary;
  const translated = t(key, step.params ?? {});
  return translated === key ? step.summary : translated;
}

/**
 * The key a step would carry if it ran today.
 *
 * A log is written once and read for months, and the wording it was written
 * with can turn out to be wrong — "written into the thread" said nothing about
 * whose letter it was, which is the difference between filing what he sent and
 * sending what she wrote. The old steps kept the arguments, so the side can be
 * read back out of them and the old runs say what they actually did.
 */
function modernKey(step: { key?: string; detail?: unknown }): string | undefined {
  if (step.key !== "step.appendChat") return step.key;
  const args = (step.detail as { args?: { role?: string } } | undefined)?.args;
  switch (args?.role) {
    case "incoming":
    case "him":
    case "his":
      return "step.appendIncoming";
    case "note":
      return "step.appendNote";
    case undefined:
      // Written before the role was recorded: nothing to go on, so it keeps
      // the wording it was written with.
      return step.key;
    default:
      return "step.appendOutgoing";
  }
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
  const waiting = (step.detail as { pending?: string } | undefined)?.pending;
  // Still in the queue: the operator can answer it here rather than hunting for
  // the panel, and the agent is still standing there waiting for the answer.
  const open = waiting ? store.pending.some((action) => action.id === waiting) : false;
  const badge = step.kind.includes("pending")
    ? `<span class="step-badge">${escapeHtml(t(open ? "chat.pending" : "chat.pendingDone"))}</span>`
    : "";
  const answer = open
    ? `<span class="step-answer">` +
      `<button class="btn btn-secondary" data-approve="${escapeHtml(waiting!)}">${t("queue.approve")}</button>` +
      `<button class="btn btn-secondary" data-reject="${escapeHtml(waiting!)}">${t("queue.reject")}</button>` +
      `</span>`
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
    badge +
    answer;

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
        /** set when the words are the app's own, so they read in this language */
        reply_key?: string;
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

      // Ask again, or ask differently: the same pair of buttons every chat
      // interface has, on the operator's own message and on the answer to it.
      // Both rewind to that message — what came after it is being replaced.
      // Drawn as icons: two words of Russian or English on every message is
      // a lot of furniture for two things nobody reads twice. The label
      // survives as the tooltip, so nothing is lost to somebody who has not
      // seen the icon before.
      const again = entry.transient
        ? ""
        : `<button class="msg-icon" data-act="retry" data-entry="${escapeHtml(entry.id)}" title="${escapeHtml(
            `${t("chat.retry")} — ${t("chat.retryHint")}`,
          )}" aria-label="${escapeHtml(t("chat.retry"))}">${RETRY_ICON}</button>` +
          `<button class="msg-icon" data-act="edit" data-entry="${escapeHtml(entry.id)}" title="${escapeHtml(
            `${t("chat.edit")} — ${t("chat.editHint")}`,
          )}" aria-label="${escapeHtml(t("chat.edit"))}">${PENCIL_ICON}</button>`;

      const asked =
        entry.sender === "user" && again
          ? `<div class="msg-actions">${again}</div>`
          : "";

      // Offering to file a draft that is already in the thread invites the
      // duplicate it would create.
      const filed = alreadyFiled(entry.text);
      const actions =
        entry.sender === "assistant" && !entry.transient
          ? `<div class="msg-actions">
               ${again}
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
        `<div class="bubble">${recipient}${thinking}${shots}${bubbleText(entry, meta.reply_key)}` +
        `${steps}${working}${usageLine(meta.usage, extras)}${actions}${asked}</div></div>`
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

/**
 * How long a message may be before it is folded.
 *
 * Long enough that ordinary answers are never touched, short enough that a
 * dossier dump or a pasted log does not push the rest of the conversation off
 * the screen.
 */
const FOLD_AT = 1200;

/**
 * The text of a bubble: the prose, the message meant for him set apart from
 * it, and a fold when the whole thing is too long to scroll past.
 *
 * The message to him gets its own block because that is what the buttons act
 * on — filing it, copying it — and because a draft that reads as part of the
 * commentary is a draft the operator sends with the commentary still in it.
 */
/** Circular arrow: ask the model the same thing again. */
const RETRY_ICON =
  '<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" ' +
  'stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
  '<path d="M13.5 8a5.5 5.5 0 1 1-1.7-3.97" /><path d="M13.6 2.4v3.2h-3.2" /></svg>';

/** Pencil: send the same thing again, worded differently. */
const PENCIL_ICON =
  '<svg viewBox="0 0 16 16" width="13" height="13" fill="none" stroke="currentColor" ' +
  'stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">' +
  '<path d="M11.2 2.3a1.6 1.6 0 0 1 2.3 2.3L5.4 12.7l-3 .7.7-3z" /><path d="M10.2 3.3l2.3 2.3" /></svg>';

function bubbleText(
  entry: { sender: string; text: string; id: string },
  replyKey?: string,
): string {
  // A reply the app wrote itself is stored in the language the core was
  // written in; the interface says it in its own.
  const source = replyKey && t(replyKey) !== replyKey ? t(replyKey) : entry.text;

  const body =
    entry.sender === "assistant"
      ? splitDraft(source)
          .map((part) =>
            part.kind === "draft"
              ? `<div class="draft"><div class="draft-label">${escapeHtml(
                  t("chat.draftLabel"),
                )}</div>${markdown(part.text)}</div>`
              : markdown(part.text),
          )
          .join("")
      : escapeHtml(source);

  if (source.length <= FOLD_AT) return `<span class="bubble-text">${body}</span>`;
  const open = store.expanded.includes(entry.id);
  return (
    `<span class="bubble-text${open ? "" : " folded"}">${body}</span>` +
    `<button class="show-more" data-act="expand" data-entry="${escapeHtml(entry.id)}">${
      open ? t("chat.showLess") : t("chat.showMore")
    }</button>`
  );
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
