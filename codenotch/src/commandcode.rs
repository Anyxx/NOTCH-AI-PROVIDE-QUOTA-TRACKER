//! Command Code usage adapter, implemented from the upstream Codenotch's documented behaviour
//! (Sources/Providers/CommandCode{Credentials,Provider,Usage}.swift).
//!
//! Keys, several accounts at once: the list saved from the API-keys window (Credential Manager), the
//! `COMMAND_CODE_API_KEY` env var the desktop harness uses, and the app's own `~/.commandcode/auth.json`
//! (`apiKey`) — all borrowed read-only, never refreshed here. The same key from two places is one
//! account. With a single account the card reads as it always has; with more, it shows a block per
//! account (like 9Router's) and the ring takes the tightest quota among them.
//!
//! Four GET calls per account against the `/alpha` endpoints, `Authorization: Bearer <apiKey>`:
//!   1. `/alpha/whoami` → `org.id`, `user.userName`
//!   2. `/alpha/billing/credits?orgId=…` → `{ credits:{monthlyCredits}, windowLimits:{fiveHour,weekly} }`
//!   3. `/alpha/billing/subscriptions?orgId=…` → `{ data:{planId,currentPeriodStart,currentPeriodEnd} }`
//!   4. `/alpha/usage/summary?orgId=…&since=<periodStart>` → `{ totalCost, … }`
//! The ring is monthly spend over cap (totalCost + monthlyCredits); five-hour/weekly windows ride
//! along when the account has them. A cap of 0 means nothing metered yet: no windows, no numbers invented.

use crate::usage::{LimitWindow, QuotaGroup, UsageSnapshot};
use crate::AppState;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const POLL_SECS: u64 = 300;
const BACKOFF_MIN_SECS: u64 = 60;
const WHOAMI: &str = "https://api.commandcode.ai/alpha/whoami";
const CREDITS: &str = "https://api.commandcode.ai/alpha/billing/credits";
const SUBSCRIPTIONS: &str = "https://api.commandcode.ai/alpha/billing/subscriptions";
const SUMMARY: &str = "https://api.commandcode.ai/alpha/usage/summary";
/// Credential Manager caps one credential's blob at 2,560 bytes (CRED_MAX_CREDENTIAL_BLOB_SIZE)
const MAX_BLOB: usize = 2560;

static REFRESH: AtomicBool = AtomicBool::new(false);
static BACKOFF_UNTIL: AtomicU64 = AtomicU64::new(0);
/// How the last read of an account went. A failed account keeps the rows it last had — it is retried
/// on its own, not dropped from the card.
#[derive(Clone, Copy, PartialEq)]
enum Health {
    Ok,
    Rejected,
    Failed,
}

/// One account's standing: what to show, how the last read went, when it is next due, and how many
/// failures in a row (each one pushes the next attempt further out).
struct Slot {
    id: String,
    group: QuotaGroup,
    health: Health,
    next_at: u64,
    fails: u32,
}
static SLOTS: Mutex<Vec<Slot>> = Mutex::new(Vec::new());

/// A healthy account is re-read every POLL_SECS; a failed one comes back in 20 s, then 40, 80 … up to
/// the normal interval, so one account the network drops is not missing for a whole cycle. Command
/// Code's API is slow enough that four at a time with a 10 s timeout beats reading them one by one.
const RETRY_FIRST_SECS: u64 = 20;
const MAX_PARALLEL: usize = 4;
const TICK_SECS: u64 = 5;

pub fn request_refresh() {
    REFRESH.store(true, Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn auth_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".commandcode").join("auth.json"))
}

fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("commandcode.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into();
            }
            BACKOFF_UNTIL.store(s.backoff_until, Ordering::Relaxed);
            s
        })
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(), t);
    }
}

// ---------------- Accounts ----------------

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct SavedKey {
    pub id: String,
    #[serde(default)]
    pub label: String,
    pub key: String,
}

pub struct Account {
    pub id: String,
    /// saved | env | auth.json
    pub source: &'static str,
    pub label: String,
    pub key: String,
}

/// Stable per key, so the same key found in two places is one account
pub fn key_id(key: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(key.trim().as_bytes());
    digest.iter().take(5).map(|b| format!("{b:02x}")).collect()
}

