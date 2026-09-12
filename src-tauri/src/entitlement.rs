//! What this copy of VelvetDesk is allowed to do.
//!
//! One place answers that, and everything else asks it: the free version is
//! the same build as the paid one, with a licence missing. The licence is an
//! Ed25519 token checked here against a key baked into the binary — no
//! network, no server call, and nothing for the operator to configure. A
//! licence that does not verify is simply absent, and the app falls back to
//! the free limits rather than refusing to start.
//!
//! The free limits are deliberately the ones a real workday runs into: a
//! hundred model calls, ten profiles, ten men in each. Everything the paid
//! plans add — the cloud provider, dictation through it, sync between
//! machines — is gated on the same token.

use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{Datelike, TimeZone, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

use crate::error::{AppError, Result};
use crate::storage::{read_json, write_json, Paths};

/// The gateway's licence key, baked in at build time.
///
/// Set `VD_LICENSE_PUBLIC_KEY` (the base64 the gateway's keygen prints) when
/// building a release. An empty key verifies nothing, which means a build
/// without it is a free build for everybody — an honest default, and one that
/// cannot be turned into a paid one by editing a file on disk.
pub const PUBLIC_KEY: &str = match option_env!("VD_LICENSE_PUBLIC_KEY") {
    Some(key) => key,
    None => "",
};

/// The key this run actually checks signatures against.
///
/// A release build trusts only what was compiled into it: reading the key
/// from the environment at run time would let anyone point the app at a key
/// of their own and sign themselves a licence. A debug build does read the
/// environment, because that is how the gateway on the bench is tested
/// without rebuilding the app for every key.
pub fn public_key() -> String {
    // An unset repository secret compiles in as an empty string; here that
    // correctly means "this build checks no signatures", so it needs no
    // special case beyond the trim.
    if !PUBLIC_KEY.trim().is_empty() {
        return PUBLIC_KEY.trim().to_string();
    }
    if cfg!(debug_assertions) {
        return std::env::var("VD_LICENSE_PUBLIC_KEY")
            .unwrap_or_default()
            .trim()
            .to_string();
    }
    String::new()
}

/// The provider that is the subscription: its "key" is the licence.
pub const CLOUD_PROVIDER: &str = "velvetdesk-cloud";

/// The header the gateway counts devices by.
pub const DEVICE_HEADER: &str = "X-VD-Device";

/// Where the subscription lives, fixed at build time.
///
/// Not a setting. The operator has no way of knowing a good value for this,
/// and a wrong one turns a paid-for subscription into a provider that
/// answers nothing — so the address is part of the build, like the licence
/// key it is checked with. `VD_CLOUD_BASE_URL` sets it: an IP works as well
/// as a name (`http://203.0.113.10:8787/v1`), and a test build points at a
/// gateway on the bench the same way.
///
/// Until there is a server to point at, the default is the gateway on this
/// machine: a build made without the variable is a build for the bench, and
/// the bench is where the gateway currently runs. Releases are built with
/// the variable set to the real address, which is an IP for as long as there
/// is no name to use.
pub const CLOUD_BASE_URL: &str = match option_env!("VD_CLOUD_BASE_URL") {
    Some(url) => url,
    None => DEFAULT_CLOUD_BASE_URL,
};

/// Where a build with no address of its own looks: the gateway on this
/// machine, which is where it runs until there is a server to point at.
pub const DEFAULT_CLOUD_BASE_URL: &str = "http://127.0.0.1:8787/v1";

/// The address this run uses.
///
/// Same rule as the licence key: a release goes where it was built to go,
/// and only a debug build reads the environment — otherwise pointing the app
/// at another gateway would be a matter of setting a variable.
pub fn cloud_base_url() -> String {
    if cfg!(debug_assertions) {
        if let Ok(url) = std::env::var("VD_CLOUD_BASE_URL") {
            if !url.trim().is_empty() {
                return url.trim().trim_end_matches('/').to_string();
            }
        }
    }
    // A CI build passes the variable from a repository secret, and an unset
    // secret arrives as an empty string rather than as nothing at all — so
    // the compiled-in value can be present and blank, which is not the same
    // as "no address was chosen". Blank means the same as absent here.
    if CLOUD_BASE_URL.trim().is_empty() {
        return DEFAULT_CLOUD_BASE_URL.to_string();
    }
    CLOUD_BASE_URL.trim().trim_end_matches('/').to_string()
}

/// Model calls a free copy may make in a day.
pub const FREE_REQUESTS_PER_DAY: u32 = 100;
/// Profiles a free copy may hold.
pub const FREE_PROFILES: usize = 10;
/// Men per profile a free copy may hold.
pub const FREE_MEN_PER_PROFILE: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Plan {
    Free,
    Pro,
    Business,
}

impl Plan {
    /// The tier string in a licence, as the gateway writes it.
    pub fn from_tier(tier: &str) -> Plan {
        match tier.trim().to_ascii_lowercase().as_str() {
            "business" | "agency" | "team" => Plan::Business,
            "" => Plan::Free,
            _ => Plan::Pro,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Plan::Free => "free",
            Plan::Pro => "pro",
            Plan::Business => "business",
        }
    }

    pub fn paid(self) -> bool {
        !matches!(self, Plan::Free)
    }
}

/// What the plan allows. `None` means no ceiling at all.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Limits {
    pub requests_per_day: Option<u32>,
    pub profiles: Option<usize>,
    pub men_per_profile: Option<usize>,
    /// Devices that may share one licence through sync.
    pub devices: u32,
    /// May route through the VelvetDesk gateway at all.
    pub cloud: bool,
    /// May sync with another machine.
    pub sync: bool,
}

