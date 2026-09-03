import { api, errorText } from "./api";
import type { ModalDeps } from "./deps";
import { closeModal, confirmDialog, escapeHtml, formatDate, openModal, toast } from "./dom";
import { dressCombo, dressSelectsIn } from "./dropdown";
import { t } from "./i18n";
import { store } from "./store";
import type { Channel, ChatMessage, Fact, Man, MsgRole, Note, Profile } from "./types";

const AVATAR_SIZE = 256;

/**
 * The dating sites an operator is likely to be working on.
 *
 * A suggestion list, not a closed set: the field takes anything, and a site
 * that is not here is typed in as it is written on the site itself.
 */
const SITES = [
  "RomanceCompass",
  "Jump 4 Love",
  "Charmdate",
  "Natashaclub",
  "Ladadate",
  "Bridge Of Love",
  "Svadba",
  "Goldenbride",
  "Generationlove",
  "Dream Singles",
  "Findbride",
  "UDate",
  "Brides4Love",
  "LoveInChat",
  "Charmlive",
  "Love-Temptation",
  "Victoriyaclub",
  "Bride Forever",
  "Waytobride",
  "Hanuma",
] as const;

/** Both sources stay available: a blocked URL falls back to a local file. */
function avatarField(current: string): string {
  return `
    <div class="field">
      <label>${t("profile.photo")}</label>
      <div class="avatar-picker">
        <div class="avatar-preview" id="avatarPreview">${
          current
            ? `<img src="${escapeHtml(current)}" alt="" id="avatarImg" />`
            : `<span class="avatar-empty">${t("profile.noPhoto")}</span>`
        }</div>
        <div class="avatar-controls">
          <input class="field-input" id="avatarUrl" placeholder="${t("profile.photoUrl")}" value="${
            current.startsWith("data:") ? "" : escapeHtml(current)
          }" />
          <div class="or-row"><span>${t("common.or")}</span></div>
          <label class="file-button">
            <input type="file" id="avatarFile" accept="image/*" hidden />
            ${t("profile.pickFile")}
          </label>
          <div class="hint-inline" id="avatarHint">${t("profile.fileHint", { size: AVATAR_SIZE })}</div>
          <button type="button" class="link-button" id="avatarClear">${t("common.remove")}</button>
        </div>
      </div>
      <input type="hidden" id="avatarValue" value="${escapeHtml(current)}" />
    </div>`;
}

function bindAvatarField(card: HTMLElement) {
  const hidden = card.querySelector<HTMLInputElement>("#avatarValue")!;
  const preview = card.querySelector<HTMLElement>("#avatarPreview")!;
  const urlInput = card.querySelector<HTMLInputElement>("#avatarUrl")!;
  const fileInput = card.querySelector<HTMLInputElement>("#avatarFile")!;
  const hint = card.querySelector<HTMLElement>("#avatarHint")!;

  const setPreview = (value: string) => {
    hidden.value = value;
    if (!value) {
      preview.innerHTML = `<span class="avatar-empty">${t("form.noPhoto")}</span>`;
      return;
    }
    preview.innerHTML = `<img src="${escapeHtml(value)}" alt="" id="avatarImg" />`;
    const img = preview.querySelector<HTMLImageElement>("#avatarImg");
    if (img && !value.startsWith("data:")) {
      img.addEventListener("error", () => {
        preview.innerHTML = `<span class="avatar-empty">${t("profile.urlFailed")}</span>`;
        hint.textContent = t("profile.urlBlocked");
        hint.classList.add("warn");
      });
      img.addEventListener("load", () => {
        hint.textContent = t("profile.fileHint", { size: AVATAR_SIZE });
        hint.classList.remove("warn");
      });
    }
  };

  urlInput.addEventListener("input", () => {
    fileInput.value = "";
    setPreview(urlInput.value.trim());
  });

  fileInput.addEventListener("change", async () => {
    const file = fileInput.files?.[0];
    if (!file) return;
    try {
      const data = await fileToDataUri(file);
      urlInput.value = "";
      hint.textContent = t("profile.fileSaved", { name: file.name });
      hint.classList.remove("warn");
      setPreview(data);
    } catch (error) {
      toast(t("toast.photoFailed", { error: errorText(error) }), "error");
    }
  });

  card.querySelector<HTMLButtonElement>("#avatarClear")?.addEventListener("click", () => {
    urlInput.value = "";
    fileInput.value = "";
    setPreview("");
  });
}