/// Enough of the key to recognise it, never enough to use it
pub fn mask_key(k: &str) -> String {
    let n = k.chars().count();
    if n <= 8 {
        return "•".repeat(n);
    }
    let tail: String = k.chars().skip(n - 4).collect();
    format!("••••••••{tail}")
}

fn load_saved() -> Vec<SavedKey> {
    if let Some(t) = crate::secrets::get(crate::secrets::COMMANDCODE_ACCOUNTS) {
        if let Ok(v) = serde_json::from_str::<Vec<SavedKey>>(&t) {
            return v;
        }
    }
    // One-time move of the single key the window used to save, so an existing setup keeps working
    if let Some(k) = crate::secrets::get(crate::secrets::COMMANDCODE) {
        let v = vec![SavedKey { id: key_id(&k), label: String::new(), key: k }];
        if store_saved(&v).is_ok() {
            crate::secrets::delete(crate::secrets::COMMANDCODE);
        }
        return v;
    }
    Vec::new()
}

fn store_saved(v: &[SavedKey]) -> Result<(), String> {
    if v.is_empty() {
        crate::secrets::delete(crate::secrets::COMMANDCODE_ACCOUNTS);
        return Ok(());
    }
    let t = serde_json::to_string(v).map_err(|e| e.to_string())?;
    if t.len() > MAX_BLOB {
        return Err("that is more accounts than one credential holds — remove one first".into());
    }
    crate::secrets::set(crate::secrets::COMMANDCODE_ACCOUNTS, &t)
}

/// Adds a key (or renames it, when it is already there). Returns how many are saved.
pub fn add_key(key: &str, label: &str) -> Result<usize, String> {
    let key = key.trim();
    let mut v = load_saved();
    let id = key_id(key);
    match v.iter_mut().find(|e| e.id == id) {
        Some(e) if !label.is_empty() => e.label = label.to_string(),
        Some(_) => {}
        None => v.push(SavedKey { id, label: label.to_string(), key: key.to_string() }),
    }
    store_saved(&v)?;
    Ok(v.len())
}

pub fn remove_key(id: &str) -> Result<(), String> {
    let mut v = load_saved();
    v.retain(|e| e.id != id);
    store_saved(&v)
}

/// The window shows the account's own name once Command Code has told us what it is
fn remember_label(id: &str, label: &str) {
    let mut v = load_saved();
    if let Some(e) = v.iter_mut().find(|e| e.id == id && e.label != label) {
        e.label = label.to_string();
        let _ = store_saved(&v);
    }
}

fn auth_file_key() -> Option<(String, String)> {
    let text = std::fs::read_to_string(auth_path()?).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let key = v.get("apiKey")?.as_str()?.trim().to_string();
    let label = v.get("userName").and_then(|x| x.as_str()).unwrap_or("").to_string();
    if key.is_empty() { None } else { Some((label, key)) }
}

fn add_unique(out: &mut Vec<Account>, source: &'static str, label: String, key: String) {
    let key = key.trim().to_string();
    if key.is_empty() {
        return;
    }
    let id = key_id(&key);
    if !out.iter().any(|a| a.id == id) {
        out.push(Account { id, source, label, key });
    }
}

/// Every account to read: saved in the window, then the env var, then the app's auth.json
pub fn accounts() -> Vec<Account> {
    let mut out = Vec::new();
    for s in load_saved() {
        add_unique(&mut out, "saved", s.label, s.key);
    }
    if let Some(env) = std::env::var_os("COMMAND_CODE_API_KEY") {
        add_unique(&mut out, "env", String::new(), env.to_string_lossy().to_string());
    }
    if let Some((label, key)) = auth_file_key() {
        add_unique(&mut out, "auth.json", label, key);
    }
    out
}

/// Is Command Code present (any account, or the auth file on disk)? If not, no cell is shown.
pub fn present() -> bool {
    !accounts().is_empty() || auth_path().map(|p| p.is_file()).unwrap_or(false)
}

// ---------------- HTTP ----------------

