/**
 * The guided tour.
 *
 * A first launch opens it, and the button in the top bar opens it again. Every
 * step darkens the whole window except the one control it is talking about, so
 * the explanation is attached to the thing itself rather than to a paragraph
 * about it — which is the difference between reading what AUTO means and
 * seeing which button it is.
 *
 * The first step asks for a language, and it is not a separate setting: it is
 * the app's own switch, so answering it turns the whole interface over.
 *
 * A phone shows one pane at a time and folds the icons into a menu, so the
 * same tour there is a different walk: it points at the tab to press, waits
 * for the press, and only then explains what opened. Controls that live in the
 * menu on a phone are pointed at through the menu button.
 */
import { escapeHtml, isPhoneLayout, showPane } from "./dom";
import { lang, setLang, t, type Lang } from "./i18n";

export interface TourDeps {
  /** The app's language switch — the tour has none of its own. */
  setLanguage(next: Lang): void;
  /** Called once the tour is over, however it ended. */
  onDone(): void;
}

interface Step {
  /**
   * Which control this step is about, as a list: the first one actually on
   * screen is used, so one step can name the button on a wide window and the
   * dropdown or the menu that stands in for it on a narrow one. Centred on the
   * screen when none is there.
   */
  target?: string[];
  /** Dictionary key of the wording: `<key>.title` and `<key>.body`. */
  key: string;
  /** The language picker belongs to the first step only. */
  languages?: boolean;
  /** A phone-only step: points at a tab and waits for it to be pressed. */
  tap?: "paneProfiles" | "paneChat" | "paneMen";
  /** Shown only when the controls are folded into the menu. */
  phoneOnly?: boolean;
}

const MENU = "#btnMenu";
const tab = (pane: string) => `.tab-btn[data-pane="${pane}"]`;

const ALL_STEPS: Step[] = [
  { key: "tour.welcome", languages: true },
  { target: [MENU], key: "tour.menu", phoneOnly: true },
  { target: ["#providerChip", MENU], key: "tour.provider" },
  { target: ["#btnKeys", MENU], key: "tour.keys" },
  { target: [tab("paneProfiles")], key: "tour.tapProfiles", tap: "paneProfiles" },
  { target: ["#paneProfiles"], key: "tour.profiles" },
  { target: [tab("paneMen")], key: "tour.tapMen", tap: "paneMen" },
  { target: ["#paneMen"], key: "tour.men" },
  { target: [tab("paneChat")], key: "tour.tapChat", tap: "paneChat" },
  { target: ["#messages"], key: "tour.chat" },
  { target: ["#composerInput"], key: "tour.composer" },
  { target: [".composer-left"], key: "tour.tools" },
  { target: ['#modeControl [data-mode="auto"]', "#modeSelect + .select-button"], key: "tour.auto" },
  { target: ['#modeControl [data-mode="act"]', "#modeSelect + .select-button"], key: "tour.act" },
  { target: ['#modeControl [data-mode="memorize"]', "#modeSelect + .select-button"], key: "tour.memorize" },
  { target: ['#modeControl [data-mode="letters"]', "#modeSelect + .select-button"], key: "tour.letters" },
  { target: ["#securityControl", "#securitySelect + .select-button"], key: "tour.security" },
  { target: ["#btnPending", MENU], key: "tour.pending" },
  { target: ["#btnMaster", MENU], key: "tour.master" },
  { target: ["#btnTemporary", MENU], key: "tour.temporary" },
  { target: ["#btnDoctor", MENU], key: "tour.doctor" },
  { target: ["#btnGuide", MENU], key: "tour.again" },
];

/** The walk for the window as it is laid out when the tour opens. */
let STEPS: Step[] = ALL_STEPS;

let at = 0;
let deps: TourDeps | null = null;
let overlay: HTMLElement | null = null;

/** Open the tour at the first step. */
export function startTour(next: TourDeps) {
  deps = next;
  at = 0;
  const phone = isPhoneLayout();
  // Tabs are pressed only where there are tabs; the menu is introduced only
  // where there is a menu.
  STEPS = ALL_STEPS.filter((each) => (each.tap || each.phoneOnly ? phone : true));
  if (phone) showPane("paneChat");
  if (!overlay) {
    overlay = document.createElement("div");
    overlay.className = "tour";
    overlay.id = "tour";
    document.body.appendChild(overlay);
    overlay.addEventListener("click", onClick);
    window.addEventListener("resize", place);
    window.addEventListener("keydown", onKey, true);
    // Capturing, so it hears the press before the tab does — and before
    // anything else on the page, which a waiting step must not let through.
    window.addEventListener("click", onWaitingTap, true);
  }
  overlay.hidden = false;
  draw();
}

/** Whether the tour is on screen. */
export function tourRunning(): boolean {
  return Boolean(overlay && !overlay.hidden);
}

function close() {
  if (overlay) {
    overlay.hidden = true;
    overlay.classList.remove("tour-tap");
  }
  deps?.onDone();
}

/**
 * While a step waits for a tab to be pressed, only that tab and the tour's own
 * card answer. The press on the tab goes through — it is what opens the pane —
 * and the tour moves on once it has.
 */
function onWaitingTap(event: MouseEvent) {
  if (!tourRunning() || !STEPS[at]?.tap) return;
  const hit = event.target as HTMLElement;
  if (document.getElementById("tourCard")?.contains(hit)) return;
  const wanted = target();
  if (wanted?.contains(hit)) {
    window.setTimeout(() => step(1), 0);
    return;
  }
  event.preventDefault();
  event.stopPropagation();
}