function fileToDataUri(file: File): Promise<string> {
  return new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onerror = () => reject(new Error("read failed"));
    reader.onload = () => {
      const image = new Image();
      image.onerror = () => reject(new Error("not an image"));
      image.onload = () => {
        const scale = Math.min(1, AVATAR_SIZE / Math.max(image.width, image.height));
        const canvas = document.createElement("canvas");
        canvas.width = Math.round(image.width * scale);
        canvas.height = Math.round(image.height * scale);
        const ctx = canvas.getContext("2d");
        if (!ctx) return reject(new Error("no canvas"));
        ctx.drawImage(image, 0, 0, canvas.width, canvas.height);
        resolve(canvas.toDataURL("image/jpeg", 0.82));
      };
      image.src = String(reader.result);
    };
    reader.readAsDataURL(file);
  });
}

function lines(value: string): string[] {
  return value
    .split("\n")
    .map((s) => s.trim())
    .filter(Boolean);
}

function csv(value: string): string[] {
  return value
    .split(",")
    .map((s) => s.trim())
    .filter(Boolean);
}

// ---------------------------------------------------------------------------
// Model profile
// ---------------------------------------------------------------------------

export async function openProfileForm(deps: ModalDeps, existing?: Profile | null) {
  const p = existing ?? null;
  const title = p ? t("profile.editTitle", { name: escapeHtml(p.name) }) : t("profile.newTitle");

  const card = openModal(`
    <h3>${title}</h3>
    <div class="modal-sub">
      ${
        p
          ? t("profile.editSub", { id: escapeHtml(p.id) })
          : t("profile.newSub")
      }
    </div>

    ${avatarField(p?.avatar ?? "")}

    <div class="field-grid">
      <div class="field"><label>${t("profile.name")}</label>
        <input class="field-input" id="fName" value="${escapeHtml(p?.name ?? "")}" placeholder="Marina Kazachok" /></div>
      <div class="field"><label>${t("profile.age")}</label>
        <input class="field-input" id="fAge" type="number" min="18" max="99" value="${p?.age ?? ""}" placeholder="42" /></div>
      <div class="field"><label>${t("profile.site")}</label>
        <input class="field-input" id="fSite" value="${escapeHtml(p?.site ?? "")}" placeholder="RomanceCompass" />
      </div>
      <div class="field"><label>${t("profile.siteId")}</label>
        <input class="field-input" id="fId" value="${escapeHtml(p?.id ?? "")}" placeholder="${t("profile.idHint")}" ${
          p ? "disabled" : ""
        } /></div>
    </div>

    <div class="field"><label>${t("profile.languages")}</label>
      <input class="field-input" id="fLangs" value="${escapeHtml((p?.languages ?? ["en"]).join(", "))}" placeholder="en, de, ru" /></div>

    <div class="field"><label>${t("profile.bio")}</label>
      <textarea class="field-area" id="fBio" placeholder="${t("profile.bioHint")}">${escapeHtml(
        p?.bio ?? "",
      )}</textarea></div>

    <div class="field"><label>${t("profile.persona")}</label>
      <textarea class="field-area" id="fPrompt" placeholder="${t("profile.personaHint")}">${escapeHtml(
        p?.system_prompt_override ?? "",
      )}</textarea></div>

    <details class="advanced">
      <summary>${t("profile.toneSection")}</summary>
      <div class="field"><label>${t("profile.tone")}</label>
        <textarea class="field-area" id="fTone" placeholder="${t("profile.toneHint")}">${escapeHtml(
          (p?.tone_rules ?? []).join("\n"),
        )}</textarea></div>
      <div class="field"><label>${t("profile.samples")}</label>
        <textarea class="field-area tall" id="fSamples" placeholder="${t("profile.samplesHint")}">${escapeHtml(
          (p?.writing_samples ?? []).join("\n\n"),
        )}</textarea></div>
      <div class="field"><label>${t("profile.banned")}</label>
        <textarea class="field-area" id="fBanned" placeholder="I hope this message finds you well">${escapeHtml(
          (p?.banned_phrases ?? []).join("\n"),
        )}</textarea></div>
      ${
        p && p.facts.length
          ? `<div class="field"><label>${t("profile.factsCollected", { n: p.facts.length })}</label>
               <div class="code-block">${p.facts
                 .map((f) => `${escapeHtml(f.key)}: ${escapeHtml(f.value)}`)
                 .join("\n")}</div></div>`
          : ""
      }
    </details>

    <div class="modal-actions">
      ${p ? `<button class="btn btn-danger" id="btnDelete">${t("profile.delete")}</button>` : ""}
      <button class="btn btn-secondary" data-act="close">${t("common.cancel")}</button>
      <button class="btn btn-primary" id="btnSubmit">${p ? t("common.save") : t("profile.create")}</button>
    </div>`);

  bindAvatarField(card);
  dressCombo(card.querySelector<HTMLInputElement>("#fSite")!, SITES);
  card.querySelector<HTMLInputElement>("#fName")?.focus();
  card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);

  const read = () => ({
    name: card.querySelector<HTMLInputElement>("#fName")!.value.trim(),
    age: Number(card.querySelector<HTMLInputElement>("#fAge")!.value) || null,
    site: card.querySelector<HTMLInputElement>("#fSite")!.value.trim(),
    id: card.querySelector<HTMLInputElement>("#fId")!.value.trim(),
    avatar: card.querySelector<HTMLInputElement>("#avatarValue")!.value.trim(),
    bio: card.querySelector<HTMLTextAreaElement>("#fBio")!.value.trim(),
    prompt: card.querySelector<HTMLTextAreaElement>("#fPrompt")!.value.trim(),
    languages: csv(card.querySelector<HTMLInputElement>("#fLangs")!.value),
    tone: lines(card.querySelector<HTMLTextAreaElement>("#fTone")!.value),
    banned: lines(card.querySelector<HTMLTextAreaElement>("#fBanned")!.value),
    // A sample letter runs over several lines, so they are separated by blank
    // lines rather than by newlines.
    samples: card
      .querySelector<HTMLTextAreaElement>("#fSamples")!
      .value.split(/\n\s*\n/)
      .map((part) => part.trim())
      .filter(Boolean),
  });

  card.querySelector<HTMLButtonElement>("#btnSubmit")?.addEventListener("click", async () => {
    const form = read();
    if (!form.name) {
      toast(t("toast.nameRequired"), "error");
      return;
    }
    try {
      if (p) {
        await api.saveProfile({
          ...p,
          name: form.name,
          age: form.age,
          site: form.site,
          avatar: form.avatar,
          bio: form.bio,
          system_prompt_override: form.prompt,
          languages: form.languages,
          tone_rules: form.tone,
          writing_samples: form.samples,
          banned_phrases: form.banned,
        });
        toast(t("toast.profileSaved"), "success");
        await deps.refresh();
      } else {
        const created = await api.createProfile({
          name: form.name,
          id: form.id || undefined,
          age: form.age ?? undefined,
          site: form.site,
          avatar: form.avatar,
          bio: form.bio,
          system_prompt_override: form.prompt,
          languages: form.languages,
          tone_rules: form.tone,
          writing_samples: form.samples,
          banned_phrases: form.banned,
        });
        toast(t("toast.profileCreated", { name: created.name }), "success");
        await deps.refresh();
        await deps.selectProfile(created.id);
      }
      closeModal();
    } catch (error) {
      toast(errorText(error), "error");
    }
  });

  card.querySelector<HTMLButtonElement>("#btnDelete")?.addEventListener("click", async () => {
    if (!p) return;
    const ok = await confirmDialog({
      title: t("profile.deleteTitle"),
      body: t("profile.deleteBody", { id: escapeHtml(p.id) }),
      confirmLabel: t("common.delete"),
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteProfile(p.id);
      store.activeModelId = null;
      store.activeManId = null;
      await deps.refresh();
      toast(t("toast.profileDeleted"), "success");
    } catch (error) {
      toast(errorText(error), "error");
    }
  });
}