pub enum TestErr {
    /// The server looked at the key and said no — it must not be stored
    Rejected(String),
    /// Could not be checked (offline, rate limited, server error) — says nothing about the key
    Other(String),
}

fn user_name(whoami: &serde_json::Value) -> Option<String> {
    whoami
        .pointer("/user/userName")
        .or_else(|| whoami.pointer("/user/name"))
        .and_then(|x| x.as_str())
        .map(String::from)
}

/// Checks a key against `/alpha/whoami` and returns the account name it belongs to.
pub fn test_key(key: &str) -> Result<String, TestErr> {
    match get(WHOAMI, key) {
        Ok(v) => Ok(user_name(&v).unwrap_or_else(|| "your account".into())),
        Err(FetchErr::NeedsAuth) => Err(TestErr::Rejected("Command Code rejected this key — check it was copied in full".into())),
        Err(FetchErr::RateLimited(s)) => Err(TestErr::Other(format!("rate limited, retrying in {s}s"))),
        Err(FetchErr::Other(e)) => Err(TestErr::Other(e)),
    }
}

enum FetchErr {
    NeedsAuth,
    RateLimited(u64),
    Other(String),
}

fn get(url: &str, token: &str) -> Result<serde_json::Value, FetchErr> {
    let resp = ureq::get(url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json")
        .set("User-Agent", "command-code-desktop")
        .set("x-command-code-version", "desktop")
        .timeout(Duration::from_secs(10))
        .call();
    match resp {
        Ok(r) => r.into_json().map_err(|e| FetchErr::Other(format!("parse: {e}"))),
        Err(ureq::Error::Status(401, _)) | Err(ureq::Error::Status(403, _)) => Err(FetchErr::NeedsAuth),
        Err(ureq::Error::Status(429, r)) => {
            let ra = r.header("retry-after").and_then(|s| s.trim().parse::<u64>().ok()).unwrap_or(0);
            Err(FetchErr::RateLimited(ra.max(BACKOFF_MIN_SECS)))
        }
        Err(ureq::Error::Status(code, _)) => Err(FetchErr::Other(format!("HTTP {code}"))),
        Err(e) => Err(FetchErr::Other(format!("{e}"))),
    }
}

fn with_query(url: &str, pairs: &[(&str, Option<&str>)]) -> String {
    let items: Vec<String> = pairs
        .iter()
        .filter_map(|(k, v)| v.filter(|s| !s.is_empty()).map(|s| format!("{k}={}", urlenc(s))))
        .collect();
    if items.is_empty() {
        url.to_string()
    } else {
        format!("{url}?{}", items.join("&"))
    }
}

fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn num(v: Option<&serde_json::Value>) -> Option<f64> {
    v.and_then(|x| x.as_f64())
}

/// ISO 8601, unix seconds, or unix milliseconds. Zero/absent is "no reset".
fn parse_date(v: Option<&serde_json::Value>) -> Option<u64> {
    if let Some(s) = v.and_then(|x| x.as_str()) {
        return chrono::DateTime::parse_from_rfc3339(s).ok().map(|d| d.timestamp_millis().max(0) as u64);
    }
    let n = num(v)?;
    if n <= 0.0 {
        return None;
    }
    Some(if n > 1_000_000_000_000.0 { n as u64 } else { (n * 1000.0) as u64 })
}

/// `currentPeriodStart` accepted either as RFC3339 or epoch seconds/ms, re-emitted as RFC3339
/// (`since` on the summary endpoint expects a timestamp string).
fn parse_iso_or_epoch(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        if chrono::DateTime::parse_from_rfc3339(s).is_ok() {
            return Some(s.to_string());
        }
    }
    let ms = parse_date(Some(v))?;
    let secs = (ms / 1000) as i64;
    chrono::DateTime::from_timestamp(secs, 0).map(|d| d.to_rfc3339())
}

fn window_from(entry: Option<&serde_json::Value>, id: &str, label: &str) -> Option<LimitWindow> {
    let entry = entry?;
    let cap = num(entry.get("cap")).filter(|c| *c > 0.0)?;
    let used = num(entry.get("used")).unwrap_or(0.0);
    Some(LimitWindow {
        id: id.into(),
        label: label.into(),
        used: (used / cap).clamp(0.0, 1.0),
        resets_at: parse_date(entry.get("resetAt")),
        ..Default::default()
    })
}