impl Limits {
    pub fn free() -> Limits {
        Limits {
            requests_per_day: Some(FREE_REQUESTS_PER_DAY),
            profiles: Some(FREE_PROFILES),
            men_per_profile: Some(FREE_MEN_PER_PROFILE),
            devices: 1,
            cloud: false,
            sync: false,
        }
    }

    pub fn paid(devices: u32) -> Limits {
        Limits {
            requests_per_day: None,
            profiles: None,
            men_per_profile: None,
            devices: devices.max(1),
            cloud: true,
            sync: devices > 1,
        }
    }
}

/// The licence as this device reads it, plus what it unlocks.
#[derive(Debug, Clone, Serialize)]
pub struct Entitlement {
    pub plan: Plan,
    /// A licence that is signed by our key and still in date.
    pub valid: bool,
    pub license_id: String,
    pub tier: String,
    /// Unix seconds; zero when there is no licence.
    pub expires_at: i64,
    /// Negative once it has run out.
    pub days_left: i64,
    /// `license.missing`, `license.expired`, `license.invalid` — for the UI.
    pub problem: String,
    pub limits: Limits,
}

impl Entitlement {
    pub fn free(problem: &str) -> Entitlement {
        Entitlement {
            plan: Plan::Free,
            valid: false,
            license_id: String::new(),
            tier: String::new(),
            expires_at: 0,
            days_left: 0,
            problem: problem.into(),
            limits: Limits::free(),
        }
    }
}

/// Read a licence token and say what it grants.
///
/// An expired licence is not an invalid one: it keeps its tier and its dates
/// so the app can say what ran out and when, while granting nothing.
pub fn read(token: &str) -> Entitlement {
    let token = token.trim();
    if token.is_empty() {
        return Entitlement::free("license.missing");
    }
    let Some(key) = vd_license::public_key_from_base64(&public_key()) else {
        return Entitlement::free("license.noPublicKey");
    };
    match vd_license::verify(token, &key) {
        Ok(license) => {
            let now = Utc::now().timestamp();
            let expired = license.expired_at(now);
            let plan = Plan::from_tier(&license.tier);
            let devices = if license.max_peers == 0 {
                match plan {
                    Plan::Business => 10,
                    Plan::Pro => 2,
                    Plan::Free => 1,
                }
            } else {
                license.max_peers
            };
            Entitlement {
                plan: if expired { Plan::Free } else { plan },
                valid: !expired,
                license_id: license.license_id,
                tier: license.tier,
                expires_at: license.expires_at,
                days_left: (license.expires_at - now).div_euclid(86_400),
                problem: if expired {
                    "license.expired".into()
                } else {
                    String::new()
                },
                limits: if expired {
                    Limits::free()
                } else {
                    Limits::paid(devices)
                },
            }
        }
        Err(err) => Entitlement::free(&format!("license.invalid:{err}")),
    }
}

