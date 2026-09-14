//! Per-model token pricing for the model picker's cost surfaces (the row
//! gradient bars and the tray's cost breakdown), in the spirit of Devin
//! Desktop's selector: at a glance how expensive a model is, and what its
//! input / cached-input / output dollars-per-1M actually are.
//!
//! SOURCES, merged (CPA wins — it prices the user's actual proxied fleet):
//! - **CPA proxy** (optional, user-configured): the cpa-manager-plus
//!   management endpoint `GET {url}` returning the model-prices map,
//!   `Authorization: Bearer` when a token is set. URL + token come from
//!   `{data_dir}/price-source.json` (`{"url": …, "token": …}`) with
//!   `ZERON_CPA_PRICES_URL` / `ZERON_CPA_PRICES_TOKEN` overriding.
//! - **models.dev** (always): the public `https://models.dev/api.json`
//!   catalog — the same data pi's own model store ships — so mainstream and
//!   proxied models price without any configuration.
//!
//! The merged table persists to `{data_dir}/model-prices.json` and outlives
//! fetch failures: a machine that was offline once still prices every picker
//! forever after. Refresh is stale-while-revalidate (12h TTL) on the tokio
//! bridge; consumers observe [`PricingChanged`] and re-render.
//!
//! Matching is spelling-tolerant rather than exact: catalogs decorate ids
//! with provider prefixes (`cpa/opencode-go/glm-5.3`), effort suffixes
//! (`gpt-6-astra-medium`), and context variants (`opus[1m]`) that price
//! tables don't carry. [`price_key`] folds all of those away on both sides.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use gpui::{App, Global, Hsla};
use serde::{Deserialize, Serialize};

/// How stale a fetched table may get before a refresh is attempted.
const REFRESH_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// The public catalog priced when no CPA source is configured (and under it,
/// for models the proxy hasn't priced).
const MODELS_DEV_URL: &str = "https://models.dev/api.json";

/// Dollars per 1M tokens, the models.dev convention. `cached_input` is the
/// cache READ price (what a repeat turn pays); cache WRITE is not surfaced.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ModelPricing {
    pub input: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input: Option<f64>,
    pub output: f64,
}

/// Notify observers (open pickers) that the table changed.
#[derive(Default)]
pub struct PricingChanged;

impl Global for PricingChanged {}

pub fn bump_pricing(cx: &mut App) {
    cx.default_global::<PricingChanged>();
}

/// The app-wide table + bookkeeping, a [`Global`] so every picker shares one
/// cache and one refresh flight.
#[derive(Default)]
pub struct PricingState {
    table: Option<Arc<HashMap<String, ModelPricing>>>,
    fetched_at: Option<SystemTime>,
    /// A refresh is in flight — `ensure_loaded` re-entry does not re-spawn.
    loading: bool,
}

impl Global for PricingState {}

/// The persisted cache: merged table + fetch stamp (unix seconds).
#[derive(Debug, Default, Serialize, Deserialize)]
struct PriceCacheFile {
    #[serde(default)]
    updated_at: Option<u64>,
    #[serde(default)]
    prices: HashMap<String, ModelPricing>,
}

/// Optional CPA price source, from `{data_dir}/price-source.json`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
struct PriceSourceConfig {
    url: Option<String>,
    token: Option<String>,
}

impl PriceSourceConfig {
    /// Env overrides win (`ZERON_CPA_PRICES_URL` / `ZERON_CPA_PRICES_TOKEN`),
    /// matching the crate's other `ZERON_*` knobs.
    fn resolve(data_dir: &Path) -> Self {
        let from_file = std::fs::read_to_string(source_path(data_dir))
            .ok()
            .and_then(|text| serde_json::from_str::<PriceSourceConfig>(&text).ok())
            .unwrap_or_default();
        Self {
            url: std::env::var("ZERON_CPA_PRICES_URL")
                .ok()
                .filter(|u| !u.is_empty())
                .or(from_file.url),
            token: std::env::var("ZERON_CPA_PRICES_TOKEN")
                .ok()
                .filter(|t| !t.is_empty())
                .or(from_file.token),
        }
    }
}

pub fn source_path(data_dir: &Path) -> PathBuf {
    data_dir.join("price-source.json")
}

pub fn cache_path(data_dir: &Path) -> PathBuf {
    data_dir.join("model-prices.json")
}