fn plan_name(plan_id: Option<&str>) -> Option<String> {
    let id = plan_id?;
    if id.is_empty() {
        return None;
    }
    let key = id.to_ascii_lowercase().replace('-', "_");
    Some(if key.contains("goat") { "GOAT".into() } else { id.to_string() })
}

struct Reading {
    windows: Vec<LimitWindow>,
    plan: Option<String>,
    user: Option<String>,
}

fn read_account(token: &str) -> Result<Reading, FetchErr> {
    let whoami = get(WHOAMI, token)?;
    let org_id = whoami.pointer("/org/id").and_then(|x| x.as_str()).map(String::from);

    let credits = get(&with_query(CREDITS, &[("orgId", org_id.as_deref())]), token)?;
    let subscription = get(&with_query(SUBSCRIPTIONS, &[("orgId", org_id.as_deref())]), token)?;

    let sub_data = subscription.get("data").cloned().unwrap_or_else(|| subscription.clone());
    let plan = plan_name(sub_data.get("planId").and_then(|x| x.as_str()));
    let period_start = sub_data.get("currentPeriodStart").and_then(parse_iso_or_epoch);
    let period_end = parse_date(sub_data.get("currentPeriodEnd"));

    let summary_url = with_query(SUMMARY, &[("orgId", org_id.as_deref()), ("since", period_start.as_deref())]);
    let summary = get(&summary_url, token)?;

    let credits_obj = credits.get("credits").cloned().unwrap_or_default();
    let limits = credits.get("windowLimits").cloned().unwrap_or_default();
    let used = num(summary.get("totalCost")).unwrap_or(0.0);
    let remaining = num(credits_obj.get("monthlyCredits")).unwrap_or(0.0);
    let cap = if used > 0.0 || remaining > 0.0 { used + remaining } else { 0.0 };

    let mut windows = Vec::new();
    if cap > 0.0 {
        windows.push(LimitWindow {
            id: "monthly".into(),
            label: "Monthly limit".into(),
            used: (used / cap).clamp(0.0, 1.0),
            resets_at: period_end,
            ..Default::default()
        });
    }
    if let Some(w) = window_from(limits.get("fiveHour"), "fiveHour", "5h limit") {
        windows.push(w);
    }
    if let Some(w) = window_from(limits.get("weekly"), "weekly", "Weekly limit") {
        windows.push(w);
    }
    Ok(Reading { windows, plan, user: user_name(&whoami) })
}

// ---------------- Putting it together ----------------

/// One snapshot from the per-account groups: the familiar single-account card for one, a block
/// per account (and the tightest quota on the ring) for more.
fn compose(mut groups: Vec<QuotaGroup>, ok: usize, rejected: usize) -> UsageSnapshot {
    let mut snap = UsageSnapshot::default();
    if groups.len() == 1 {
        let g = groups.pop().unwrap();
        let has_rows = !g.rows.is_empty();
        snap.windows = g.rows;
        snap.note = g.plan.unwrap_or_default();
        snap.status = match (ok, rejected, has_rows) {
            (1, _, true) => "ok",
            (1, _, false) => "none",
            (_, 1, _) => "needsAuth",
            (_, _, true) => "stale",
            _ => "error",
        }
        .into();
        if let Some(m) = g.message {
            snap.note = m;
        }
        if ok == 1 {
            snap.fetched_at = now_ms();
        }
        return snap;
    }
    snap.status = if ok > 0 {
        "ok"
    } else if rejected == groups.len() {
        "needsAuth"
    } else {
        "stale"
    }
    .into();
    if ok > 0 {
        snap.fetched_at = now_ms();
    }
    snap.note = format!("{} accounts", groups.len());
    if let Some(mut h) = crate::router9::headline(&groups) {
        h.id = "tightest".into();
        snap.windows = vec![h];
    }
    snap.groups = groups;
    snap
}

