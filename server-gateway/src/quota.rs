//! What an answer cost and whether the licence may spend it.
//!
//! Requests are not the unit: a turn carrying a twelve-thousand-token dossier
//! and a turn saying "hi" are the same request and a hundredfold apart in
//! money. The unit is the credit, which is a fixed amount of cost, so a tier's
//! budget is a budget and the margin is arithmetic.

use crate::config::Tier;
use crate::db::Db;
use crate::registry::ModelRow;
use vd_llm::Usage;

/// Five hours, in seconds — the smaller window, sized to a shift.
pub const WINDOW_5H: i64 = 5 * 60 * 60;
/// Seven days, in seconds — the one that keeps a month's cost in shape.
pub const WINDOW_WEEK: i64 = 7 * 24 * 60 * 60;

/// Cost of one answer in credits.
///
/// Prompt tokens the provider served from its own cache are billed at the
/// cached rate and the rest at the full one; a model with no cache prices
/// both the same, so nothing is discounted that nobody discounted for us.
pub fn credits(model: &ModelRow, usage: &Usage, credit_usd: f64) -> f64 {
    // When the upstream says what it charged, that is what gets billed. A
    // price table is a copy of somebody else's prices and goes stale the day
    // they change one — and the first sign of that is a month of answers
    // sold below cost.
    if let Some(dollars) = usage.upstream_cost {
        if credit_usd <= 0.0 {
            return 0.0;
        }
        return (dollars / credit_usd).max(0.0);
    }
    let cached = usage.cached_tokens.min(usage.prompt_tokens) as f64;
    let fresh = usage.prompt_tokens as f64 - cached;
    let out = usage.completion_tokens as f64;
    let dollars = (fresh * model.price_in + cached * model.cached_price() + out * model.price_out)
        / 1_000_000.0;
    if credit_usd <= 0.0 {
        return 0.0;
    }
    dollars / credit_usd
}

/// What one dictated clip costs, in credits.
///
/// A transcription comes back as text with no token count, so it is priced
/// per clip: the model row carries the dollars, and this turns them into the
/// same credits everything else is measured in.
pub fn request_credits(dollars: f64, credit_usd: f64) -> f64 {
    if credit_usd <= 0.0 {
        return 0.0;
    }
    (dollars / credit_usd).max(0.0)
}

/// Record a flat-rate call — one with no tokens to count — and say what is
/// left afterwards.
pub fn charge_flat(
    db: &Db,
    license_id: &str,
    tier: Tier,
    model: &str,
    credits: f64,
    now: i64,
) -> rusqlite::Result<Allowance> {
    db.record(
        &crate::db::Spend {
            license_id: license_id.to_string(),
            model: model.to_string(),
            credits,
            usage: Usage::default(),
        },
        now,
    )?;
    allowance(db, license_id, tier, now)
}

/// What the two windows have left, and when the tighter one reopens.
#[derive(Debug, Clone, Copy)]
pub struct Allowance {
    pub left_5h: f64,
    pub left_week: f64,
    /// Unix seconds at which the exhausted window first has room. Zero when
    /// nothing is exhausted.
    pub reset_at: i64,
}

impl Allowance {
    pub fn exhausted(&self) -> bool {
        self.left_5h <= 0.0 || self.left_week <= 0.0
    }

    /// What the client gets to show the operator: the tighter of the two.
    pub fn left(&self) -> f64 {
        self.left_5h.min(self.left_week)
    }
}

/// Where this licence stands, before a request is allowed to cost anything.
pub fn allowance(db: &Db, license_id: &str, tier: Tier, now: i64) -> rusqlite::Result<Allowance> {
    let since_5h = now - WINDOW_5H;
    let since_week = now - WINDOW_WEEK;
    let used_5h = db.spent_since(license_id, since_5h)?;
    let used_week = db.spent_since(license_id, since_week)?;
    let left_5h = tier.credits_5h - used_5h;
    let left_week = tier.credits_week - used_week;

    // A window reopens when its oldest entry falls out of it. Reporting the
    // window that is actually exhausted matters: telling someone to wait five
    // hours when the weekly budget is gone would be a lie they find out about
    // five hours later.
    let mut reset_at = 0;
    if left_5h <= 0.0 {
        if let Some(oldest) = db.oldest_since(license_id, since_5h)? {
            reset_at = oldest + WINDOW_5H;
        }
    }
    if left_week <= 0.0 {
        if let Some(oldest) = db.oldest_since(license_id, since_week)? {
            reset_at = reset_at.max(oldest + WINDOW_WEEK);
        }
    }

    Ok(Allowance {
        left_5h,
        left_week,
        reset_at,
    })
}

/// Record what an answer cost, and say what is left after it.
///
/// A model nobody has priced is recorded at zero rather than dropped: the
/// answer was already produced and paid for upstream, and losing the row
/// would hide it from every report afterwards.
/// One answer, ready to be written down.
pub struct Bill<'a> {
    pub license_id: &'a str,
    pub tier: Tier,
    /// The name the answer was billed under — the model that actually
    /// replied, not the one that was asked for.
    pub model: &'a str,
    /// The model's prices, when it has any. A model nobody has priced is
    /// recorded at zero rather than dropped: the answer was produced and paid
    /// for upstream, and losing the row would hide it from every report.
    pub priced: Option<&'a ModelRow>,
    pub usage: &'a Usage,
    pub credit_usd: f64,
    pub now: i64,
}

