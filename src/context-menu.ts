/**
 * Right-click menus.
 *
 * A webview gives no useful native menu of its own, so the app draws one. It
 * is deliberately plain: a flat list of actions for whatever was right-clicked,
 * always including the ones an operator reaches for constantly — copy a name,
 * copy an id, copy the drafted reply.
 */
import { t } from "./i18n";

export interface MenuAction {
  label: string;
  /** Shown greyed out and not clickable. */
  disabled?: boolean;
  danger?: boolean;
  onSelect: () => void | Promise<void>;
}

export type MenuEntry = MenuAction | "separator";

let open: HTMLElement | null = null;

export function closeContextMenu() {
  open?.remove();
  open = null;
}

/** Copy text, falling back to a hidden textarea where the async API is absent. */
export async function copyText(text: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    // Older webviews refuse the async clipboard outside a secure context.
  }
  const helper = document.createElement("textarea");
  helper.value = text;
  helper.setAttribute("readonly", "");
  helper.style.cssText = "position:fixed;top:-1000px;opacity:0";
  document.body.appendChild(helper);
  helper.select();
  try {
    document.execCommand("copy");
  } finally {
    helper.remove();
  }
}

/** Draw the menu at a point, keeping it inside the window. */
export function openContextMenu(x: number, y: number, entries: MenuEntry[]) {
  closeContextMenu();
  const actions = entries.filter((e): e is MenuAction => e !== "separator");
  if (actions.length === 0) return;

  const menu = document.createElement("div");
  menu.className = "ctx-menu";
  for (const entry of entries) {
    if (entry === "separator") {
      const line = document.createElement("div");
      line.className = "ctx-separator";
      menu.appendChild(line);
      continue;
    }
    const item = document.createElement("button");
    item.className = `ctx-item${entry.danger ? " danger" : ""}`;
    item.textContent = entry.label;
    item.disabled = entry.disabled === true;
    item.addEventListener("click", () => {
      closeContextMenu();
      void entry.onSelect();
    });
    menu.appendChild(item);
  }

  // Measured off-screen first: the size depends on the labels.
  menu.style.visibility = "hidden";
  document.body.appendChild(menu);
  const { width, height } = menu.getBoundingClientRect();
  const left = Math.min(x, window.innerWidth - width - 8);
  const top = Math.min(y, window.innerHeight - height - 8);
  menu.style.left = `${Math.max(8, left)}px`;
  menu.style.top = `${Math.max(8, top)}px`;
  menu.style.visibility = "visible";
  open = menu;
}

/** The selected text, when the selection is inside `element`. */
export function selectionWithin(element: Element): string {
  const selection = window.getSelection();
  if (!selection || selection.isCollapsed || selection.rangeCount === 0) return "";
  const range = selection.getRangeAt(0);
  return element.contains(range.commonAncestorContainer) ? selection.toString().trim() : "";
}

/** Cut / copy / paste / select all for a text field. */
export function editingEntries(field: HTMLInputElement | HTMLTextAreaElement): MenuEntry[] {
  const selected = field.value.slice(field.selectionStart ?? 0, field.selectionEnd ?? 0);
  const replaceSelection = (text: string) => {
    const start = field.selectionStart ?? field.value.length;
    const end = field.selectionEnd ?? field.value.length;
    field.value = `${field.value.slice(0, start)}${text}${field.value.slice(end)}`;
    const caret = start + text.length;
    field.setSelectionRange(caret, caret);
    field.dispatchEvent(new Event("input", { bubbles: true }));
  };

  return [
    {
      label: t("ctx.cut"),
      disabled: !selected || field.readOnly,
      onSelect: async () => {
        await copyText(selected);
        replaceSelection("");
      },
    },
    { label: t("ctx.copy"), disabled: !selected, onSelect: () => copyText(selected) },
    {
      label: t("ctx.paste"),
      disabled: field.readOnly,
      onSelect: async () => {
        try {
          replaceSelection(await navigator.clipboard.readText());
        } catch {
          field.focus();
          document.execCommand("paste");
        }
      },
    },
    "separator",
    {
      label: t("ctx.selectAll"),
      onSelect: () => {
        field.focus();
        field.select();
      },
    },
  ];
}

// The menu is transient: anything that moves the page under it closes it.
document.addEventListener("pointerdown", (event) => {
  if (open && !open.contains(event.target as Node)) closeContextMenu();
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closeContextMenu();
});
window.addEventListener("blur", closeContextMenu);
window.addEventListener("resize", closeContextMenu);
document.addEventListener("scroll", closeContextMenu, true);