fn group_of(a: &Account, r: Reading) -> QuotaGroup {
    let fallback = if a.label.is_empty() { mask_key(&a.key) } else { a.label.clone() };
    QuotaGroup {
        provider: "commandcode".into(),
        title: "Command Code".into(),
        account: r.user.unwrap_or(fallback),
        plan: r.plan,
        message: if r.windows.is_empty() { Some("Command Code has nothing metered on this account yet".into()) } else { None },
        rows: r.windows,
    }
}

/// The accounts whose turn it is. An account not read yet is due at once.
fn due_accounts(accts: &[Account], now: u64) -> Vec<usize> {
    let slots = SLOTS.lock().unwrap();
    accts
        .iter()
        .enumerate()
        .filter(|(_, a)| slots.iter().find(|s| s.id == a.id).map(|s| s.next_at <= now).unwrap_or(true))
        .map(|(i, _)| i)
        .collect()
}

/// Reads up to MAX_PARALLEL accounts at a time: one slow account no longer holds up the others
fn read_batch(accts: &[&Account]) -> Vec<Result<Reading, FetchErr>> {
    let mut out = Vec::with_capacity(accts.len());
    for chunk in accts.chunks(MAX_PARALLEL) {
        let results: Vec<_> = std::thread::scope(|s| {
            let handles: Vec<_> = chunk.iter().map(|a| s.spawn(|| read_account(&a.key))).collect();
            handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(FetchErr::Other("read panicked".into())))).collect()
        });
        out.extend(results);
    }
    out
}

/// Updates one account's slot from its reading, and says when to come back to it
fn apply_result(a: &Account, res: Result<Reading, FetchErr>, now: u64) {
    let mut slots = SLOTS.lock().unwrap();
    let idx = slots.iter().position(|s| s.id == a.id);
    let prev_group = idx.map(|i| slots[i].group.clone());
    let fails = idx.map(|i| slots[i].fails).unwrap_or(0);
    let (group, health, wait, fails) = match res {
        Ok(r) => {
            if let (Some(u), "saved") = (&r.user, a.source) {
                remember_label(&a.id, u);
            }
            (group_of(a, r), Health::Ok, POLL_SECS, 0)
        }
        Err(e) => {
            let (text, health, wait) = match e {
                FetchErr::NeedsAuth => (
                    "Command Code rejected this key — remove it or add a new one".to_string(),
                    Health::Rejected,
                    POLL_SECS,
                ),
                FetchErr::RateLimited(secs) => (format!("Rate limited — retrying in {secs}s"), Health::Failed, secs.max(BACKOFF_MIN_SECS)),
                FetchErr::Other(m) => {
                    crate::applog(&format!("commandcode: read failed for account {} ({m})", a.id));
                    let wait = (RETRY_FIRST_SECS << fails.min(4)).min(POLL_SECS);
                    (format!("Couldn't refresh — retrying in {wait}s"), Health::Failed, wait)
                }
            };
            let mut g = prev_group.unwrap_or_else(|| QuotaGroup {
                provider: "commandcode".into(),
                title: "Command Code".into(),
                account: if a.label.is_empty() { mask_key(&a.key) } else { a.label.clone() },
                ..Default::default()
            });
            g.message = Some(text);
            (g, health, wait, fails.saturating_add(1))
        }
    };
    let slot = Slot { id: a.id.clone(), group, health, next_at: now + wait * 1000, fails };
    match idx {
        Some(i) => slots[i] = slot,
        None => slots.push(slot),
    }
}

/// The card as it stands: every account in order, with whatever each last had
fn snapshot_now(accts: &[Account]) -> UsageSnapshot {
    let slots = SLOTS.lock().unwrap();
    let mut groups = Vec::new();
    let (mut ok, mut rejected) = (0usize, 0usize);
    for a in accts {
        if let Some(s) = slots.iter().find(|s| s.id == a.id) {
            match s.health {
                Health::Ok => ok += 1,
                Health::Rejected => rejected += 1,
                Health::Failed => {}
            }
            groups.push(s.group.clone());
        }
    }
    compose(groups, ok, rejected)
}

