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
 */
import { escapeHtml } from "./dom";
import { lang, setLang, t, type Lang } from "./i18n";

export interface TourDeps {
  /** The app's language switch — the tour has none of its own. */
  setLanguage(next: Lang): void;
  /** Called once the tour is over, however it ended. */
  onDone(): void;
}

interface Step {
  /** Which control this step is about; centred on the screen when absent. */
  target?: string;
  /** Dictionary key of the wording: `<key>.title` and `<key>.body`. */
  key: string;
  /** The language picker belongs to the first step only. */
  languages?: boolean;
}

const STEPS: Step[] = [
  { key: "tour.welcome", languages: true },
  { target: "#providerChip", key: "tour.provider" },
  { target: "#btnKeys", key: "tour.keys" },
  { target: "#paneProfiles", key: "tour.profiles" },
  { target: "#paneMen", key: "tour.men" },
  { target: "#messages", key: "tour.chat" },
  { target: "#composerInput", key: "tour.composer" },
  { target: ".composer-left", key: "tour.tools" },
  { target: '#modeControl [data-mode="auto"]', key: "tour.auto" },
  { target: '#modeControl [data-mode="act"]', key: "tour.act" },
  { target: '#modeControl [data-mode="memorize"]', key: "tour.memorize" },
  { target: '#modeControl [data-mode="letters"]', key: "tour.letters" },
  { target: "#securityControl", key: "tour.security" },
  { target: "#btnPending", key: "tour.pending" },
  { target: "#btnMaster", key: "tour.master" },
  { target: "#btnTemporary", key: "tour.temporary" },
  { target: "#btnDoctor", key: "tour.doctor" },
  { target: "#btnGuide", key: "tour.again" },
];

let at = 0;
let deps: TourDeps | null = null;
let overlay: HTMLElement | null = null;

/** Open the tour at the first step. */
export function startTour(next: TourDeps) {
  deps = next;
  at = 0;
  if (!overlay) {
    overlay = document.createElement("div");
    overlay.className = "tour";
    overlay.id = "tour";
    document.body.appendChild(overlay);
    overlay.addEventListener("click", onClick);
    window.addEventListener("resize", place);
    window.addEventListener("keydown", onKey, true);
  }
  overlay.hidden = false;
  draw();
}

/** Whether the tour is on screen. */
export function tourRunning(): boolean {
  return Boolean(overlay && !overlay.hidden);
}

function close() {
  if (overlay) overlay.hidden = true;
  deps?.onDone();
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
  const next = at + by;
  if (next < 0) return;
  if (next >= STEPS.length) return close();
  at = next;
  draw();
}

/** The element this step is about, when it is on screen. */
function target(): HTMLElement | null {
  const selector = STEPS[at].target;
  if (!selector) return null;
  return document.querySelector<HTMLElement>(selector);
}

function draw() {
  if (!overlay) return;
  const step = STEPS[at];
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
    `<button class="btn btn-primary" data-tour="next">${escapeHtml(
      t(at + 1 === STEPS.length ? "tour.finish" : "tour.next"),
    )}</button>` +
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