/// Load the disk cache synchronously (small file) and kick a background
/// refresh when stale. Idempotent; the first stale caller wins the flight.
pub fn ensure_loaded(data_dir: &Path, cx: &mut App) {
    let should_fetch = {
        let state = cx.default_global::<PricingState>();
        let cold = state.table.is_none() && !state.loading;
        if cold
            && let Ok(text) = std::fs::read_to_string(cache_path(data_dir))
            && let Ok(file) = serde_json::from_str::<PriceCacheFile>(&text)
            && !file.prices.is_empty()
        {
            state.table = Some(Arc::new(file.prices));
            state.fetched_at = file
                .updated_at
                .and_then(|secs| UNIX_EPOCH.checked_add(Duration::from_secs(secs)));
        }
        let stale = state
            .fetched_at
            .map(|at| at.elapsed().unwrap_or(REFRESH_TTL) >= REFRESH_TTL)
            .unwrap_or(true);
        let go = stale && !state.loading;
        if go {
            state.loading = true;
        }
        go
    };
    if !should_fetch {
        return;
    }
    // Unit tests get the disk cache only: a live fetch would hit the network
    // from `cargo test` and race the test runtime's tokio teardown.
    if cfg!(test) {
        return;
    }
    let dir = data_dir.to_path_buf();
    let fetch = gpui_tokio::Tokio::spawn(cx, async move { refresh(&dir).await });
    cx.spawn(async move |cx| {
        let (table, at) = fetch.await.ok().unwrap_or((None, None));
        cx.update(|cx| {
            let state = cx.global_mut::<PricingState>();
            state.loading = false;
            // A failed refresh keeps the previous table AND its stamp, so the
            // next ensure retries after the TTL rather than on every render.
            if let Some(table) = table {
                state.table = Some(Arc::new(table));
                state.fetched_at = at;
            }
            bump_pricing(cx);
        });
    })
    .detach();
}

/// Resolve a catalog model's price: the folded key first, then the
/// effort-stripped base. `None` renders nothing — a model the sources don't
/// price stays unpriced, never a guessed number.
pub fn pricing_for(model_id: &str, cx: &App) -> Option<ModelPricing> {
    let state = cx.try_global::<PricingState>()?;
    let table = state.table.as_ref()?;
    let key = price_key(model_id);
    lookup(table, &key)
}

fn lookup(table: &HashMap<String, ModelPricing>, key: &str) -> Option<ModelPricing> {
    table
        .get(key)
        .or_else(|| {
            let base = strip_effort_suffixes(key);
            (base != key).then(|| table.get(&base)).flatten()
        })
        .copied()
}

/// Fetch both sources and merge (CPA wins per key); persist the merged table.
/// `(None, None)` — "keep the previous table and stamp" — when BOTH sources
/// failed (or produced nothing): a transient outage must not wipe a cache.
async fn refresh(
    dir: &Path,
) -> (
    Option<HashMap<String, ModelPricing>>,
    Option<SystemTime>,
) {
    let config = PriceSourceConfig::resolve(dir);
    let Some(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .ok()
    else {
        return (None, None);
    };

    let mut merged: HashMap<String, ModelPricing> = HashMap::new();
    let mut any_ok = false;

    // models.dev first, so the CPA map (fetched second) wins per key.
    if let Some(text) = get_text(&client, client.get(MODELS_DEV_URL)).await {
        let parsed = parse_models_dev(&text);
        if !parsed.is_empty() {
            merged.extend(parsed);
            any_ok = true;
        }
    }
    if let Some(url) = config.url {
        let request = match &config.token {
            Some(token) => client.get(&url).bearer_auth(token),
            None => client.get(&url),
        };
        if let Some(text) = get_text(&client, request).await {
            let parsed = parse_cpa(&text);
            if !parsed.is_empty() {
                merged.extend(parsed);
                any_ok = true;
            }
        }
    }
    if !any_ok {
        return (None, None);
    }

    // Persist best-effort; a read-only data dir only costs a re-fetch.
    let now = SystemTime::now();
    let file = PriceCacheFile {
        updated_at: now.duration_since(UNIX_EPOCH).ok().map(|d| d.as_secs()),
        prices: merged.clone(),
    };
    if let Ok(json) = serde_json::to_vec_pretty(&file)
        && std::fs::create_dir_all(dir).is_ok()
    {
        let tmp = dir.join(format!("model-prices.json.{}.tmp", std::process::id()));
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, cache_path(dir));
        }
    }
    (Some(merged), Some(now))
}