/// Record what an answer cost, and say what is left after it.
pub fn charge(db: &Db, bill: &Bill<'_>) -> rusqlite::Result<Allowance> {
    let spent = bill
        .priced
        .map(|model| credits(model, bill.usage, bill.credit_usd))
        .unwrap_or(0.0);
    db.record(
        &crate::db::Spend {
            license_id: bill.license_id.to_string(),
            model: bill.model.to_string(),
            credits: spent,
            usage: bill.usage.clone(),
        },
        bill.now,
    )?;
    allowance(db, bill.license_id, bill.tier, bill.now)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real price beats a remembered one.
    #[test]
    fn the_upstream_bill_wins_when_there_is_one() {
        let usage = Usage {
            prompt_tokens: 1_000_000,
            completion_tokens: 0,
            total_tokens: 1_000_000,
            cached_tokens: 0,
            upstream_cost: Some(0.5),
        };
        // The table says $0.28 for a million prompt tokens; the provider
        // says it charged fifty cents, and fifty cents is what is billed.
        assert_eq!(credits(&model(), &usage, 0.001), 500.0);
    }

    fn model() -> ModelRow {
        ModelRow {
            name: "deepseek-chat".into(),
            upstream_id: "openrouter".into(),
            upstream_name: String::new(),
            price_in: 0.28,
            price_cached: Some(0.028),
            price_out: 0.42,
            context_tokens: None,
            enabled: true,
            position: 0,
            voice: false,
            price_request: 0.0,
        }
    }

    fn usage(prompt: u32, cached: u32, out: u32) -> Usage {
        Usage {
            prompt_tokens: prompt,
            completion_tokens: out,
            total_tokens: prompt + out,
            cached_tokens: cached,
            upstream_cost: None,
        }
    }

    /// A million fresh prompt tokens costs exactly the model's input price,
    /// turned into credits at the configured rate.
    #[test]
    fn a_million_tokens_costs_the_listed_price() {
        let credits = credits(&model(), &usage(1_000_000, 0, 0), 0.001);
        assert!((credits - 280.0).abs() < 1e-6, "{credits}");
    }

    /// The cache is where the margin is: the same prompt served from it costs
    /// a tenth as much, and the bill has to show that.
    #[test]
    fn cached_prompt_tokens_are_billed_at_the_cached_rate() {
        let fresh = credits(&model(), &usage(1_000_000, 0, 0), 0.001);
        let cached = credits(&model(), &usage(1_000_000, 1_000_000, 0), 0.001);
        assert!((cached - 28.0).abs() < 1e-6, "{cached}");
        assert!(cached * 9.0 < fresh);
    }

    /// A provider that reports more cached tokens than prompt tokens — it
    /// happens — must not produce a negative bill.
    #[test]
    fn nonsense_cache_counts_cannot_go_below_zero() {
        let credits = credits(&model(), &usage(100, 500, 0), 0.001);
        assert!(credits >= 0.0);
    }

    #[test]
    fn output_tokens_are_billed_at_the_output_price() {
        let credits = credits(&model(), &usage(0, 0, 1_000_000), 0.001);
        assert!((credits - 420.0).abs() < 1e-6, "{credits}");
    }

    fn tier() -> Tier {
        Tier {
            credits_5h: 100.0,
            credits_week: 1_000.0,
            max_peers: 2,
        }
    }

    #[test]
    fn spending_eats_into_both_windows() {
        let db = Db::memory().unwrap();
        let now = 10_000_000;
        let after = charge(
            &db,
            &Bill {
                license_id: "VD-PRO-1",
                tier: tier(),
                model: "deepseek-chat",
                priced: Some(&model()),
                usage: &usage(100_000, 0, 0),
                credit_usd: 0.001,
                now,
            },
        )
        .unwrap();
        // 100k fresh tokens at $0.28/M = $0.028 = 28 credits.
        assert!((after.left_5h - 72.0).abs() < 1e-6, "{:?}", after);
        assert!((after.left_week - 972.0).abs() < 1e-6, "{:?}", after);
        assert!(!after.exhausted());
    }

    /// The five-hour window is the one that trips first, and it says when it
    /// reopens — five hours after the earliest spend still inside it.
    #[test]
    fn the_short_window_trips_first_and_says_when_it_reopens() {
        let db = Db::memory().unwrap();
        let now = 10_000_000;
        for _ in 0..4 {
            charge(
                &db,
                &Bill {
                    license_id: "VD-PRO-1",
                    tier: tier(),
                    model: "deepseek-chat",
                    priced: Some(&model()),
                    usage: &usage(100_000, 0, 0),
                    credit_usd: 0.001,
                    now,
                },
            )
            .unwrap();
        }
        let state = allowance(&db, "VD-PRO-1", tier(), now).unwrap();
        assert!(state.exhausted());
        assert_eq!(state.reset_at, now + WINDOW_5H);
        assert!(state.left_week > 0.0, "the week is not gone yet");

        // Once the spending is old enough, the window has room again.
        let later = now + WINDOW_5H + 1;
        let state = allowance(&db, "VD-PRO-1", tier(), later).unwrap();
        assert!(!state.exhausted());
    }

    /// An unknown model costs nothing rather than crashing the request: the
    /// answer was already produced, and losing the record would be worse.
    #[test]
    fn an_unpriced_model_is_recorded_at_zero() {
        let db = Db::memory().unwrap();
        let after = charge(
            &db,
            &Bill {
                license_id: "VD-PRO-1",
                tier: tier(),
                model: "model-nobody-configured",
                priced: None,
                usage: &usage(100_000, 0, 0),
                credit_usd: 0.001,
                now: 10_000_000,
            },
        )
        .unwrap();
        assert_eq!(after.left_5h, 100.0);
    }
}
