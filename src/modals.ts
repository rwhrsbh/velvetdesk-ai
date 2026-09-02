import { api, errorText } from "./api";
import type { ModalDeps } from "./deps";
import { closeModal, escapeHtml, formatDate, openModal, toast } from "./dom";
import { t } from "./i18n";
import { store } from "./store";
import type { Backup, DoctorReport, PendingAction } from "./types";

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

/**
 * Copies kept before an agent changed a file.
 *
 * They live beside the doctor because this is where an operator comes when
 * something is wrong with the data — including "the agent deleted my file".
 */
async function backupsSection(): Promise<string> {
  let backups: Backup[] = [];
  try {
    backups = await api.listBackups();
  } catch (error) {
    console.error("backups", error);
    return "";
  }
  if (backups.length === 0) return "";

  const rows = backups
    .slice(0, 40)
    .map(
      (backup) =>
        `<div class="doctor-line"><span class="lvl ok">${escapeHtml(backup.reason)}</span>` +
        `<span class="doctor-text">${escapeHtml(backup.original)}` +
        `<span class="doctor-path">${formatDate(backup.created_at)} · ${Math.max(1, Math.round(backup.bytes / 1024))} KB</span></span>` +
        `<button class="btn btn-secondary" data-restore="${escapeHtml(backup.id)}">${t("doctor.restore")}</button></div>`,
    )
    .join("");

  return (
    `<label class="section-label">${t("doctor.backups")}</label>` +
    `<div class="doctor-list">${rows}</div>`
  );
}

export async function openDoctorModal(deps: ModalDeps) {
  const card = openModal(`<h3>${t("doctor.title")}</h3><div class="modal-sub">${t("doctor.scanning")}</div>`);
  try {
    const report = await api.doctorScan();
    const backups = await backupsSection();
    card.innerHTML = `
      <h3>${t("doctor.titleFull")}</h3>
      ${doctorBody(report)}
      ${backups}
      <div class="modal-actions">
        <button class="btn btn-secondary" data-act="close">${t("common.close")}</button>
        <button class="btn btn-primary" id="btnFix">${t("doctor.fix")}</button>
      </div>`;
    card.querySelector<HTMLButtonElement>('[data-act="close"]')?.addEventListener("click", closeModal);

    card.addEventListener("click", async (event) => {
      const id = (event.target as HTMLElement).closest<HTMLElement>("[data-restore]")?.dataset
        .restore;
      if (!id) return;
      try {
        const path = await api.restoreBackup(id);
        toast(t("doctor.restored", { path }), "success");
      } catch (error) {
        toast(errorText(error), "error");
      }
    });

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