async fn get_text(
    _client: &reqwest::Client,
    request: reqwest::RequestBuilder,
) -> Option<String> {
    let response = request.send().await.ok()?;
    let response = response.error_for_status().ok()?;
    response.text().await.ok()
}

/// models.dev api.json: `{ provider: { models: { id: { cost: { … } } } } }`.
/// Tolerant walk — entries without a usable `cost` are skipped.
pub fn parse_models_dev(text: &str) -> HashMap<String, ModelPricing> {
    let mut out = HashMap::new();
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return out;
    };
    let Some(providers) = root.as_object() else {
        return out;
    };
    for provider in providers.values() {
        let Some(models) = provider.get("models").and_then(|m| m.as_object()) else {
            continue;
        };
        for (id, model) in models {
            if let Some(pricing) = parse_cost(model.get("cost")) {
                out.insert(price_key(id), pricing);
            }
        }
    }
    out
}

/// CPA management model-prices. Accepts the map bare (`{ model: {…} }`) or
/// wrapped (`{"prices": { model: {…} } }`); each entry's keys may be spelled
/// models.dev-style (`input`/`output`/`cache_read`) or proxy-style
/// (`prompt`/`completion`/`cacheRead`).
pub fn parse_cpa(text: &str) -> HashMap<String, ModelPricing> {
    let mut out = HashMap::new();
    let Ok(root) = serde_json::from_str::<serde_json::Value>(text) else {
        return out;
    };
    let entries = root
        .get("prices")
        .and_then(|p| p.as_object())
        .or_else(|| root.as_object());
    let Some(entries) = entries else {
        return out;
    };
    for (id, entry) in entries {
        let (Some(input), Some(output)) = (
            number(entry, &["input", "prompt"]),
            number(entry, &["output", "completion"]),
        ) else {
            continue;
        };
        out.insert(
            price_key(id),
            ModelPricing {
                input,
                cached_input: number(entry, &["cache_read", "cacheRead"]),
                output,
            },
        );
    }
    out
}

fn parse_cost(cost: Option<&serde_json::Value>) -> Option<ModelPricing> {
    let cost = cost?;
    Some(ModelPricing {
        input: number(cost, &["input", "prompt"])?,
        cached_input: number(cost, &["cache_read", "cacheRead"]),
        output: number(cost, &["output", "completion"])?,
    })
}

fn number(entry: &serde_json::Value, keys: &[&str]) -> Option<f64> {
    let object = entry.as_object()?;
    keys.iter()
        .filter_map(|key| object.get(*key))
        .find_map(|value| value.as_f64())
        .filter(|value| value.is_finite() && *value > 0.0)
}