/// What the gateway last said about a licence, kept for the builds that
/// cannot check a signature themselves.
///
/// A release carries the public key and needs none of this. A build made
/// without one — a development run, or a fork someone compiled — would
/// otherwise treat a perfectly good licence as absent, because it has
/// nothing to check it against. Asking the gateway is the honest fallback:
/// the gateway is the party that decides anyway, and it is the one that can
/// refuse. The answer is remembered so the app is not useless on a train.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Verdict {
    /// Which token this was about, hashed — the token itself already lives
    /// in the secrets file and does not need a second copy.
    #[serde(default)]
    pub token_hash: String,
    #[serde(default)]
    pub tier: String,
    #[serde(default)]
    pub license_id: String,
    #[serde(default)]
    pub expires_at: i64,
    #[serde(default)]
    pub max_peers: u32,
    #[serde(default)]
    pub checked_at: i64,
}

/// A token's fingerprint, for matching a remembered verdict to a key.
pub fn fingerprint(token: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"velvetdesk-license-fingerprint/v1");
    hash.update(token.trim().as_bytes());
    format!("{:x}", hash.finalize())
}

pub fn read_verdict(paths: &Paths) -> Option<Verdict> {
    read_json::<Verdict>(&paths.root.join("cloud_verdict.json"))
        .ok()
        .flatten()
}

pub fn write_verdict(paths: &Paths, verdict: &Verdict) -> Result<()> {
    write_json(&paths.root.join("cloud_verdict.json"), verdict)
}

pub fn forget_verdict(paths: &Paths) {
    let _ = std::fs::remove_file(paths.root.join("cloud_verdict.json"));
}

/// What a licence grants, using the signature when this build can check one
/// and the gateway's remembered answer when it cannot.
pub fn read_here(paths: &Paths, token: &str) -> Entitlement {
    let local = read(token);
    if local.problem != "license.noPublicKey" {
        return local;
    }
    let Some(verdict) = read_verdict(paths) else {
        return local;
    };
    if verdict.token_hash != fingerprint(token) {
        return local;
    }
    let now = Utc::now().timestamp();
    let expired = verdict.expires_at != 0 && verdict.expires_at <= now;
    let plan = Plan::from_tier(&verdict.tier);
    Entitlement {
        plan: if expired { Plan::Free } else { plan },
        valid: !expired,
        license_id: verdict.license_id,
        tier: verdict.tier,
        expires_at: verdict.expires_at,
        days_left: if verdict.expires_at == 0 {
            i64::MAX / 86_400
        } else {
            (verdict.expires_at - now).div_euclid(86_400)
        },
        problem: if expired {
            "license.expired".into()
        } else {
            String::new()
        },
        limits: if expired {
            Limits::free()
        } else {
            Limits::paid(verdict.max_peers.max(1))
        },
    }
}

/// The limits in force right now, for code too deep to be handed them.
///
/// The agent's tools are the reason this exists: a tool call that would create
/// the eleventh dossier has to be refused where it is planned, several layers
/// below anything that knows about licences.
static CURRENT: RwLock<Limits> = RwLock::new(Limits {
    requests_per_day: Some(FREE_REQUESTS_PER_DAY),
    profiles: Some(FREE_PROFILES),
    men_per_profile: Some(FREE_MEN_PER_PROFILE),
    devices: 1,
    cloud: false,
    sync: false,
});

pub fn limits() -> Limits {
    *CURRENT.read()
}

pub fn set_limits(limits: Limits) {
    *CURRENT.write() = limits;
}

