/**
 * Buying a subscription, or more credits, without leaving the app.
 *
 * Three steps on one screen: what to buy, what to pay with, and where to send
 * it. The dialog then waits — a crypto payment lands when it lands, and the
 * operator should be able to close the window, come back, and still get what
 * they paid for. Every order is written down on both sides the moment it is
 * opened, so the only thing a closed window costs is the waiting.
 *
 * When the money arrives the key is not shown and left there to be copied: it
 * is activated on the spot, because the operator's next move would be to
 * paste it into the field two centimetres away. It is still listed under
 * "purchases", for the second machine and for the one who reinstalls.
 */
import { api, errorText } from "./api";
import { closeModal, escapeHtml, openModal, toast } from "./dom";
import { t } from "./i18n";

interface Plan {
  tier: string;
  months: number;
  price_usd: number;
  devices: number;
  credits_week: number;
}

interface Coin {
  code: string;
  name: string;
  logo: string | null;
}

interface Order {
  order_id: string;
  tier: string;
  months: number;
  paid: boolean;
  license: string;
  license_id: string;
  created_at: number;
}

/** How often the dialog asks whether the money has landed. */
const POLL_MS = 6000;

/** Coins worth offering first: the ones people actually pay in. */
const FAVOURITES = ["usdttrc20", "usdtbsc", "usdcbsc", "btc", "eth", "ltc", "trx", "ton"];

function planName(plan: Plan): string {
  const period = plan.months === 12 ? t("buy.year") : t("buy.months", { n: plan.months });
  return `${plan.tier === "business" ? "Business" : "Pro"} · ${period}`;
}