/// Fold a model id (or price-table key) to its matching key: last path
/// segment, context variants dropped, effort suffixes dropped, separators and
/// case folded away. `cpa/opencode-go/GLM-5.3` → `glm53`;
/// `gpt-6-astra-medium` → `gpt6astra`; `opus[1m]` → `opus`.
pub fn price_key(id: &str) -> String {
    let segment = id.rsplit('/').next().unwrap_or(id);
    let segment = segment.split(['[', ' ']).next().unwrap_or(segment);
    strip_effort_suffixes(segment)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Repeatedly peel KNOWN effort/decoration suffixes off a dash-delimited id.
/// Deliberately conservative: only tokens that mean reasoning effort or a
/// thinking marker are peeled — `muse-spark-1.3-contributor` keeps its tail.
pub fn strip_effort_suffixes(id: &str) -> String {
    const SUFFIXES: [&str; 10] = [
        "thinking", "ultrathink", "ultracode", "minimal", "xhigh", "medium", "high", "low", "max",
        "ultra",
    ];
    let mut current = id.to_string();
    while let Some((base, last)) = current.rsplit_once('-') {
        if base.is_empty() {
            break;
        }
        let folded = last
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>();
        if !SUFFIXES
            .iter()
            .any(|suffix| folded.eq_ignore_ascii_case(suffix))
        {
            break;
        }
        current = base.to_string();
    }
    current
}

// ---------------------------------------------------------------------------
// Cost → visual scale (the gradient bars)
// ---------------------------------------------------------------------------

/// The cheap end of the scale: $0.15 / 1M blended — most open models sit here.
const COST_FLOOR: f64 = 0.15;
/// The expensive end: flagship-tier blended cost (~$25 in / $75 out).
const COST_CEILING: f64 = 75.0;

/// A model's position on the cost spectrum, 0 (near-free) → 1 (premium).
/// Output is weighted higher — agentic turns are output-dominated. Log-scaled:
/// $0.2 → $2 → $20 spread evenly, the way price differences actually feel.
pub fn cost_position(pricing: &ModelPricing) -> f32 {
    let blended = 0.35 * pricing.input + 0.65 * pricing.output;
    let clamped = blended.clamp(COST_FLOOR, COST_CEILING);
    let t = (clamped.log10() - COST_FLOOR.log10()) / (COST_CEILING.log10() - COST_FLOOR.log10());
    t as f32
}

/// The gradient the bars paint: Devin-Desktop's cheap→premium ramp.
pub const COST_STOPS: [(f32, u32); 5] = [
    (0.0, 0x2ECC71),  // green — near-free
    (0.35, 0xEAB308), // yellow
    (0.6, 0xF97316),  // orange
    (0.8, 0xEC4899),  // pink
    (1.0, 0xA855F7),  // purple — premium
];

/// The gradient color at position `t` (0..1): the marker dot samples this so
/// a row's dot and the track always agree.
pub fn cost_color_at(t: f32) -> Hsla {
    let t = t.clamp(0.0, 1.0);
    let mut lower = COST_STOPS[0];
    let mut upper = *COST_STOPS.last().unwrap();
    for pair in COST_STOPS.windows(2) {
        if t >= pair[0].0 && t <= pair[1].0 {
            lower = pair[0];
            upper = pair[1];
            break;
        }
    }
    let span = (upper.0 - lower.0).max(f32::EPSILON);
    let local = (t - lower.0) / span;
    lerp_rgb(lower.1, upper.1, local)
}

fn lerp_rgb(from: u32, to: u32, t: f32) -> Hsla {
    let channel = |color: u32, shift: u32| ((color >> shift) & 0xFF) as f32 / 255.0;
    let mix = |shift: u32| {
        let a = channel(from, shift);
        let b = channel(to, shift);
        a + (b - a) * t
    };
    gpui::rgb(
        ((mix(16) * 255.0).round() as u32) << 16
            | ((mix(8) * 255.0).round() as u32) << 8
            | (mix(0) * 255.0).round() as u32,
    )
    .into()
}

/// "$1.2", "$0.12", "$6" — trailing zeros stripped, the way the reference
/// selector prints per-1M prices. Sub-cent prices keep enough decimals to
/// stay non-zero ("$0.004"): a cache-read price of $0.004 must never read
/// as free.
pub fn format_price(value: f64) -> String {
    let digits = if value >= 1.0 {
        1
    } else if value >= 0.1 {
        2
    } else {
        3
    };
    let mut text = format!("${value:.digits$}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pricing(input: f64, output: f64) -> ModelPricing {
        ModelPricing {
            input,
            cached_input: None,
            output,
        }
    }

    #[test]
    fn price_key_folds_prefixes_variants_and_effort() {
        assert_eq!(price_key("cpa/opencode-go/glm-5.3"), "glm53");
        assert_eq!(price_key("gpt-6-astra-medium"), "gpt6astra");
        assert_eq!(price_key("opus[1m]"), "opus");
        assert_eq!(price_key("claude-sonnet-4-6-thinking"), "claudesonnet46");
        assert_eq!(price_key("GLM-5.3"), "glm53");
        assert_eq!(
            price_key("muse-spark-1.3-contributor"),
            "musespark13contributor"
        );
    }

    #[test]
    fn strip_effort_never_empties_or_eats_unknown_tails() {
        assert_eq!(strip_effort_suffixes("swe-2-max"), "swe-2");
        assert_eq!(strip_effort_suffixes("high"), "high");
        assert_eq!(
            strip_effort_suffixes("muse-spark-1.3-contributor"),
            "muse-spark-1.3-contributor"
        );
    }

    #[test]
    fn lookup_falls_through_to_effort_stripped_base() {
        let table = HashMap::from([(price_key("gpt-6-astra"), pricing(1.2, 6.0))]);
        let found = lookup(&table, &price_key("gpt-6-astra-medium"));
        assert_eq!(found.map(|p| p.output), Some(6.0));
    }

    #[test]
    fn parse_models_dev_walks_nested_cost() {
        let text = r#"{
            "anthropic": {"models": {
                "claude-opus-5": {"cost": {"input": 10.0, "output": 50.0, "cache_read": 1.0, "cache_write": 12.5}}
            }},
            "openai": {"models": {
                "gpt-5.6-sol": {"cost": {"input": 1.2, "output": 6.0}},
                "freebie": {}
            }}
        }"#;
        let table = parse_models_dev(text);
        assert_eq!(table.len(), 2);
        let opus = table.get(&price_key("claude-opus-5")).unwrap();
        assert_eq!(opus.input, 10.0);
        assert_eq!(opus.cached_input, Some(1.0));
        assert!(table.get(&price_key("freebie")).is_none());
    }

    #[test]
    fn parse_cpa_reads_both_spellings_and_wrappers() {
        let wrapped = r#"{"prices": {
            "opencode-go/muse-spark-1.3": {"prompt": 0.5, "completion": 2.0, "cacheRead": 0.1},
            "glm-5.3": {"input": 0.2, "output": 0.8}
        }}"#;
        let table = parse_cpa(wrapped);
        let muse = table
            .get(&price_key("cpa/opencode-go/muse-spark-1.3"))
            .unwrap();
        assert_eq!(muse.input, 0.5);
        assert_eq!(muse.cached_input, Some(0.1));
        assert_eq!(table.get(&price_key("glm-5.3")).unwrap().output, 0.8);
    }

    #[test]
    fn parse_rejects_nonpositive_and_garbage() {
        assert!(parse_cpa("not json").is_empty());
        assert!(parse_cpa(r#"{"x": {"input": 0, "output": 1}}"#).is_empty());
        assert!(parse_cpa(r#"{"x": {"input": -1, "output": 1}}"#).is_empty());
    }

    #[test]
    fn cost_position_is_monotone_and_bounded() {
        let free = cost_position(&pricing(0.05, 0.2));
        let mid = cost_position(&pricing(1.2, 6.0));
        let premium = cost_position(&pricing(25.0, 75.0));
        assert!(free < mid && mid < premium);
        assert!((0.0..=1.0).contains(&free));
        assert!(premium > 0.9); // near the ceiling…
        // …and past it the log scale clamps.
        assert_eq!(cost_position(&pricing(250.0, 750.0)), 1.0);
        // Output dominates: a 10x output jump moves further than a 10x input one.
        let input_heavy = cost_position(&pricing(1.0, 1.0));
        let output_heavy = cost_position(&pricing(1.0, 10.0));
        let input_heavier = cost_position(&pricing(10.0, 1.0));
        assert!(output_heavy - input_heavy > input_heavier - input_heavy);
    }

    #[test]
    fn cost_color_runs_green_to_purple() {
        // Hue is NOT monotone across the ramp (green 145° → yellow 45° wraps
        // backwards); assert on the channel character instead: the cheap end
        // is green-dominant, the premium end red+blue.
        let green = cost_color_at(0.0);
        let purple = cost_color_at(1.0);
        let to_rgb = |c: Hsla| {
            let rgba = gpui::Rgba::from(c);
            (rgba.r, rgba.g, rgba.b)
        };
        let (r, g, b) = to_rgb(green);
        assert!(g > r && g > b, "cheap end green-dominant: {r} {g} {b}");
        let (r, g, b) = to_rgb(purple);
        assert!(r > g && b > g, "premium end red+blue: {r} {g} {b}");
        // Extremes differ from the middle and from each other.
        assert_ne!(to_rgb(cost_color_at(0.5)), to_rgb(green));
        assert_ne!(to_rgb(cost_color_at(0.5)), to_rgb(purple));
    }

    #[test]
    fn format_price_strips_trailing_zeros() {
        assert_eq!(format_price(1.2), "$1.2");
        assert_eq!(format_price(0.12), "$0.12");
        assert_eq!(format_price(6.0), "$6");
        assert_eq!(format_price(0.1), "$0.1");
        assert_eq!(format_price(0.125), "$0.12");
        // Sub-cent prices never read as free.
        assert_eq!(format_price(0.004), "$0.004");
    }
}