fn broadcast(app: &AppHandle, snap: UsageSnapshot) {
    let st = app.state::<AppState>();
    *st.commandcode.lock().unwrap() = snap.clone();
    persist(&snap);
    let _ = app.emit("commandcode", &snap);
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        {
            let st = app.state::<AppState>();
            let snap = st.commandcode.lock().unwrap().clone();
            let _ = app.emit("commandcode", &snap);
        }
        loop {
            let forced = REFRESH.swap(false, Ordering::Relaxed);
            let accts = accounts();
            if accts.is_empty() {
                SLOTS.lock().unwrap().clear();
                // Command Code installed but without a key: say so, rather than hiding the cell
                let snap = if present() {
                    UsageSnapshot { status: "needsAuth".into(), note: "No Command Code API key found".into(), ..Default::default() }
                } else {
                    UsageSnapshot { status: "absent".into(), ..Default::default() }
                };
                broadcast(&app, snap);
                std::thread::sleep(Duration::from_secs(TICK_SECS));
                continue;
            }
            // Accounts removed in the settings window leave with their slot
            SLOTS.lock().unwrap().retain(|s| accts.iter().any(|a| a.id == s.id));
            let now = now_ms();
            let due: Vec<usize> = if forced { (0..accts.len()).collect() } else { due_accounts(&accts, now) };
            if !due.is_empty() {
                let batch: Vec<&Account> = due.iter().map(|i| &accts[*i]).collect();
                let results = read_batch(&batch);
                let now = now_ms();
                for (a, res) in batch.into_iter().zip(results) {
                    apply_result(a, res, now);
                }
                broadcast(&app, snapshot_now(&accts));
            }
            std::thread::sleep(Duration::from_secs(TICK_SECS));
        }
    });
}

/// For doctor: contains no secrets
pub fn probe() -> String {
    let accts = accounts();
    if accts.is_empty() {
        return if auth_path().map(|p| p.is_file()).unwrap_or(false) {
            "Command Code: auth.json present but has no apiKey".into()
        } else {
            "Command Code: no accounts (none saved, no COMMAND_CODE_API_KEY, no auth.json)".into()
        };
    }
    let list: Vec<String> = accts.iter().map(|a| format!("{} [{}]", a.id, a.source)).collect();
    format!("Command Code: {} account(s): {}", accts.len(), list.join(", "))
}

#[cfg(test)]
mod tests {
    #[test]
    #[ignore = "calls api.commandcode.ai"]
    fn bogus_key_is_rejected_not_merely_unverified() {
        assert!(matches!(super::test_key("definitely-not-a-key"), Err(super::TestErr::Rejected(_))));
    }

    #[test]
    fn the_same_key_from_two_places_is_one_account() {
        let mut out = Vec::new();
        super::add_unique(&mut out, "saved", "me".into(), "ccsk_abc123".into());
        super::add_unique(&mut out, "auth.json", String::new(), "  ccsk_abc123 ".into());
        super::add_unique(&mut out, "env", String::new(), "ccsk_other".into());
        assert_eq!(out.iter().map(|a| a.source).collect::<Vec<_>>(), ["saved", "env"]);
        assert_eq!(super::key_id("ccsk_abc123"), super::key_id(" ccsk_abc123 "));
    }

    fn group(name: &str, used: f64) -> super::QuotaGroup {
        super::QuotaGroup {
            provider: "commandcode".into(),
            title: "Command Code".into(),
            account: name.into(),
            rows: vec![super::LimitWindow { id: "weekly".into(), label: "Weekly limit".into(), used, ..Default::default() }],
            ..Default::default()
        }
    }

    #[test]
    fn one_account_keeps_the_classic_card_and_more_become_blocks() {
        let one = super::compose(vec![group("a", 0.4)], 1, 0);
        assert_eq!(one.status, "ok");
        assert!(one.groups.is_empty());
        assert_eq!(one.windows.len(), 1);

        let two = super::compose(vec![group("a", 0.4), group("b", 0.7)], 2, 0);
        assert_eq!(two.groups.len(), 2);
        // Several keys are used in turn: the ring follows the account with the most left
        assert!((two.windows[0].used - 0.4).abs() < 1e-9, "the ring follows the account with the most left");

        let rejected = super::compose(vec![group("a", 0.0), group("b", 0.0)], 0, 2);
        assert_eq!(rejected.status, "needsAuth");
    }
}