/// How many model calls today has already cost.
///
/// Kept next to the data rather than in settings: settings are a file the
/// operator is invited to edit, and this is a meter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Meter {
    /// Days since the epoch, so a comparison is a number and not a parse.
    #[serde(default)]
    pub day: i64,
    #[serde(default)]
    pub used: u32,
    /// The latest instant this meter has ever seen.
    ///
    /// Winding the clock back is the obvious way to get a fresh hundred
    /// requests, so the meter remembers how far time had got and refuses to
    /// go backwards: a day rolls over when the clock passes the high-water
    /// mark, not when it merely shows a different date.
    #[serde(default)]
    pub high_water: i64,
    /// Times the clock was found behind the high-water mark. Shown to nobody;
    /// it is here so support can tell a broken RTC from a wound-back one.
    #[serde(default)]
    pub rewinds: u32,
}

fn day_of(unix: i64) -> i64 {
    unix.div_euclid(86_400)
}

/// Roll the stored meter forward to `now`.
///
/// Time only ever moves forward here. A clock that reads earlier than the
/// high-water mark is ignored in favour of the mark, so winding the machine
/// back buys nothing; a clock wound *forward* does open the next day early,
/// and then the mark it left behind holds the meter shut until real time
/// catches up with it.
fn roll(stored: Meter, now: i64) -> Meter {
    let effective = now.max(stored.high_water);
    let mut meter = stored;
    if now < meter.high_water {
        meter.rewinds = meter.rewinds.saturating_add(1);
    }
    if day_of(effective) > meter.day {
        meter.day = day_of(effective);
        meter.used = 0;
    }
    meter.high_water = effective;
    meter
}

/// The meter as it stands, with the day rolled over if it is genuinely a new
/// one. Never writes; `charge` does that.
pub fn meter(paths: &Paths) -> Meter {
    let stored = read_json::<Meter>(&paths.meter_file())
        .ok()
        .flatten()
        .unwrap_or_default();
    roll(stored, Utc::now().timestamp())
}

/// Count one model call against the day, or refuse it.
///
/// Called once per operator action — a reply, a master-chat turn, a digest —
/// not once per HTTP request to the provider: a run that needs three calls to
/// answer is one thing the operator asked for.
pub fn charge(paths: &Paths) -> Result<Meter> {
    let mut meter = meter(paths);
    if let Some(cap) = limits().requests_per_day {
        if meter.used >= cap {
            return Err(AppError::message(
                "limit.requestsPerDay",
                serde_json::json!({ "cap": cap, "resetsIn": seconds_to_midnight() }),
            ));
        }
    }
    meter.used = meter.used.saturating_add(1);
    write_json(&paths.meter_file(), &meter)?;
    Ok(meter)
}

/// Seconds until the meter's day rolls over, counted from the high-water mark
/// so a wound-back clock does not promise an early reset.
fn seconds_to_midnight() -> i64 {
    let now = Utc::now().timestamp();
    let next = (day_of(now) + 1) * 86_400;
    (next - now).max(0)
}

/// Refuse the eleventh profile, and say which cap was hit.
pub fn check_profiles(existing: usize) -> Result<()> {
    match limits().profiles {
        Some(cap) if existing >= cap => Err(AppError::message(
            "limit.profiles",
            serde_json::json!({ "cap": cap }),
        )),
        _ => Ok(()),
    }
}

/// Refuse the eleventh dossier inside one profile.
pub fn check_men(existing: usize) -> Result<()> {
    match limits().men_per_profile {
        Some(cap) if existing >= cap => Err(AppError::message(
            "limit.menPerProfile",
            serde_json::json!({ "cap": cap }),
        )),
        _ => Ok(()),
    }
}

/// A nag counter: how many times the free-version notice has been shown, so
/// the UI can space it out instead of firing on every launch.
static NAGS: AtomicU32 = AtomicU32::new(0);

pub fn nagged() -> u32 {
    NAGS.fetch_add(1, Ordering::Relaxed)
}

/// Everything the UI needs to draw the plan: what is allowed, what is left,
/// and when it runs out.
#[derive(Debug, Clone, Serialize)]
pub struct PlanState {
    #[serde(flatten)]
    pub entitlement: Entitlement,
    pub used_today: u32,
    pub requests_left: Option<u32>,
    pub resets_in: i64,
    pub profiles_used: usize,
    pub expires_on: String,
}