export async function openBuyModal(options: { credits?: boolean } = {}) {
  const buyingCredits = Boolean(options.credits);
  const card = openModal(`
    <h3>${t(buyingCredits ? "buy.titleCredits" : "buy.title")}</h3>
    <div class="modal-sub">${t(buyingCredits ? "buy.subCredits" : "buy.sub")}</div>
    <div id="buyBody"><div class="empty-hint">${t("buy.loading")}</div></div>
    <div class="modal-actions">
      <button class="btn btn-secondary" data-act="close">${t("common.close")}</button>
    </div>
  `);

  const body = card.querySelector<HTMLElement>("#buyBody")!;
  let plans: Plan[] = [];
  let coins: Coin[] = [];
  let chosenPlan: Plan | null = null;
  let credits = 5000;
  let chosenCoin = "";
  let filter = "";

  try {
    const [planList, coinList] = await Promise.all([api.cloudPlans(), api.cloudCoins()]);
    plans = (planList.plans ?? []) as Plan[];
    coins = (coinList.coins ?? []) as Coin[];
    chosenPlan = plans[0] ?? null;
    // Favourites first, then everything else alphabetically: the list runs to
    // several hundred coins and the first screen should be the usual ones.
    coins.sort((a, b) => {
      const rank = (coin: Coin) => {
        const at = FAVOURITES.indexOf(coin.code);
        return at === -1 ? FAVOURITES.length : at;
      };
      return rank(a) - rank(b) || a.code.localeCompare(b.code);
    });
    chosenCoin = coins[0]?.code ?? "";
  } catch (error) {
    body.innerHTML = `<div class="empty-hint">${escapeHtml(errorText(error))}</div>`;
    return;
  }

  const coinRow = (coin: Coin) => `
    <button type="button" class="coin${coin.code === chosenCoin ? " active" : ""}" data-coin="${escapeHtml(coin.code)}">
      ${
        coin.logo
          ? `<img src="${escapeHtml(coin.logo)}" alt="" loading="lazy" draggable="false" />`
          : `<span class="coin-blank">${escapeHtml(coin.code.slice(0, 3).toUpperCase())}</span>`
      }
      <span class="coin-code">${escapeHtml(coin.code.toUpperCase())}</span>
    </button>`;

  function drawChoice() {
    const needle = filter.trim().toLowerCase();
    const shown = coins
      .filter((coin) => !needle || coin.code.includes(needle) || coin.name.toLowerCase().includes(needle))
      .slice(0, 120);

    body.innerHTML = `
      ${
        buyingCredits
          ? `<div class="field">
        <label>${t("buy.howMany")}</label>
        <div class="row-inline">
          <input class="field-input" id="buyCredits" type="number" min="100" step="500" value="${credits}" />
          <span class="meta" id="buyPrice"></span>
        </div>
      </div>`
          : `<div class="field">
        <label>${t("buy.plan")}</label>
        <div class="plan-grid">
          ${plans
            .map(
              (plan) => `<button type="button" class="plan-pick${
                chosenPlan && plan.tier === chosenPlan.tier && plan.months === chosenPlan.months
                  ? " active"
                  : ""
              }" data-plan="${escapeHtml(`${plan.tier}:${plan.months}`)}">
                <span class="plan-pick-name">${escapeHtml(planName(plan))}</span>
                <span class="plan-pick-price">$${plan.price_usd}</span>
                <span class="plan-pick-meta">${t("buy.planMeta", {
                  devices: plan.devices,
                  credits: Math.round(plan.credits_week),
                })}</span>
              </button>`,
            )
            .join("")}
        </div>
      </div>`
      }

      <div class="field">
        <label>${t("buy.coin")}
          <span class="hint-inline">${t("buy.coinHint")}</span>
        </label>
        <input class="field-input" id="coinSearch" placeholder="${t("buy.coinSearch")}" value="${escapeHtml(filter)}" autocomplete="off" />
        <div class="coin-grid">${shown.map(coinRow).join("")}</div>
      </div>

      <div class="row-inline">
        <button class="btn btn-primary" id="btnPay">${t("buy.pay")}</button>
      </div>`;

    const price = body.querySelector<HTMLElement>("#buyPrice");
    if (price) price.textContent = t("buy.priceHint", { n: credits });

    body.querySelector<HTMLInputElement>("#buyCredits")?.addEventListener("input", (event) => {
      credits = Number((event.target as HTMLInputElement).value) || 0;
      if (price) price.textContent = t("buy.priceHint", { n: credits });
    });

    const search = body.querySelector<HTMLInputElement>("#coinSearch");
    search?.addEventListener("input", () => {
      filter = search.value;
      const at = search.selectionStart;
      drawChoice();
      const again = body.querySelector<HTMLInputElement>("#coinSearch");
      again?.focus();
      if (at !== null) again?.setSelectionRange(at, at);
    });

    body.querySelectorAll<HTMLButtonElement>("[data-plan]").forEach((button) => {
      button.addEventListener("click", () => {
        const [tier, months] = (button.dataset.plan ?? "").split(":");
        chosenPlan = plans.find((plan) => plan.tier === tier && plan.months === Number(months)) ?? null;
        drawChoice();
      });
    });

    body.querySelectorAll<HTMLButtonElement>("[data-coin]").forEach((button) => {
      button.addEventListener("click", () => {
        chosenCoin = button.dataset.coin ?? "";
        drawChoice();
      });
    });

    body.querySelector("#btnPay")?.addEventListener("click", () => void pay());
  }

  async function pay() {
    if (!chosenCoin) return;
    const button = body.querySelector<HTMLButtonElement>("#btnPay");
    if (button) button.disabled = true;
    try {
      const payment = buyingCredits
        ? await api.cloudBuyCredits(credits, chosenCoin)
        : await api.cloudSubscribe(chosenPlan!.tier, chosenPlan!.months, chosenCoin, "");
      drawPayment(payment as Record<string, unknown>);
    } catch (error) {
      toast(errorText(error), "error");
      if (button) button.disabled = false;
    }
  }

  function drawPayment(payment: Record<string, unknown>) {
    const address = String(payment.pay_address ?? "");
    const amount = String(payment.pay_amount ?? "");
    const coin = String(payment.pay_currency ?? chosenCoin).toUpperCase();
    const order = String(payment.order_id ?? "");

    body.innerHTML = `
      <div class="pay-card">
        <div class="meta">${t("buy.sendExactly")}</div>
        <div class="pay-amount">${escapeHtml(amount)} ${escapeHtml(coin)}</div>
        <div class="meta">${t("buy.toAddress")}</div>
        <div class="pay-address" id="payAddress">${escapeHtml(address)}</div>
        <div class="row-inline">
          <button class="btn btn-secondary" id="btnCopyAddress">${t("buy.copyAddress")}</button>
          <button class="btn btn-secondary" id="btnCopyAmount">${t("buy.copyAmount")}</button>
        </div>
        <div class="meta" id="payState">${t("buy.waiting")}</div>
        ${order ? `<div class="meta">${t("buy.orderIs", { id: order })}</div>` : ""}
        <div class="meta">${t(order ? "buy.closeIsSafe" : "buy.closeIsSafeCredits")}</div>
      </div>`;

    body.querySelector("#btnCopyAddress")?.addEventListener("click", () => {
      void navigator.clipboard.writeText(address).then(() => toast(t("buy.copied"), "success"));
    });
    body.querySelector("#btnCopyAmount")?.addEventListener("click", () => {
      void navigator.clipboard.writeText(amount).then(() => toast(t("buy.copied"), "success"));
    });

    if (order) void watch(order);
  }

  /** Ask every few seconds until the key is there, then put it to work. */
  async function watch(order: string) {
    const state = () => body.querySelector<HTMLElement>("#payState");
    for (;;) {
      await new Promise((done) => window.setTimeout(done, POLL_MS));
      // The operator closed the dialog: the order is on the gateway and in
      // the local history, so there is nothing to keep a timer alive for.
      if (!document.body.contains(body)) return;
      let answer: Order;
      try {
        answer = (await api.cloudOrder(order)) as unknown as Order;
      } catch {
        continue;
      }
      if (!answer.paid || !answer.license) continue;

      const line = state();
      if (line) line.textContent = t("buy.arrived");
      try {
        await api.activateLicense(answer.license);
        toast(t("buy.activated"), "success");
        closeModal();
      } catch (error) {
        // The key is good and saved; only switching to it failed. Show it
        // rather than swallowing it — it is the thing they paid for.
        if (line) {
          line.innerHTML = `${t("buy.activateFailed", { error: escapeHtml(errorText(error)) })}
            <div class="pay-address">${escapeHtml(answer.license)}</div>`;
        }
      }
      return;
    }
  }

  drawChoice();
}

