import { closeContextMenu, isContextMenuOpen, openContextMenu, type MenuEntry } from "./context-menu";

/**
 * The app's own dropdowns, over the browser's.
 *
 * A native `<select>` opens a popup the page has no say in: it is drawn by the
 * platform, ignores the theme, and in the desktop webview it leaves an empty
 * panel hanging next to the list. The `<select>` stays in the markup — it is
 * still what the rest of the app reads and writes — but it is hidden behind a
 * button that opens the same menu the rest of the interface uses.
 */
const dressed = new Map<HTMLSelectElement, HTMLButtonElement>();

/** Whose menu is on screen, so pressing the same button again closes it. */
let showing: HTMLSelectElement | null = null;
/** The button whose menu the pointer press has just dismissed. */
let dismissed: HTMLSelectElement | null = null;

export function dressSelect(select: HTMLSelectElement) {
  if (dressed.has(select)) return;
  // Modals are rebuilt every time they open; their old controls are gone.
  for (const known of [...dressed.keys()]) {
    if (!known.isConnected) dressed.delete(known);
  }

  const button = document.createElement("button");
  button.type = "button";
  button.className = "btn btn-secondary select-button";
  const title = select.getAttribute("title");
  if (title) button.title = title;
  // The label is read from the option, which is where translation lands.
  select.after(button);
  select.classList.add("dressed");
  dressed.set(select, button);

  // A press anywhere closes the open menu, this button included; without
  // noticing that, the click right after it would open the menu straight back
  // up and pressing the button twice would never close anything.
  button.addEventListener("pointerdown", () => {
    dismissed = isContextMenuOpen() && showing === select ? select : null;
  });

  button.addEventListener("click", () => {
    if (dismissed === select) {
      dismissed = null;
      showing = null;
      closeContextMenu();
      return;
    }
    const entries: MenuEntry[] = Array.from(select.options).map((option) => ({
      label: `${option.value === select.value ? "✓ " : ""}${option.textContent ?? option.value}`,
      onSelect: () => {
        if (option.value === select.value) return;
        select.value = option.value;
        select.dispatchEvent(new Event("change", { bubbles: true }));
        syncSelect(select);
        showing = null;
      },
    }));
    // Above the button: these controls sit on the last row of the window.
    const box = button.getBoundingClientRect();
    openContextMenu(box.left, Math.max(8, box.top - 8 - select.options.length * 30), entries);
    showing = select;
  });

  select.addEventListener("change", () => syncSelect(select));
  syncSelect(select);
}

/**
 * A field that takes any text and offers a list.
 *
 * The browser's own datalist popup has the same trouble as its select: it is
 * drawn outside the page, ignores the theme and leaves an empty panel behind
 * it. The field stays an ordinary input — anything can be typed into it — and
 * the caret beside it opens the app's own menu of what is usually typed.
 */
export function dressCombo(input: HTMLInputElement, options: readonly string[]) {
  input.removeAttribute("list");
  input.autocomplete = "off";

  const wrap = document.createElement("span");
  wrap.className = "combo";
  input.after(wrap);
  wrap.appendChild(input);

  const caret = document.createElement("button");
  caret.type = "button";
  caret.className = "combo-caret";
  caret.tabIndex = -1;
  caret.textContent = "▾";
  wrap.appendChild(caret);

  let comboOpen = false;
  caret.addEventListener("pointerdown", () => {
    comboOpen = isContextMenuOpen();
  });
  caret.addEventListener("click", () => {
    if (comboOpen) {
      comboOpen = false;
      closeContextMenu();
      return;
    }
    const entries: MenuEntry[] = options.map((option) => ({
      label: `${option === input.value ? "✓ " : ""}${option}`,
      onSelect: () => {
        input.value = option;
        input.dispatchEvent(new Event("input", { bubbles: true }));
        input.dispatchEvent(new Event("change", { bubbles: true }));
      },
    }));
    const box = wrap.getBoundingClientRect();
    openContextMenu(box.left, box.bottom + 4, entries);
  });
}

/** Put the chosen option's words on the button. */
function syncSelect(select: HTMLSelectElement) {
  const button = dressed.get(select);
  if (!button) return;
  const chosen = select.options[select.selectedIndex];
  button.textContent = chosen?.textContent?.trim() || select.value;
  const caret = document.createElement("span");
  caret.className = "select-caret";
  caret.textContent = "▾";
  button.appendChild(caret);
}

/** Dress every select under a node — rows built after a modal was opened. */
export function dressSelectsIn(root: ParentNode) {
  for (const select of root.querySelectorAll<HTMLSelectElement>("select:not(.dressed)")) {
    dressSelect(select);
  }
}

/** After a language switch every label has changed underneath. */
export function syncDressedSelects() {
  for (const select of dressed.keys()) syncSelect(select);
}