pub fn plan_state(paths: &Paths, token: &str, profiles_used: usize) -> PlanState {
    let entitlement = read_here(paths, token);
    let meter = meter(paths);
    let requests_left = entitlement
        .limits
        .requests_per_day
        .map(|cap| cap.saturating_sub(meter.used));
    let expires_on = Utc
        .timestamp_opt(entitlement.expires_at, 0)
        .single()
        .map(|date| format!("{:04}-{:02}-{:02}", date.year(), date.month(), date.day()))
        .unwrap_or_default();
    PlanState {
        entitlement,
        used_today: meter.used,
        requests_left,
        resets_in: seconds_to_midnight(),
        profiles_used,
        expires_on,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_licence_is_the_free_plan() {
        let free = read("   ");
        assert_eq!(free.plan, Plan::Free);
        assert_eq!(free.problem, "license.missing");
        assert_eq!(free.limits.requests_per_day, Some(FREE_REQUESTS_PER_DAY));
        assert!(!free.limits.cloud);
    }

    #[test]
    fn nonsense_in_the_licence_field_does_not_unlock_anything() {
        let entitlement = read("VD.not-a-licence.at-all");
        assert_eq!(entitlement.plan, Plan::Free);
        assert!(!entitlement.valid);
    }

    /// A tier decides the ceiling on devices, and the free plan gets one.
    #[test]
    fn plans_read_from_the_tier_string() {
        assert_eq!(Plan::from_tier("business"), Plan::Business);
        assert_eq!(Plan::from_tier("Pro"), Plan::Pro);
        assert_eq!(Plan::from_tier(""), Plan::Free);
        assert_eq!(Limits::paid(2).devices, 2);
        assert!(Limits::paid(2).sync);
        // One device is a licence for one machine: nothing to sync with.
        assert!(!Limits::paid(1).sync);
    }

    #[test]
    fn caps_refuse_at_the_ceiling_and_not_before() {
        set_limits(Limits::free());
        assert!(check_profiles(FREE_PROFILES - 1).is_ok());
        assert!(check_profiles(FREE_PROFILES).is_err());
        assert!(check_men(FREE_MEN_PER_PROFILE).is_err());
        set_limits(Limits::paid(2));
        assert!(check_profiles(10_000).is_ok());
        assert!(check_men(10_000).is_ok());
        set_limits(Limits::free());
    }

    /// The state a machine is in after a hundred requests today.
    fn spent(now: i64) -> Meter {
        Meter {
            day: day_of(now),
            used: FREE_REQUESTS_PER_DAY,
            high_water: now,
            rewinds: 0,
        }
    }

    #[test]
    fn a_new_day_gives_the_requests_back() {
        let now = Utc::now().timestamp();
        let next = roll(spent(now), now + 86_400);
        assert_eq!(next.used, 0);
    }

    /// The whole point of the high-water mark.
    #[test]
    fn winding_the_clock_back_does_not_reset_the_day() {
        let now = Utc::now().timestamp();
        for back in [3_600, 86_400, 30 * 86_400, 400 * 86_400] {
            let next = roll(spent(now), now - back);
            assert_eq!(next.used, FREE_REQUESTS_PER_DAY, "wound back {back}s");
            assert_eq!(next.rewinds, 1);
            assert_eq!(next.high_water, now);
        }
    }

    /// Winding forward opens tomorrow early — and shuts it for the rest of
    /// today and all of tomorrow, because the mark stays where the jump left
    /// it. The tamperer ends up with fewer requests, not more.
    #[test]
    fn winding_the_clock_forward_is_borrowed_not_gained() {
        let now = Utc::now().timestamp();
        let jumped = roll(spent(now), now + 2 * 86_400);
        assert_eq!(jumped.used, 0);
        let back_to_real_time = roll(jumped, now);
        assert_eq!(back_to_real_time.day, day_of(now + 2 * 86_400));
        assert_eq!(back_to_real_time.high_water, now + 2 * 86_400);
        // Tomorrow arrives, and the meter is still on the day it jumped to.
        let tomorrow = roll(back_to_real_time, now + 86_400);
        assert_eq!(tomorrow.day, day_of(now + 2 * 86_400));
    }
}