function onKey(event: KeyboardEvent) {
  if (!tourRunning()) return;
  if (event.key === "Escape") {
    event.preventDefault();
    close();
    return;
  }
  if (event.key === "ArrowRight" || event.key === "Enter") {
    event.preventDefault();
    step(1);
  }
  if (event.key === "ArrowLeft") {
    event.preventDefault();
    step(-1);
  }
}

function onClick(event: MouseEvent) {
  const button = (event.target as HTMLElement).closest<HTMLElement>("[data-tour]");
  if (!button) return;
  const act = button.dataset.tour;
  if (act === "skip") return close();
  if (act === "next") return step(1);
  if (act === "back") return step(-1);
  if (act === "lang") {
    const picked = (button.dataset.lang ?? "ru") as Lang;
    // The app's own switch, not a setting of the tour: the whole interface
    // turns over, and the tour is redrawn in the language it was told.
    deps?.setLanguage(picked);
    setLang(picked);
    draw();
  }
}

function step(by: number) {
  let next = at + by;
  // Going back over a tap is not asking to press the tab again: the pane the
  // step before it explains is brought forward by `draw` in any case.
  while (by < 0 && next > 0 && STEPS[next]?.tap) next -= 1;
  if (next < 0) return;
  if (next >= STEPS.length) return close();
  at = next;
  draw();
}

function onScreen(element: HTMLElement): boolean {
  const box = element.getBoundingClientRect();
  return box.width > 0 && box.height > 0;
}

/** The element this step is about, when it is on screen. */
function target(): HTMLElement | null {
  const selectors = STEPS[at].target;
  if (!selectors) return null;
  for (const selector of selectors) {
    const element = document.querySelector<HTMLElement>(selector);
    if (element && onScreen(element)) return element;
  }
  return null;
}

/**
 * On a phone, the pane a step is about has to be the one showing. A tap step
 * gets there by the operator's own press; stepping back, or a step whose
 * control is inside a pane, gets there here.
 */
function bringForward(current: Step) {
  if (!isPhoneLayout() || current.tap || !current.target) return;
  for (const selector of current.target) {
    const element = document.querySelector<HTMLElement>(selector);
    const pane = element?.closest<HTMLElement>("#paneProfiles, #paneChat, #paneMen");
    if (!pane) continue;
    if (!pane.classList.contains("pane-active")) {
      showPane(pane.id as "paneProfiles" | "paneChat" | "paneMen");
    }
    return;
  }
}

function draw() {
  if (!overlay) return;
  const step = STEPS[at];
  bringForward(step);
  // A waiting step lets the press through to the tab, and has no "next": the
  // press is the next.
  overlay.classList.toggle("tour-tap", Boolean(step.tap));
  const languages = step.languages
    ? `<div class="tour-langs">${(["ru", "en"] as Lang[])
        .map(
          (code) =>
            `<button class="btn ${code === lang() ? "btn-primary" : "btn-secondary"}" ` +
            `data-tour="lang" data-lang="${code}">${t(`tour.lang.${code}`)}</button>`,
        )
        .join("")}</div>`
    : "";

  overlay.innerHTML =
    `<div class="tour-hole" id="tourHole"></div>` +
    `<div class="tour-card" id="tourCard">` +
    `<div class="tour-step">${escapeHtml(t("tour.counter", { n: at + 1, total: STEPS.length }))}</div>` +
    `<h3>${escapeHtml(t(`${step.key}.title`))}</h3>` +
    `<p>${escapeHtml(t(`${step.key}.body`))}</p>` +
    languages +
    `<div class="tour-actions">` +
    `<button class="link-button" data-tour="skip">${escapeHtml(t("tour.skip"))}</button>` +
    `<span class="tour-spacer"></span>` +
    (at > 0
      ? `<button class="btn btn-secondary" data-tour="back">${escapeHtml(t("tour.back"))}</button>`
      : "") +
    (step.tap
      ? ""
      : `<button class="btn btn-primary" data-tour="next">${escapeHtml(
          t(at + 1 === STEPS.length ? "tour.finish" : "tour.next"),
        )}</button>`) +
    `</div></div>`;
  place();
}

/**
 * Put the hole over the control and the card beside it.
 *
 * The hole is a box with an enormous shadow: everything outside it goes dark,
 * everything inside stays exactly as it was. A step with nothing to point at
 * hides the hole and centres the card.
 */
function place() {
  if (!overlay || overlay.hidden) return;
  const hole = document.getElementById("tourHole");
  const card = document.getElementById("tourCard");
  if (!hole || !card) return;

  const element = target();
  if (!element) {
    hole.hidden = true;
    card.style.left = "50%";
    card.style.top = "50%";
    card.style.transform = "translate(-50%, -50%)";
    return;
  }

  const box = element.getBoundingClientRect();
  const pad = 6;
  hole.hidden = false;
  hole.style.left = `${Math.max(0, box.left - pad)}px`;
  hole.style.top = `${Math.max(0, box.top - pad)}px`;
  hole.style.width = `${box.width + pad * 2}px`;
  hole.style.height = `${box.height + pad * 2}px`;

  card.style.transform = "none";
  const cardBox = card.getBoundingClientRect();
  const gap = 14;
  // Below the control when there is room, above it when there is not, and
  // never off the edge of the window.
  const below = box.bottom + gap;
  const above = box.top - gap - cardBox.height;
  const top = below + cardBox.height < window.innerHeight ? below : Math.max(gap, above);
  const left = Math.min(
    Math.max(gap, box.left + box.width / 2 - cardBox.width / 2),
    window.innerWidth - cardBox.width - gap,
  );
  card.style.top = `${top}px`;
  card.style.left = `${left}px`;
}
