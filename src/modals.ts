import { api, errorText } from "./api";
import type { ModalDeps } from "./deps";
import { closeModal, escapeHtml, formatDate, openModal, toast } from "./dom";
import { t } from "./i18n";
import { store } from "./store";
import type { DoctorReport, PendingAction } from "./types";

// ---------------------------------------------------------------------------
// Doctor
// ---------------------------------------------------------------------------

function doctorBody(report: DoctorReport): string {
  const counts = [
    t("doctor.models", { n: report.models_checked }),
    t("doctor.men", { n: report.men_checked }),
    t("doctor.chats", { n: report.chats_checked }),
  ];
  if (report.fixes_applied) counts.push(t("doctor.fixed", { n: report.fixes_applied }));
  const lines = report.issues
    .map(
      (issue) =>
        `<div class="doctor-line">` +
        `<span class="lvl ${issue.level}">${issue.level.toUpperCase()}</span>` +
        `<span class="doctor-text">${escapeHtml(issue.message)}${issue.fixed ? t("doctor.wasFixed") : ""}` +
        `<span class="doctor-path">${escapeHtml(issue.path)}</span></span>` +
        `</div>`,
    )
    .join("");
  return (
    `<div class="modal-sub">${escapeHtml(t("doctor.checked", { parts: counts.join(" \u00b7 ") }))}</div>` +
    `<div class="doctor-list">${lines}</div>`
  );
}

export async function openDoctorModal(deps: ModalDeps) {
  const card = openModal(`<h3>${t("doctor.title")}</h3><div class="modal-sub">${t("doctor.scanning")}</div>`);
  try {
    const report = await api.doctorScan();
    card.innerHTML = `
      <h3>${t("doctor.titleFull")}</h3>
      ${doctorBody(report)}
      <div class="modal-actions">
        <button class="btn btn-secondary" data-act="close">${t("common.close")}</button>
        <button class="btn btn-primary" id="btnFix">${t("doctor.fix")}</button>
      </div>`;
    card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);
    card.querySelector<HTMLButtonElement>("#btnFix")?.addEventListener("click", async () => {
      try {
        const fixed = await api.doctorFix();
        card.innerHTML = `<h3>${t("doctor.result")}</h3>${doctorBody(fixed)}
          <div class="modal-actions"><button class="btn btn-primary" data-act="close">${t("common.done")}</button></div>`;
        card
          .querySelector<HTMLButtonElement>('[data-act="close"]')
          ?.addEventListener("click", closeModal);
        await deps.refresh();
        toast(t("toast.fixed", { n: fixed.fixes_applied }), "success");
      } catch (error) {
        toast(errorText(error), "error");
      }
    });
  } catch (error) {
    card.innerHTML = `<h3>${t("doctor.title")}</h3><div class="modal-sub">${escapeHtml(errorText(error))}</div>
      <div class="modal-actions"><button class="btn btn-secondary" data-act="close">${t("common.close")}</button></div>`;
    card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);
  }
}

// ---------------------------------------------------------------------------
// Master agent
// ---------------------------------------------------------------------------

