import { dressSelectsIn } from "./dropdown";
import { lang, t } from "./i18n";
export function $<T extends HTMLElement = HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el as T;
}

export function escapeHtml(value: unknown): string {
  return String(value ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function initials(name: string): string {
  const parts = name.trim().split(/\s+/).filter(Boolean);
  if (parts.length === 0) return "?";
  if (parts.length === 1) return parts[0].slice(0, 2).toUpperCase();
  return (parts[0][0] + parts[1][0]).toUpperCase();
}

export function avatarHtml(name: string, url: string, extraClass = ""): string {
  if (url) {
    return `<img class="avatar ${extraClass}" src="${escapeHtml(url)}" alt="" onerror="this.replaceWith(Object.assign(document.createElement('div'),{className:'avatar ${extraClass}',textContent:'${escapeHtml(
      initials(name),
    )}'}))" />`;
  }
  return `<div class="avatar ${extraClass}">${escapeHtml(initials(name))}</div>`;
}

export function formatDate(value: string | null | undefined): string {
  if (!value) return "—";
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return "—";
  return date.toLocaleString(lang() === "en" ? "en-GB" : "ru-RU", {
    day: "2-digit",
    month: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/**
 * A short-lived notice.
 *
 * A long one used to vanish in the same four seconds as "Saved", which is not
 * enough time to read a sentence — so it stays on screen for as long as it
 * takes to read it, and a click dismisses it early.
 */
export function toast(message: string, kind: "info" | "success" | "error" = "info", ms = 0) {
  const stack = document.getElementById("toastStack");
  if (!stack) return;
  const node = document.createElement("div");
  node.className = `toast ${kind}`;
  node.textContent = message;
  node.title = message;
  stack.appendChild(node);

  // Roughly a comfortable reading pace, floored at a glance and capped so a
  // wall of text does not sit there forever.
  const life = ms || Math.min(18_000, 3_400 + message.length * 55);
  const timer = window.setTimeout(() => node.remove(), life);
  node.addEventListener("click", () => {
    window.clearTimeout(timer);
    node.remove();
  });
}

/**
 * A notice that stays until it is dealt with.
 *
 * An ordinary toast is for things that already happened; this is for something
 * waiting — a summary written while the operator was elsewhere. It sits in the
 * corner until they open it or dismiss it.
 */
export function notify(message: string, onOpen: () => void) {
  const stack = document.getElementById("toastStack");
  if (!stack) return;
  const node = document.createElement("div");
  node.className = "toast info sticky";

  const body = document.createElement("button");
  body.type = "button";
  body.className = "toast-open";
  body.textContent = message;
  body.addEventListener("click", () => {
    node.remove();
    onOpen();
  });

  const close = document.createElement("button");
  close.type = "button";
  close.className = "toast-close";
  close.textContent = "×";
  close.addEventListener("click", () => node.remove());

  node.append(body, close);
  stack.appendChild(node);
}

let closeHandler: (() => void) | null = null;

export function openModal(html: string, onClose?: () => void) {
  const overlay = $("modalOverlay");
  const card = $("modalCard");
  card.innerHTML = html;

  // The webview offers to remember and refill these fields, and draws that
  // offer as an oversized panel over the form. Nothing here is a login or an
  // address, so the offer is refused outright.
  for (const field of card.querySelectorAll<HTMLElement>("input, textarea")) {
    field.setAttribute("autocomplete", "off");
    field.setAttribute("autocorrect", "off");
    field.setAttribute("data-lpignore", "true");
  }
  // Its dropdowns are drawn outside the page too, so they are ours as well.
  dressSelectsIn(card);
  overlay.classList.add("open");
  closeHandler = onClose ?? null;
  return card;
}

export function closeModal() {
  $("modalOverlay").classList.remove("open");
  $("modalCard").innerHTML = "";
  if (closeHandler) {
    const fn = closeHandler;
    closeHandler = null;
    fn();
  }
}

export function bindModalDismiss() {
  const overlay = $("modalOverlay");

  // Selecting text in a dialog often ends with the mouse outside it, and a
  // click event fires on the nearest common ancestor — the backdrop — which
  // used to throw the dialog away mid-selection. A dismissal has to both start
  // and end on the backdrop.
  let pressedBackdrop = false;
  overlay.addEventListener("pointerdown", (event) => {
    pressedBackdrop = event.target === overlay;
  });
  overlay.addEventListener("click", (event) => {
    if (event.target === overlay && pressedBackdrop) closeModal();
    pressedBackdrop = false;
  });
  document.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && overlay.classList.contains("open")) closeModal();
  });
}

/** Confirm dialog rendered in the app's own style (no native alerts). */
export function confirmDialog(options: {
  title: string;
  body: string;
  confirmLabel?: string;
  danger?: boolean;
}): Promise<boolean> {
  return new Promise((resolve) => {
    let settled = false;
    const done = (value: boolean) => {
      if (settled) return;
      settled = true;
      resolve(value);
    };
    const card = openModal(
      `<h3>${escapeHtml(options.title)}</h3>
       <div class="modal-sub">${options.body}</div>
       <div class="modal-actions">
         <button class="btn btn-secondary" data-act="cancel">${t("common.cancel")}</button>
         <button class="btn ${options.danger ? "btn-danger" : "btn-primary"}" data-act="ok">${escapeHtml(
           options.confirmLabel ?? t("common.confirm"),
         )}</button>
       </div>`,
      () => done(false),
    );
    card.querySelector<HTMLButtonElement>('[data-act="cancel"]')?.addEventListener("click", () => {
      done(false);
      closeModal();
    });
    card.querySelector<HTMLButtonElement>('[data-act="ok"]')?.addEventListener("click", () => {
      done(true);
      closeModal();
    });
  });
}

export function promptDialog(options: {
  title: string;
  label: string;
  placeholder?: string;
  value?: string;
  multiline?: boolean;
}): Promise<string | null> {
  return new Promise((resolve) => {
    let settled = false;
    const done = (value: string | null) => {
      if (settled) return;
      settled = true;
      resolve(value);
    };
    const input = options.multiline
      ? `<textarea class="field-area" id="promptField" placeholder="${escapeHtml(
          options.placeholder ?? "",
        )}">${escapeHtml(options.value ?? "")}</textarea>`
      : `<input class="field-input" id="promptField" placeholder="${escapeHtml(
          options.placeholder ?? "",
        )}" value="${escapeHtml(options.value ?? "")}" />`;

    const card = openModal(
      `<h3>${escapeHtml(options.title)}</h3>
       <div class="field"><label>${escapeHtml(options.label)}</label>${input}</div>
       <div class="modal-actions">
         <button class="btn btn-secondary" data-act="cancel">${t("common.cancel")}</button>
         <button class="btn btn-primary" data-act="ok">${t("common.ok")}</button>
       </div>`,
      () => done(null),
    );
    const field = card.querySelector<HTMLInputElement | HTMLTextAreaElement>("#promptField");
    field?.focus();
    const submit = () => {
      const value = field?.value.trim() ?? "";
      done(value.length ? value : null);
      closeModal();
    };
    card.querySelector<HTMLButtonElement>('[data-act="cancel"]')?.addEventListener("click", () => {
      done(null);
      closeModal();
    });
    card.querySelector<HTMLButtonElement>('[data-act="ok"]')?.addEventListener("click", submit);
    field?.addEventListener("keydown", (raw) => {
      const event = raw as KeyboardEvent;
      if (event.key === "Enter" && (!options.multiline || event.ctrlKey)) submit();
    });
  });
}