// ---------------------------------------------------------------------------
// Man dossier
// ---------------------------------------------------------------------------

/**
 * Facts as the operator typed them: one `key: value` per line.
 *
 * A line that still matches a stored fact keeps that fact whole — its id, who
 * wrote it, when — and only what actually changed becomes a new entry.
 */
function editedFacts(text: string | undefined, man: Man): Fact[] {
  if (text === undefined) return man.facts;
  const seen = new Set<string>();
  return lines(text).map((line) => {
    const at = line.indexOf(":");
    const key = at === -1 ? "note" : line.slice(0, at).trim();
    const value = at === -1 ? line.trim() : line.slice(at + 1).trim();
    const kept = man.facts.find(
      (fact) => !seen.has(fact.id) && fact.key === key && fact.value === value,
    );
    if (kept) {
      seen.add(kept.id);
      return kept;
    }
    return {
      id: crypto.randomUUID(),
      key,
      value,
      source: "operator",
      created_at: new Date().toISOString(),
    };
  });
}

/** Notes the same way: one per line, untouched ones kept as they are. */
function editedNotes(text: string | undefined, man: Man): Note[] {
  if (text === undefined) return man.notes;
  const seen = new Set<string>();
  return lines(text).map((line) => {
    const kept = man.notes.find((note) => !seen.has(note.id) && note.text.trim() === line);
    if (kept) {
      seen.add(kept.id);
      return kept;
    }
    return {
      id: crypto.randomUUID(),
      text: line,
      author: "operator",
      created_at: new Date().toISOString(),
    };
  });
}