export async function openMasterModal(deps: ModalDeps) {
  const card = openModal(`
    <h3>${t("master.title")}</h3>
    <div class="modal-sub">
      ${t("master.sub")}
    </div>
    <div class="field">
      <label>${t("master.input")}</label>
      <textarea class="field-area" id="masterInput" placeholder="${t("master.inputHint")}"></textarea>
    </div>
    <label class="toggle" style="margin-bottom:10px">
      <input type="checkbox" id="autoCreate" checked /> ${t("master.autoCreate")}
    </label>
    <div id="masterResult"></div>
    <div class="modal-actions">
      <button class="btn btn-secondary" id="btnSearchOnly">${t("master.searchOnly")}</button>
      <button class="btn btn-secondary" data-act="close">${t("common.close")}</button>
      <button class="btn btn-primary" id="btnRoute">${t("master.route")}</button>
    </div>`);

  const result = card.querySelector<HTMLDivElement>("#masterResult")!;
  const input = card.querySelector<HTMLTextAreaElement>("#masterInput")!;
  input.focus();

  card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);

  card.querySelector<HTMLButtonElement>("#btnSearchOnly")?.addEventListener("click", async () => {
    const query = input.value.trim();
    if (!query) return;
    try {
      const hits = await api.globalSearch(query);
      result.innerHTML = hits.length
        ? hits
            .map(
              (hit) => `<div class="list-row">
                <div>
                  <div>${escapeHtml(hit.man_name)} — ${escapeHtml(hit.model_name)}</div>
                  <div class="meta">${escapeHtml(hit.snippet.slice(0, 110))}</div>
                </div>
                <button class="btn btn-secondary" data-open="${escapeHtml(hit.model_id)}|${escapeHtml(
                  hit.man_id,
                )}">${t("common.open")}</button>
              </div>`,
            )
            .join("")
        : `<div class="empty-hint">${t("empty.nothingFound")}</div>`;
      bindOpenButtons(result, deps);
    } catch (error) {
      toast(errorText(error), "error");
    }
  });

  card.querySelector<HTMLButtonElement>("#btnRoute")?.addEventListener("click", async () => {
    const raw = input.value.trim();
    if (!raw) return;
    result.innerHTML = `<div class="empty-hint">${t("master.thinking")}</div>`;
    try {
      const decision = await api.masterRoute(
        raw,
        card.querySelector<HTMLInputElement>("#autoCreate")!.checked,
      );
      const steps = decision.steps
        .map((s) => `<div class="meta">${escapeHtml(s.summary)}</div>`)
        .join("");
      result.innerHTML = `<div class="list-row">
          <div>
            <div>${escapeHtml(decision.reason || t("master.decided"))}</div>
            <div class="meta">${escapeHtml(
              t("master.meta", {
                model: decision.model_id ?? "—",
                man: decision.man_id ?? "—",
                conf: (decision.confidence * 100).toFixed(0),
              }),
            )}</div>
            ${steps}
          </div>
          ${
            decision.model_id
              ? `<button class="btn btn-primary" data-open="${escapeHtml(decision.model_id)}|${escapeHtml(
                  decision.man_id ?? "",
                )}">${t("master.goto")}</button>`
              : ""
          }
        </div>`;
      bindOpenButtons(result, deps);
      await deps.refresh();
    } catch (error) {
      result.innerHTML = `<div class="empty-hint">${escapeHtml(errorText(error))}</div>`;
    }
  });
}

function bindOpenButtons(container: HTMLElement, deps: ModalDeps) {
  container.querySelectorAll<HTMLButtonElement>("[data-open]").forEach((btn) => {
    btn.addEventListener("click", async () => {
      const [modelId, manId] = (btn.dataset.open ?? "").split("|");
      closeModal();
      if (modelId) await deps.selectProfile(modelId);
      if (manId) await deps.selectMan(manId);
    });
  });
}

// ---------------------------------------------------------------------------
// Approval queue
// ---------------------------------------------------------------------------

function diffBlock(action: PendingAction): string {
  return `<div class="diff-grid">
    <div class="code-block before">${escapeHtml(JSON.stringify(action.before, null, 2) ?? "null")}</div>
    <div class="code-block after">${escapeHtml(JSON.stringify(action.after, null, 2) ?? "null")}</div>
  </div>`;
}

export async function openPendingModal(deps: ModalDeps) {
  const draw = async () => {
    const pending = await api.pendingList();
    store.pending = pending;
    const card = openModal(`
      <h3>${t("queue.title", { n: pending.length })}</h3>
      <div class="modal-sub">${t("queue.sub")}</div>
      ${
        pending.length === 0
          ? `<div class="empty-hint">${t("empty.queue")}</div>`
          : pending
              .map(
                (action) => `<div class="list-row" style="flex-direction:column;align-items:stretch">
                  <div style="display:flex;justify-content:space-between;gap:10px;align-items:center">
                    <div>
                      <div>${escapeHtml(action.summary)}</div>
                      <div class="meta">${escapeHtml(action.tool)} · ${escapeHtml(action.risk)} · ${escapeHtml(
                        action.model_id,
                      )} · ${formatDate(action.created_at)}</div>
                    </div>
                    <div style="display:flex;gap:6px">
                      <button class="btn btn-secondary" data-reject="${escapeHtml(action.id)}">${t("queue.reject")}</button>
                      <button class="btn btn-primary" data-approve="${escapeHtml(action.id)}">${t("queue.approve")}</button>
                    </div>
                  </div>
                  ${diffBlock(action)}
                </div>`,
              )
              .join("")
      }
      <div class="modal-actions">
        ${pending.length ? `<button class="btn btn-danger" id="btnClearAll">${t("queue.clear")}</button>` : ""}
        <button class="btn btn-secondary" data-act="close">${t("common.close")}</button>
      </div>`);

    card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);
    card.querySelector<HTMLButtonElement>("#btnClearAll")?.addEventListener("click", async () => {
      await api.pendingClear();
      await deps.refresh();
      await draw();
    });

    card.querySelectorAll<HTMLButtonElement>("[data-approve]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        try {
          await api.pendingApprove(btn.dataset.approve!);
          toast(t("toast.applied"), "success");
          await deps.refresh();
          await draw();
        } catch (error) {
          toast(errorText(error), "error");
        }
      });
    });

    card.querySelectorAll<HTMLButtonElement>("[data-reject]").forEach((btn) => {
      btn.addEventListener("click", async () => {
        await api.pendingReject(btn.dataset.reject!);
        await deps.refresh();
        await draw();
      });
    });
  };

  await draw();
}
