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
  return date.toLocaleString("ru-RU", {
    day: "2-digit",
    month: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function toast(message: string, kind: "info" | "success" | "error" = "info", ms = 4200) {
  const stack = document.getElementById("toastStack");
  if (!stack) return;
  const node = document.createElement("div");
  node.className = `toast ${kind}`;
  node.textContent = message;
  stack.appendChild(node);
  setTimeout(() => node.remove(), ms);
}

let closeHandler: (() => void) | null = null;

export function openModal(html: string, onClose?: () => void) {
  const overlay = $("modalOverlay");
  const card = $("modalCard");
  card.innerHTML = html;
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
  overlay.addEventListener("click", (event) => {
    if (event.target === overlay) closeModal();
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
         <button class="btn-send secondary" data-act="cancel">Отмена</button>
         <button class="btn-send ${options.danger ? "danger" : ""}" data-act="ok">${escapeHtml(
           options.confirmLabel ?? "Подтвердить",
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
         <button class="btn-send secondary" data-act="cancel">Отмена</button>
         <button class="btn-send" data-act="ok">Ок</button>
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