/**
 * The correspondence, editable message by message.
 *
 * This record is what every prompt is built from, so a message pasted under the
 * wrong role, a line that belongs to another man, or a typo his letter never
 * had all steer the model until someone fixes them. Each message keeps its own
 * row: who wrote it, over which channel, and the text; rows can be added and
 * removed, and what is left is written back in the order shown.
 */
export async function openChatEditor(deps: ModalDeps, man: Man) {
  let messages: ChatMessage[] = [];
  try {
    messages = (await api.getChat(man.model_id, man.id)).messages;
  } catch (error) {
    toast(errorText(error), "error");
    return;
  }

  const card = openModal(`
    <h3>${t("chat.editTitle", { name: escapeHtml(man.name) })}</h3>
    <div class="modal-sub">${t("chat.editSub")}</div>
    <div class="msg-rows" id="msgRows"></div>
    <button class="btn btn-secondary" id="btnAddMessage" type="button">${t("chat.addMessage")}</button>
    <div class="modal-actions">
      <button class="btn btn-secondary" data-act="close">${t("common.cancel")}</button>
      <button class="btn btn-primary" id="btnSaveChat">${t("common.save")}</button>
    </div>`);

  const rows = card.querySelector<HTMLElement>("#msgRows")!;

  const draw = () => {
    rows.innerHTML = messages
      .map(
        (message, index) => `
        <div class="msg-row" data-index="${index}">
          <div class="msg-row-head">
            <select class="field-input compact" data-field="role">
              <option value="incoming"${message.role === "incoming" ? " selected" : ""}>${t("chat.roleIncoming")}</option>
              <option value="outgoing"${message.role === "outgoing" ? " selected" : ""}>${t("chat.roleOutgoing")}</option>
              <option value="note"${message.role === "note" ? " selected" : ""}>${t("chat.roleNote")}</option>
            </select>
            <select class="field-input compact" data-field="channel">
              <option value="chat"${message.channel === "chat" ? " selected" : ""}>${t("composer.chat")}</option>
              <option value="letter"${message.channel === "letter" ? " selected" : ""}>${t("composer.letter")}</option>
              <option value="note"${message.channel === "note" ? " selected" : ""}>${t("chat.roleNote")}</option>
            </select>
            <span class="msg-row-date">${formatDate(message.ts)}</span>
            <button class="btn btn-icon" data-act="drop" title="${t("common.delete")}">×</button>
          </div>
          <textarea class="field-area" data-field="text">${escapeHtml(message.text)}</textarea>
        </div>`,
      )
      .join("");
    // The rows appear after the dialog did, so their dropdowns are dressed here.
    dressSelectsIn(rows);
  };

  /** Read the rows back before anything that redraws them. */
  const collect = () => {
    messages = Array.from(rows.querySelectorAll<HTMLElement>(".msg-row")).map((row) => {
      const index = Number(row.dataset.index);
      const previous = messages[index];
      return {
        ...previous,
        role: row.querySelector<HTMLSelectElement>('[data-field="role"]')!.value as MsgRole,
        channel: row.querySelector<HTMLSelectElement>('[data-field="channel"]')!.value as Channel,
        text: row.querySelector<HTMLTextAreaElement>('[data-field="text"]')!.value,
      };
    });
  };

  draw();

  rows.addEventListener("click", (event) => {
    const drop = (event.target as HTMLElement).closest<HTMLElement>('[data-act="drop"]');
    if (!drop) return;
    const index = Number(drop.closest<HTMLElement>(".msg-row")?.dataset.index);
    collect();
    messages.splice(index, 1);
    draw();
  });

  card.querySelector<HTMLButtonElement>("#btnAddMessage")?.addEventListener("click", () => {
    collect();
    messages.push({
      id: crypto.randomUUID(),
      role: "incoming",
      channel: "chat",
      text: "",
      ts: new Date().toISOString(),
    });
    draw();
  });

  card.querySelector<HTMLButtonElement>("#btnSaveChat")?.addEventListener("click", async () => {
    collect();
    // An empty row is a row the operator meant to throw away.
    const kept = messages.filter((message) => message.text.trim());
    try {
      await api.saveChat(man.model_id, man.id, kept);
      toast(t("toast.chatSaved"), "success");
      closeModal();
      await deps.refresh();
    } catch (error) {
      toast(errorText(error), "error");
    }
  });
}