/** Orders this machine has placed, with the keys they earned. */
export async function openPurchasesModal() {
  const card = openModal(`
    <h3>${t("buy.historyTitle")}</h3>
    <div class="modal-sub">${t("buy.historySub")}</div>
    <div id="historyBody"><div class="empty-hint">${t("buy.loading")}</div></div>
    <div class="modal-actions">
      <button class="btn btn-secondary" data-act="close">${t("common.close")}</button>
    </div>
  `);

  const body = card.querySelector<HTMLElement>("#historyBody")!;
  let orders: Order[] = [];
  try {
    const answer = await api.cloudPurchases();
    orders = (answer.orders ?? []) as Order[];
  } catch (error) {
    body.innerHTML = `<div class="empty-hint">${escapeHtml(errorText(error))}</div>`;
    return;
  }

  if (orders.length === 0) {
    body.innerHTML = `<div class="empty-hint">${t("buy.historyEmpty")}</div>`;
    return;
  }

  body.innerHTML = orders
    .map(
      (order) => `<div class="list-row">
        <div>
          <div>${escapeHtml(order.tier || "—")} · ${t("buy.months", { n: order.months })} · ${
            order.paid ? t("buy.paid") : t("buy.unpaid")
          }</div>
          <div class="meta">${new Date((order.created_at || 0) * 1000).toLocaleString()}</div>
          ${order.license ? `<div class="pay-address">${escapeHtml(order.license)}</div>` : ""}
        </div>
        ${
          order.license
            ? `<button class="btn btn-secondary" data-use="${escapeHtml(order.license)}">${t("buy.useKey")}</button>`
            : ""
        }
      </div>`,
    )
    .join("");

  body.querySelectorAll<HTMLButtonElement>("[data-use]").forEach((button) => {
    button.addEventListener("click", async () => {
      try {
        await api.activateLicense(button.dataset.use ?? "");
        toast(t("buy.activated"), "success");
        closeModal();
      } catch (error) {
        toast(errorText(error), "error");
      }
    });
  });
}