export async function openManForm(deps: ModalDeps, existing?: Man | null) {
  const m = existing ?? null;
  if (!m && !store.activeModelId) {
    toast(t("toast.pickProfile"), "error");
    return;
  }

  let transcript = "";
  if (m) {
    try {
      const thread = await api.getChat(m.model_id, m.id);
      transcript = thread.messages
        .slice(-40)
        .map(
          (msg) =>
            `${msg.role === "incoming" ? t("chat.him") : msg.role === "outgoing" ? t("chat.her") : "•"}: ${msg.text}`,
        )
        .join("\n\n");
    } catch {
      /* dossier without history */
    }
  }

  const stages = ["new", "warming", "attached", "dating", "cooled"];

  const card = openModal(`
    <h3>${m ? `${escapeHtml(m.name)} · ID ${escapeHtml(m.id)}` : t("man.newTitle")}</h3>
    <div class="modal-sub">${
      m
        ? t("man.editSub", { model: escapeHtml(m.model_id), id: escapeHtml(m.id) })
        : t("man.newSub")
    }</div>

    ${avatarField(m?.avatar ?? "")}

    <div class="field-grid">
      <div class="field"><label>${t("man.name")}</label>
        <input class="field-input" id="mName" value="${escapeHtml(m?.name ?? "")}" placeholder="Hartwig Buesing" /></div>
      <div class="field"><label>${t("profile.age")}</label>
        <input class="field-input" id="mAge" type="number" min="18" max="99" value="${m?.age ?? ""}" placeholder="65" /></div>
      <div class="field"><label>${t("man.location")}</label>
        <input class="field-input" id="mLoc" value="${escapeHtml(m?.location ?? "")}" placeholder="Bückeburg, Germany" /></div>
      <div class="field"><label>${t("man.siteId")}</label>
        <input class="field-input" id="mId" value="${escapeHtml(m?.id ?? "")}" placeholder="${t("man.idHint")}" ${
          m ? "disabled" : ""
        } /></div>
    </div>

    <div class="field-grid">
      <div class="field"><label>${t("man.stage")}</label>
        <select class="field-input" id="mStage">
          ${stages
            .map(
              (s) =>
                `<option value="${s}" ${s === (m?.stage ?? "new") ? "selected" : ""}>${s}</option>`,
            )
            .join("")}
        </select>
      </div>
      <div class="field"><label>${t("man.tags")}</label>
        <input class="field-input" id="mTags" value="${escapeHtml((m?.tags ?? []).join(", "))}" placeholder="${t("man.tagsHint")}" /></div>
    </div>

    <div class="field"><label>${t("man.status")}</label>
      <input class="field-input" id="mStatus" value="${escapeHtml(m?.status ?? "")}" placeholder="${t("man.statusHint")}" /></div>
    <div class="field"><label>${t("man.next")}</label>
      <input class="field-input" id="mNext" value="${escapeHtml(m?.next_action ?? "")}" placeholder="${t("man.nextHint")}" /></div>

    <details class="advanced">
      <summary>${t("man.memorySection", { memory: m ? t("man.memorySuffix") : "" })}</summary>
      <div class="field"><label>${t("man.triggers")}</label>
        <textarea class="field-area" id="mTriggers" placeholder="${t("man.triggersHint")}">${escapeHtml(
          (m?.triggers ?? []).join("\n"),
        )}</textarea></div>
      <div class="field"><label>${t("man.boundaries")}</label>
        <textarea class="field-area" id="mBounds" placeholder="${t("man.boundariesHint")}">${escapeHtml(
          (m?.boundaries ?? []).join("\n"),
        )}</textarea></div>
      ${
        m
          ? `<div class="field"><label>${t("man.facts", { n: m.facts.length })}</label>
               <textarea class="field-area tall" id="mFacts" placeholder="${t("man.factsHint")}">${escapeHtml(
                 m.facts.map((f) => `${f.key}: ${f.value}`).join("\n"),
               )}</textarea></div>
             <div class="field"><label>${t("man.notes", { n: m.notes.length })}</label>
               <textarea class="field-area tall" id="mNotes" placeholder="${t("man.notesHint")}">${escapeHtml(
                 m.notes.map((n) => n.text.replace(/\s*\n\s*/g, " ")).join("\n"),
               )}</textarea></div>
             <div class="field"><label>${t("man.gifts", { n: m.gifts.length })}</label>
               <div class="code-block">${
                 m.gifts.length
                   ? m.gifts
                       .map(
                         (g) =>
                           `${formatDate(g.date)} — ${escapeHtml(g.title)}${g.value ? ` (${g.value})` : ""}`,
                       )
                       .join("\n")
                   : t("common.empty")
               }</div></div>
             <div class="field"><label>${t("man.chat")}</label>
               <div class="code-block">${transcript ? escapeHtml(transcript) : t("common.empty")}</div>
               <button class="btn btn-secondary" id="btnEditChat" type="button">${t("man.editChat")}</button>
             </div>`
          : ""
      }
    </details>

    <div class="modal-actions">
      ${m ? `<button class="btn btn-danger" id="btnDeleteMan">${t("man.delete")}</button>` : ""}
      <button class="btn btn-secondary" data-act="close">${t("common.cancel")}</button>
      <button class="btn btn-primary" id="btnSubmitMan">${m ? t("common.save") : t("man.create")}</button>
    </div>`);

  bindAvatarField(card);
  card.querySelector<HTMLInputElement>("#mName")?.focus();
  card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);

  card.querySelector<HTMLButtonElement>("#btnSubmitMan")?.addEventListener("click", async () => {
    const form = {
      name: card.querySelector<HTMLInputElement>("#mName")!.value.trim(),
      age: Number(card.querySelector<HTMLInputElement>("#mAge")!.value) || null,
      location: card.querySelector<HTMLInputElement>("#mLoc")!.value.trim(),
      id: card.querySelector<HTMLInputElement>("#mId")!.value.trim(),
      stage: card.querySelector<HTMLSelectElement>("#mStage")!.value,
      tags: csv(card.querySelector<HTMLInputElement>("#mTags")!.value),
      status: card.querySelector<HTMLInputElement>("#mStatus")!.value.trim(),
      next_action: card.querySelector<HTMLInputElement>("#mNext")!.value.trim(),
      avatar: card.querySelector<HTMLInputElement>("#avatarValue")!.value.trim(),
      triggers: lines(card.querySelector<HTMLTextAreaElement>("#mTriggers")!.value),
      boundaries: lines(card.querySelector<HTMLTextAreaElement>("#mBounds")!.value),
    };
    if (!form.name) {
      toast(t("form.nameRequired"), "error");
      return;
    }
    try {
      if (m) {
        // The dossier's memory is the operator's too: edited lines replace what
        // was there, while every line left alone keeps the id and the date it
        // was written with, so nothing looks freshly remembered after a typo
        // fix somewhere else.
        const facts = editedFacts(card.querySelector<HTMLTextAreaElement>("#mFacts")?.value, m);
        const notes = editedNotes(card.querySelector<HTMLTextAreaElement>("#mNotes")?.value, m);
        await api.saveMan({ ...m, ...form, facts, notes, id: m.id, age: form.age });
        toast(t("toast.manSaved"), "success");
        await deps.refresh();
      } else {
        const created = await api.createMan(store.activeModelId!, {
          name: form.name,
          id: form.id || undefined,
          age: form.age ?? undefined,
          location: form.location,
          tags: form.tags,
          stage: form.stage,
          status: form.status,
          next_action: form.next_action,
          avatar: form.avatar,
          triggers: form.triggers,
          boundaries: form.boundaries,
        });
        await deps.refresh();
        await deps.selectMan(created.id);
        toast(t("toast.manCreated", { name: created.name }), "success");
      }
      closeModal();
    } catch (error) {
      toast(errorText(error), "error");
    }
  });

  card.querySelector<HTMLButtonElement>("#btnEditChat")?.addEventListener("click", () => {
    if (m) void openChatEditor(deps, m);
  });

  card.querySelector<HTMLButtonElement>("#btnDeleteMan")?.addEventListener("click", async () => {
    if (!m) return;
    const ok = await confirmDialog({
      title: t("man.deleteTitle"),
      body: t("man.deleteBody", { name: escapeHtml(m.name) }),
      confirmLabel: t("common.delete"),
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteMan(m.model_id, m.id);
      await deps.selectMan(null);
      await deps.refresh();
      toast(t("toast.manDeleted"), "success");
    } catch (error) {
      toast(errorText(error), "error");
    }
  });
}
