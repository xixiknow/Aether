use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use aether_contracts::ExecutionPlan;
use aether_runtime_state::{DataLayerError, RuntimeState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tracing::{debug, warn};

use super::kiro_cache::{
    KiroPromptCacheUsage, KIRO_AUTO_CACHE_BREAKPOINTS_CONTEXT_FIELD,
    KIRO_SIMULATED_CACHE_ENABLED_CONTEXT_FIELD,
};
use crate::clock::current_unix_ms;

const STRATEGY: &str = "stable_99_v2";
const PROFILE_VERSION: &str = "kiro-stable-99-v2";
const DEFAULT_TARGET_PERCENT: u64 = 99;
const DEFAULT_TTL_SECS: u64 = 3600;
const DEFAULT_MAX_ENTRIES: usize = 65_536;
const MIN_TARGET_PERCENT: u64 = 1;
const MAX_TARGET_PERCENT: u64 = 99;
const MIN_TTL_SECS: u64 = 60;
const MAX_TTL_SECS: u64 = 86_400;
const MIN_MAX_ENTRIES: usize = 1024;
const MAX_MAX_ENTRIES: usize = 1_000_000;
const PREFIX_LOOKBACK_CANDIDATES: usize = 256;
const CLEANUP_INTERVAL_MS: u64 = 30_000;
const CACHE_INDEX_KEY: &str = "kiro:prompt-cache:v2:index";

pub(crate) const KIRO_CACHE_STRATEGY_CONTEXT_FIELD: &str = "kiro_simulated_cache_strategy";
pub(crate) const KIRO_CACHE_WARM_CONTEXT_FIELD: &str = "kiro_cache_warm";
pub(crate) const KIRO_CACHE_ACTUAL_READ_CONTEXT_FIELD: &str = "kiro_cache_actual_read_input_tokens";
pub(crate) const KIRO_CACHE_SYNTHETIC_READ_CONTEXT_FIELD: &str =
    "kiro_cache_synthetic_read_input_tokens";
pub(crate) const KIRO_CACHE_MATCH_KIND_CONTEXT_FIELD: &str = "kiro_cache_match_kind";

const TARGET_PERCENT_CONTEXT_FIELD: &str = "kiro_simulated_cache_target_percent";
const TTL_SECS_CONTEXT_FIELD: &str = "kiro_simulated_cache_ttl_secs";
const MAX_ENTRIES_CONTEXT_FIELD: &str = "kiro_simulated_cache_max_entries";
const BACKEND_CONTEXT_FIELD: &str = "kiro_simulated_cache_backend";
const BACKEND_RUNTIME: &str = "runtime";
const BACKEND_LOCAL: &str = "local_fallback";

static LOCAL_TRACKER: OnceLock<Mutex<HashMap<(String, [u8; 32]), LocalEntry>>> = OnceLock::new();
static LAST_CLEANUP_MS: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KiroStableCachePolicy {
    pub(crate) enabled: bool,
    pub(crate) target_percent: u64,
    pub(crate) ttl_secs: u64,
    pub(crate) max_entries: usize,
}

impl Default for KiroStableCachePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            target_percent: DEFAULT_TARGET_PERCENT,
            ttl_secs: DEFAULT_TTL_SECS,
            max_entries: DEFAULT_MAX_ENTRIES,
        }
    }
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize)]
struct RuntimeEntry {
    token_count: u64,
    ttl_secs: u64,
}

#[derive(Debug, Clone)]
struct LocalEntry {
    token_count: u64,
    expires_at: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PrefixPoint {
    fingerprint: [u8; 32],
    cumulative_tokens: u64,
}

#[derive(Debug, Clone)]
struct StableProfile {
    model: String,
    total_input_tokens: u64,
    breakpoints: Vec<PrefixPoint>,
    candidates: Vec<PrefixPoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProbeResult {
    actual_read_tokens: u64,
    warm: bool,
    match_kind: &'static str,
    local_fallback: bool,
}

pub(crate) fn stable_cache_policy_from_provider_config(
    config: Option<&Value>,
) -> KiroStableCachePolicy {
    let kiro = config
        .and_then(Value::as_object)
        .and_then(|config| config.get("kiro"))
        .and_then(Value::as_object);
    KiroStableCachePolicy {
        enabled: kiro
            .and_then(|kiro| kiro.get("simulated_cache_enabled"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        target_percent: kiro
            .and_then(|kiro| kiro.get("simulated_cache_target_percent"))
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TARGET_PERCENT)
            .clamp(MIN_TARGET_PERCENT, MAX_TARGET_PERCENT),
        ttl_secs: kiro
            .and_then(|kiro| kiro.get("simulated_cache_ttl_secs"))
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TTL_SECS)
            .clamp(MIN_TTL_SECS, MAX_TTL_SECS),
        max_entries: kiro
            .and_then(|kiro| kiro.get("simulated_cache_max_entries"))
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_MAX_ENTRIES)
            .clamp(MIN_MAX_ENTRIES, MAX_MAX_ENTRIES),
    }
}

pub(crate) fn seed_stable_cache_policy_context(
    report_context: &mut Option<Value>,
    policy: KiroStableCachePolicy,
) {
    let Some(context) = report_context.as_mut().and_then(Value::as_object_mut) else {
        return;
    };
    context.remove(KIRO_AUTO_CACHE_BREAKPOINTS_CONTEXT_FIELD);
    if !policy.enabled {
        context.remove(KIRO_SIMULATED_CACHE_ENABLED_CONTEXT_FIELD);
        context.remove(KIRO_CACHE_STRATEGY_CONTEXT_FIELD);
        context.remove(TARGET_PERCENT_CONTEXT_FIELD);
        context.remove(TTL_SECS_CONTEXT_FIELD);
        context.remove(MAX_ENTRIES_CONTEXT_FIELD);
        return;
    }
    context.insert(
        KIRO_SIMULATED_CACHE_ENABLED_CONTEXT_FIELD.to_string(),
        Value::Bool(true),
    );
    context.insert(
        KIRO_CACHE_STRATEGY_CONTEXT_FIELD.to_string(),
        Value::String(STRATEGY.to_string()),
    );
    context.insert(
        TARGET_PERCENT_CONTEXT_FIELD.to_string(),
        Value::from(policy.target_percent),
    );
    context.insert(
        TTL_SECS_CONTEXT_FIELD.to_string(),
        Value::from(policy.ttl_secs),
    );
    context.insert(
        MAX_ENTRIES_CONTEXT_FIELD.to_string(),
        Value::from(policy.max_entries as u64),
    );
}

pub(crate) async fn probe_and_seed_stable_cache_usage(
    runtime_state: &RuntimeState,
    plan: &ExecutionPlan,
    report_context: &mut Option<Value>,
) -> Option<KiroPromptCacheUsage> {
    let context = report_context.as_mut()?.as_object_mut()?;
    let policy = policy_from_context(context)?;
    if !policy.enabled {
        return None;
    }
    let scope = cache_scope(context)?;
    let Some(profile) = build_stable_profile(plan, context) else {
        seed_audit_context(context, false, 0, 0, "ineligible", BACKEND_RUNTIME);
        return None;
    };

    context.insert(
        "input_tokens".to_string(),
        Value::from(profile.total_input_tokens),
    );
    let probe = match probe_runtime(runtime_state, &scope, &profile).await {
        Ok(runtime_probe) if runtime_probe.warm => runtime_probe,
        Ok(runtime_probe) => {
            let local_probe = probe_local(&scope, &profile);
            if local_probe.warm {
                local_probe
            } else {
                runtime_probe
            }
        }
        Err(err) => {
            warn!(
                event_name = "kiro_stable_cache_probe_runtime_failed",
                log_type = "event",
                request_id = %plan.request_id,
                error = ?err,
                "failed to probe Kiro stable cache runtime state; using process-local fallback"
            );
            probe_local(&scope, &profile)
        }
    };

    let synthetic_read_tokens = if probe.warm {
        profile
            .total_input_tokens
            .saturating_mul(policy.target_percent)
            / 100
    } else {
        0
    };
    let usage = if probe.warm {
        KiroPromptCacheUsage {
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: synthetic_read_tokens,
        }
    } else {
        KiroPromptCacheUsage {
            cache_creation_input_tokens: profile.total_input_tokens,
            cache_read_input_tokens: 0,
        }
    };
    let backend = if probe.local_fallback {
        BACKEND_LOCAL
    } else {
        BACKEND_RUNTIME
    };
    seed_audit_context(
        context,
        probe.warm,
        probe.actual_read_tokens,
        synthetic_read_tokens,
        probe.match_kind,
        backend,
    );
    context.insert(
        "cache_creation_input_tokens".to_string(),
        Value::from(usage.cache_creation_input_tokens),
    );
    context.insert(
        "cache_read_input_tokens".to_string(),
        Value::from(usage.cache_read_input_tokens),
    );

    debug!(
        event_name = "kiro_stable_cache_probed",
        log_type = "event",
        request_id = %plan.request_id,
        model = %profile.model,
        warm = probe.warm,
        match_kind = probe.match_kind,
        backend,
        total_input_tokens = profile.total_input_tokens,
        actual_read_tokens = probe.actual_read_tokens,
        synthetic_read_tokens,
        target_percent = policy.target_percent,
        "computed Kiro stable cache usage"
    );
    Some(usage)
}

pub(crate) async fn commit_stable_cache_usage(
    runtime_state: &RuntimeState,
    plan: &ExecutionPlan,
    report_context: Option<&Value>,
) {
    let Some(context) = report_context.and_then(Value::as_object) else {
        return;
    };
    let Some(policy) = policy_from_context(context) else {
        return;
    };
    if !policy.enabled {
        return;
    }
    let Some(scope) = cache_scope(context) else {
        return;
    };
    let Some(profile) = build_stable_profile(plan, context) else {
        return;
    };

    let force_local = context
        .get(BACKEND_CONTEXT_FIELD)
        .and_then(Value::as_str)
        .is_some_and(|backend| backend == BACKEND_LOCAL);
    if force_local {
        if commit_runtime(runtime_state, &scope, &profile, policy)
            .await
            .is_ok()
        {
            return;
        }
        commit_local(&scope, &profile, policy);
        return;
    }
    if let Err(err) = commit_runtime(runtime_state, &scope, &profile, policy).await {
        warn!(
            event_name = "kiro_stable_cache_commit_runtime_failed",
            log_type = "event",
            request_id = %plan.request_id,
            error = ?err,
            "failed to commit Kiro stable cache runtime state; using process-local fallback"
        );
        commit_local(&scope, &profile, policy);
    }
}

fn seed_audit_context(
    context: &mut serde_json::Map<String, Value>,
    warm: bool,
    actual_read_tokens: u64,
    synthetic_read_tokens: u64,
    match_kind: &str,
    backend: &str,
) {
    context.insert(KIRO_CACHE_WARM_CONTEXT_FIELD.to_string(), Value::Bool(warm));
    context.insert(
        KIRO_CACHE_ACTUAL_READ_CONTEXT_FIELD.to_string(),
        Value::from(actual_read_tokens),
    );
    context.insert(
        KIRO_CACHE_SYNTHETIC_READ_CONTEXT_FIELD.to_string(),
        Value::from(synthetic_read_tokens),
    );
    context.insert(
        KIRO_CACHE_MATCH_KIND_CONTEXT_FIELD.to_string(),
        Value::String(match_kind.to_string()),
    );
    context.insert(
        BACKEND_CONTEXT_FIELD.to_string(),
        Value::String(backend.to_string()),
    );
}

fn policy_from_context(context: &serde_json::Map<String, Value>) -> Option<KiroStableCachePolicy> {
    let enabled = context
        .get(KIRO_SIMULATED_CACHE_ENABLED_CONTEXT_FIELD)
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let strategy_matches = context
        .get(KIRO_CACHE_STRATEGY_CONTEXT_FIELD)
        .and_then(Value::as_str)
        .is_some_and(|strategy| strategy == STRATEGY);
    if !enabled || !strategy_matches {
        return None;
    }
    Some(KiroStableCachePolicy {
        enabled: true,
        target_percent: context
            .get(TARGET_PERCENT_CONTEXT_FIELD)
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TARGET_PERCENT)
            .clamp(MIN_TARGET_PERCENT, MAX_TARGET_PERCENT),
        ttl_secs: context
            .get(TTL_SECS_CONTEXT_FIELD)
            .and_then(Value::as_u64)
            .unwrap_or(DEFAULT_TTL_SECS)
            .clamp(MIN_TTL_SECS, MAX_TTL_SECS),
        max_entries: context
            .get(MAX_ENTRIES_CONTEXT_FIELD)
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .unwrap_or(DEFAULT_MAX_ENTRIES)
            .clamp(MIN_MAX_ENTRIES, MAX_MAX_ENTRIES),
    })
}

fn cache_scope(context: &serde_json::Map<String, Value>) -> Option<String> {
    context
        .get("api_key_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| format!("client:{value}"))
}

fn build_stable_profile(
    plan: &ExecutionPlan,
    context: &serde_json::Map<String, Value>,
) -> Option<StableProfile> {
    let body = plan.body.json_body.as_ref()?;
    let conversation = body.get("conversationState")?;
    let current_message = conversation.get("currentMessage")?;
    let current_user = current_message.get("userInputMessage")?;
    let model = current_user
        .get("modelId")
        .and_then(Value::as_str)
        .or_else(|| context.get("mapped_model").and_then(Value::as_str))
        .or(plan.model_name.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())?
        .to_string();

    let mut blocks = Vec::new();
    let mut tools = current_user
        .get("userInputMessageContext")
        .and_then(Value::as_object)
        .and_then(|context| context.get("tools"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(strip_non_semantic_fields)
        .map(canonicalize_json)
        .collect::<Vec<_>>();
    tools.sort_by(|left, right| tool_sort_key(left).cmp(&tool_sort_key(right)));
    for tool in tools {
        blocks.push(canonicalize_json(serde_json::json!({
            "kind": "tool",
            "value": tool,
        })));
    }

    if let Some(history) = conversation.get("history").and_then(Value::as_array) {
        for item in history {
            if let Some(item) = normalize_message(item) {
                blocks.push(item);
            }
        }
    }
    if let Some(current) = normalize_message(current_message) {
        blocks.push(current);
    }
    if blocks.is_empty() {
        return None;
    }

    let block_tokens = blocks.iter().map(estimate_value_tokens).collect::<Vec<_>>();
    let raw_total = block_tokens.iter().copied().sum::<u64>().max(1);
    let context_total = context
        .get("input_tokens")
        .and_then(Value::as_u64)
        .filter(|value| *value > 0);
    let total_input_tokens = context_total.unwrap_or(raw_total);
    let min_cacheable_tokens = minimum_cacheable_tokens_for_model(&model);
    if total_input_tokens < min_cacheable_tokens {
        return None;
    }

    let prelude = canonicalize_json(serde_json::json!({
        "profile": PROFILE_VERSION,
        "model": model,
    }));
    let mut hasher = Sha256::new();
    hash_value(&mut hasher, &prelude);
    let mut raw_cumulative = 0u64;
    let mut all_points = Vec::with_capacity(blocks.len());
    for (index, (block, tokens)) in blocks.iter().zip(block_tokens).enumerate() {
        hash_value(&mut hasher, block);
        raw_cumulative = raw_cumulative.saturating_add(tokens);
        let cumulative_tokens = if index + 1 == blocks.len() {
            total_input_tokens
        } else {
            raw_cumulative
                .saturating_mul(total_input_tokens)
                .saturating_add(raw_total - 1)
                / raw_total
        }
        .min(total_input_tokens);
        all_points.push(PrefixPoint {
            fingerprint: hasher.clone().finalize().into(),
            cumulative_tokens,
        });
    }

    let earliest_index = all_points
        .iter()
        .position(|point| point.cumulative_tokens >= min_cacheable_tokens)?;
    let tail_index = all_points.len() - 1;
    let mut breakpoints = vec![all_points[earliest_index]];
    if tail_index != earliest_index {
        breakpoints.push(all_points[tail_index]);
    }

    let lookback_start = all_points.len().saturating_sub(PREFIX_LOOKBACK_CANDIDATES);
    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for point in
        std::iter::once(&all_points[earliest_index]).chain(all_points[lookback_start..].iter())
    {
        if point.cumulative_tokens >= min_cacheable_tokens && seen.insert(point.fingerprint) {
            candidates.push(*point);
        }
    }
    candidates.sort_by_key(|point| point.cumulative_tokens);

    Some(StableProfile {
        model,
        total_input_tokens,
        breakpoints,
        candidates,
    })
}

fn normalize_message(value: &Value) -> Option<Value> {
    let mut message = value.clone();
    let object = message.as_object_mut()?;
    if let Some(user) = object
        .get_mut("userInputMessage")
        .and_then(Value::as_object_mut)
    {
        if let Some(message_context) = user
            .get_mut("userInputMessageContext")
            .and_then(Value::as_object_mut)
        {
            message_context.remove("tools");
            message_context.retain(|_, value| !is_empty_context_value(value));
            if message_context.is_empty() {
                user.remove("userInputMessageContext");
            }
        }
    }
    Some(canonicalize_json(strip_non_semantic_fields(message)))
}

fn strip_non_semantic_fields(mut value: Value) -> Value {
    match &mut value {
        Value::Array(items) => {
            for item in items {
                *item = strip_non_semantic_fields(item.take());
            }
        }
        Value::Object(object) => {
            object.remove("conversationId");
            object.remove("agentContinuationId");
            object.remove("toolUseId");
            object.remove("cache_control");
            object.remove("cacheControl");
            for item in object.values_mut() {
                *item = strip_non_semantic_fields(item.take());
            }
        }
        _ => {}
    }
    value
}

fn is_empty_context_value(value: &Value) -> bool {
    matches!(value, Value::Null)
        || value.as_array().is_some_and(Vec::is_empty)
        || value.as_object().is_some_and(serde_json::Map::is_empty)
}

fn canonicalize_json(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(canonicalize_json).collect()),
        Value::Object(object) => {
            let ordered = object
                .into_iter()
                .map(|(key, value)| (key, canonicalize_json(value)))
                .collect::<BTreeMap<_, _>>();
            Value::Object(ordered.into_iter().collect())
        }
        other => other,
    }
}

fn tool_sort_key(value: &Value) -> String {
    let name = value
        .get("toolSpecification")
        .and_then(Value::as_object)
        .and_then(|spec| spec.get("name"))
        .and_then(Value::as_str)
        .or_else(|| value.get("name").and_then(Value::as_str))
        .unwrap_or_default()
        .to_ascii_lowercase();
    format!(
        "{name}\0{}",
        serde_json::to_string(value).unwrap_or_default()
    )
}

fn hash_value(hasher: &mut Sha256, value: &Value) {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn estimate_value_tokens(value: &Value) -> u64 {
    match value {
        Value::Null => 0,
        Value::Bool(_) | Value::Number(_) => 1,
        Value::String(text) => estimate_text_tokens(text),
        Value::Array(items) => items.iter().map(estimate_value_tokens).sum(),
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| {
                if is_inline_image_data(key, object) && value.as_str().is_some() {
                    256
                } else {
                    estimate_text_tokens(key).saturating_add(estimate_value_tokens(value))
                }
            })
            .sum(),
    }
}

fn is_inline_image_data(key: &str, object: &serde_json::Map<String, Value>) -> bool {
    (key.eq_ignore_ascii_case("data") || key.eq_ignore_ascii_case("bytes"))
        && object
            .get("media_type")
            .or_else(|| object.get("mediaType"))
            .and_then(Value::as_str)
            .is_some_and(|media_type| media_type.trim().to_ascii_lowercase().starts_with("image/"))
}

fn estimate_text_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let mut cjk = 0usize;
    let mut other = 0usize;
    for character in text.chars().filter(|character| !character.is_whitespace()) {
        if matches!(
            character,
            '\u{4E00}'..='\u{9FFF}'
                | '\u{3400}'..='\u{4DBF}'
                | '\u{3040}'..='\u{30FF}'
                | '\u{AC00}'..='\u{D7AF}'
        ) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    ((cjk as f64 / 1.5) + (other as f64 / 3.5)).round().max(1.0) as u64
}

fn minimum_cacheable_tokens_for_model(model: &str) -> u64 {
    let model = model.to_ascii_lowercase().replace('_', "-");
    if model.contains("opus") {
        4096
    } else if model.contains("haiku") && (model.contains("claude-3") || model.contains("haiku-3")) {
        2048
    } else {
        1024
    }
}

async fn probe_runtime(
    runtime_state: &RuntimeState,
    scope: &str,
    profile: &StableProfile,
) -> Result<ProbeResult, DataLayerError> {
    let keys = profile
        .candidates
        .iter()
        .map(|point| runtime_key(scope, &profile.model, &point.fingerprint))
        .collect::<Vec<_>>();
    let values = runtime_state.kv_get_many(&keys).await?;
    for (point, value) in profile.candidates.iter().zip(values).rev() {
        let Some(entry) = value.as_deref().and_then(parse_runtime_entry) else {
            continue;
        };
        return Ok(ProbeResult {
            actual_read_tokens: entry
                .token_count
                .min(point.cumulative_tokens)
                .min(profile.total_input_tokens),
            warm: true,
            match_kind: if point.fingerprint
                == profile
                    .breakpoints
                    .last()
                    .map(|point| point.fingerprint)
                    .unwrap_or_default()
            {
                "tail"
            } else {
                "prefix"
            },
            local_fallback: false,
        });
    }
    Ok(ProbeResult {
        actual_read_tokens: 0,
        warm: false,
        match_kind: "cold",
        local_fallback: false,
    })
}

fn probe_local(scope: &str, profile: &StableProfile) -> ProbeResult {
    let entries = LOCAL_TRACKER.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut entries) = entries.lock() else {
        return ProbeResult {
            actual_read_tokens: 0,
            warm: false,
            match_kind: "cold",
            local_fallback: true,
        };
    };
    let now = Instant::now();
    entries.retain(|_, entry| entry.expires_at > now);
    for point in profile.candidates.iter().rev() {
        let key = (local_scope_key(scope, &profile.model), point.fingerprint);
        let Some(entry) = entries.get(&key) else {
            continue;
        };
        return ProbeResult {
            actual_read_tokens: entry
                .token_count
                .min(point.cumulative_tokens)
                .min(profile.total_input_tokens),
            warm: true,
            match_kind: if point.fingerprint
                == profile
                    .breakpoints
                    .last()
                    .map(|point| point.fingerprint)
                    .unwrap_or_default()
            {
                "tail"
            } else {
                "prefix"
            },
            local_fallback: true,
        };
    }
    ProbeResult {
        actual_read_tokens: 0,
        warm: false,
        match_kind: "cold",
        local_fallback: true,
    }
}

async fn commit_runtime(
    runtime_state: &RuntimeState,
    scope: &str,
    profile: &StableProfile,
    policy: KiroStableCachePolicy,
) -> Result<(), DataLayerError> {
    for point in &profile.breakpoints {
        let key = runtime_key(scope, &profile.model, &point.fingerprint);
        let entry = RuntimeEntry {
            token_count: point.cumulative_tokens,
            ttl_secs: policy.ttl_secs,
        };
        runtime_state
            .kv_set(
                key.as_str(),
                serde_json::to_string(&entry).unwrap_or_default(),
                Some(Duration::from_secs(policy.ttl_secs)),
            )
            .await?;
        runtime_state
            .score_set(
                CACHE_INDEX_KEY,
                key.as_str(),
                current_unix_ms().saturating_add(policy.ttl_secs.saturating_mul(1000)) as f64,
            )
            .await?;
    }
    maybe_cleanup_runtime(runtime_state, policy.max_entries).await;
    Ok(())
}

fn commit_local(scope: &str, profile: &StableProfile, policy: KiroStableCachePolicy) {
    let entries = LOCAL_TRACKER.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut entries) = entries.lock() else {
        return;
    };
    let now = Instant::now();
    entries.retain(|_, entry| entry.expires_at > now);
    let local_scope = local_scope_key(scope, &profile.model);
    for point in &profile.breakpoints {
        entries.insert(
            (local_scope.clone(), point.fingerprint),
            LocalEntry {
                token_count: point.cumulative_tokens,
                expires_at: now + Duration::from_secs(policy.ttl_secs),
            },
        );
    }
    while entries.len() > policy.max_entries {
        let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.expires_at)
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        entries.remove(&oldest);
    }
}

async fn maybe_cleanup_runtime(runtime_state: &RuntimeState, max_entries: usize) {
    let now = current_unix_ms();
    let previous = LAST_CLEANUP_MS.load(Ordering::Relaxed);
    if now.saturating_sub(previous) < CLEANUP_INTERVAL_MS
        || LAST_CLEANUP_MS
            .compare_exchange(previous, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    if runtime_state
        .score_remove_by_score(CACHE_INDEX_KEY, now as f64)
        .await
        .is_err()
    {
        return;
    }
    let Ok(index_len) = runtime_state.score_len(CACHE_INDEX_KEY).await else {
        return;
    };
    let trim_count = index_len.saturating_sub(max_entries);
    if trim_count == 0 {
        return;
    }
    let Ok(members) = runtime_state.score_range_by_min(CACHE_INDEX_KEY, 0.0).await else {
        return;
    };
    let members = members.into_iter().take(trim_count).collect::<Vec<_>>();
    let _ = runtime_state.kv_delete_many(&members).await;
    let _ = runtime_state
        .score_remove_by_rank(CACHE_INDEX_KEY, 0, trim_count as i64 - 1)
        .await;
}

fn parse_runtime_entry(value: &str) -> Option<RuntimeEntry> {
    serde_json::from_str::<RuntimeEntry>(value)
        .ok()
        .filter(|entry| entry.token_count > 0 && entry.ttl_secs > 0)
}

fn local_scope_key(scope: &str, model: &str) -> String {
    format!("{scope}\0{}", model.trim().to_ascii_lowercase())
}

fn runtime_key(scope: &str, model: &str, fingerprint: &[u8; 32]) -> String {
    let scope_hash: [u8; 32] = Sha256::digest(local_scope_key(scope, model).as_bytes()).into();
    format!(
        "kiro:prompt-cache:v2:{}:{}",
        hex_digest(&scope_hash),
        hex_digest(fingerprint)
    )
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use aether_contracts::RequestBody;
    use aether_runtime_state::MemoryRuntimeStateConfig;
    use serde_json::json;

    use super::*;

    fn long_text(label: &str) -> String {
        format!("{label} {}", "stable cache content ".repeat(600))
    }

    fn envelope_plan(conversation_state: Value) -> ExecutionPlan {
        ExecutionPlan {
            request_id: "req-v2".to_string(),
            candidate_id: None,
            provider_name: Some("kiro-pool".to_string()),
            provider_id: "provider-a".to_string(),
            endpoint_id: "endpoint-a".to_string(),
            key_id: "upstream-account-a".to_string(),
            method: "POST".to_string(),
            url: "https://q.us-east-1.amazonaws.com/generateAssistantResponse".to_string(),
            headers: BTreeMap::new(),
            content_type: Some("application/json".to_string()),
            content_encoding: None,
            body: RequestBody::from_json(json!({"conversationState": conversation_state})),
            stream: true,
            client_api_format: "claude:messages".to_string(),
            provider_api_format: "claude:messages".to_string(),
            model_name: Some("claude-sonnet-4.6".to_string()),
            proxy: None,
            transport_profile: None,
            timeouts: None,
        }
    }

    fn first_turn_plan(conversation_id: &str, continuation_id: &str) -> ExecutionPlan {
        envelope_plan(json!({
            "agentContinuationId": continuation_id,
            "conversationId": conversation_id,
            "history": [
                {"userInputMessage": {"content": long_text("system"), "modelId": "claude-sonnet-4.6", "origin": "AI_EDITOR"}},
                {"assistantResponseMessage": {"content": "I will follow these instructions."}}
            ],
            "currentMessage": {"userInputMessage": {
                "content": long_text("first user"),
                "modelId": "claude-sonnet-4.6",
                "origin": "AI_EDITOR",
                "userInputMessageContext": {"tools": [
                    {"toolSpecification": {"name": "Write", "description": "write", "inputSchema": {"json": {"type": "object"}}}},
                    {"toolSpecification": {"name": "Read", "description": "read", "inputSchema": {"json": {"type": "object"}}}}
                ]}
            }}
        }))
    }

    fn enabled_context(api_key_id: &str) -> Option<Value> {
        let mut context = Some(json!({"api_key_id": api_key_id}));
        seed_stable_cache_policy_context(
            &mut context,
            KiroStableCachePolicy {
                enabled: true,
                ..KiroStableCachePolicy::default()
            },
        );
        context
    }

    #[test]
    fn policy_defaults_and_bounds_are_stable() {
        let defaults = stable_cache_policy_from_provider_config(Some(&json!({
            "kiro": {"simulated_cache_enabled": true}
        })));
        assert!(defaults.enabled);
        assert_eq!(defaults.target_percent, 99);
        assert_eq!(defaults.ttl_secs, 3600);
        assert_eq!(defaults.max_entries, 65_536);

        let bounded = stable_cache_policy_from_provider_config(Some(&json!({
            "kiro": {
                "simulated_cache_enabled": true,
                "simulated_cache_target_percent": 100,
                "simulated_cache_ttl_secs": 1,
                "simulated_cache_max_entries": 2
            }
        })));
        assert_eq!(bounded.target_percent, 99);
        assert_eq!(bounded.ttl_secs, 60);
        assert_eq!(bounded.max_entries, 1024);
    }

    #[test]
    fn model_cache_thresholds_cover_haiku_three_name_variants() {
        assert_eq!(minimum_cacheable_tokens_for_model("claude-opus-4-1"), 4096);
        assert_eq!(minimum_cacheable_tokens_for_model("claude-3-haiku"), 2048);
        assert_eq!(minimum_cacheable_tokens_for_model("claude-3-5-haiku"), 2048);
        assert_eq!(minimum_cacheable_tokens_for_model("claude_3_haiku"), 2048);
        assert_eq!(minimum_cacheable_tokens_for_model("claude-haiku-3-5"), 2048);
        assert_eq!(minimum_cacheable_tokens_for_model("claude-haiku-4-5"), 1024);
        assert_eq!(
            minimum_cacheable_tokens_for_model("claude-sonnet-4-6"),
            1024
        );
    }

    #[test]
    fn volatile_ids_and_tool_order_do_not_change_profile() {
        let mut first = first_turn_plan("conversation-a", "continuation-a");
        let mut second = first_turn_plan("conversation-b", "continuation-b");
        first.body.json_body.as_mut().unwrap()["conversationState"]["history"][1]
            ["assistantResponseMessage"]["toolUses"] = json!([{
            "toolUseId": "tool-call-a",
            "name": "Read",
            "input": {"path": "README.md"}
        }]);
        second.body.json_body.as_mut().unwrap()["conversationState"]["history"][1]
            ["assistantResponseMessage"]["toolUses"] = json!([{
            "toolUseId": "tool-call-b",
            "name": "Read",
            "input": {"path": "README.md"}
        }]);
        first.body.json_body.as_mut().unwrap()["conversationState"]["currentMessage"]
            ["userInputMessage"]["userInputMessageContext"]["empty"] = json!([]);
        second.body.json_body.as_mut().unwrap()["conversationState"]["currentMessage"]
            ["userInputMessage"]["cache_control"] = json!({"type": "ephemeral"});
        second.body.json_body.as_mut().unwrap()["conversationState"]["currentMessage"]
            ["userInputMessage"]["userInputMessageContext"]["tools"]
            .as_array_mut()
            .unwrap()
            .reverse();
        let context = enabled_context("client-a").unwrap();
        let left = build_stable_profile(&first, context.as_object().unwrap()).unwrap();
        let right = build_stable_profile(&second, context.as_object().unwrap()).unwrap();
        assert_eq!(left.breakpoints, right.breakpoints);
        assert_eq!(left.candidates, right.candidates);
    }

    #[test]
    fn previous_current_message_is_a_candidate_after_it_moves_to_history() {
        let first = first_turn_plan("conversation-a", "continuation-a");
        let mut second_state = first.body.json_body.as_ref().unwrap()["conversationState"].clone();
        let previous_current = second_state["currentMessage"].clone();
        let history = second_state["history"].as_array_mut().unwrap();
        history.push(previous_current);
        history.push(json!({"assistantResponseMessage": {"content": "previous answer"}}));
        second_state["currentMessage"] = json!({"userInputMessage": {
            "content": long_text("second user"),
            "modelId": "claude-sonnet-4.6",
            "origin": "AI_EDITOR",
            "userInputMessageContext": {"tools": [
                {"toolSpecification": {"name": "Read", "description": "read", "inputSchema": {"json": {"type": "object"}}}},
                {"toolSpecification": {"name": "Write", "description": "write", "inputSchema": {"json": {"type": "object"}}}}
            ]}
        }});
        let second = envelope_plan(second_state);
        let context = enabled_context("client-a").unwrap();
        let first_profile = build_stable_profile(&first, context.as_object().unwrap()).unwrap();
        let second_profile = build_stable_profile(&second, context.as_object().unwrap()).unwrap();
        let previous_tail = first_profile.breakpoints.last().unwrap().fingerprint;
        assert!(second_profile
            .candidates
            .iter()
            .any(|candidate| candidate.fingerprint == previous_tail));
    }

    #[tokio::test]
    async fn cold_then_warm_request_reports_exactly_ninety_nine_percent() {
        let runtime = RuntimeState::memory(MemoryRuntimeStateConfig::default());
        let plan = first_turn_plan("conversation-a", "continuation-a");
        let mut first_context = enabled_context("client-a");
        let cold = probe_and_seed_stable_cache_usage(&runtime, &plan, &mut first_context)
            .await
            .unwrap();
        assert_eq!(cold.cache_read_input_tokens, 0);
        assert!(cold.cache_creation_input_tokens > 0);
        commit_stable_cache_usage(&runtime, &plan, first_context.as_ref()).await;

        let mut second_context = enabled_context("client-a");
        let warm = probe_and_seed_stable_cache_usage(&runtime, &plan, &mut second_context)
            .await
            .unwrap();
        let total = second_context.as_ref().unwrap()["input_tokens"]
            .as_u64()
            .unwrap();
        assert_eq!(warm.cache_creation_input_tokens, 0);
        assert_eq!(warm.cache_read_input_tokens, total * 99 / 100);
        assert_eq!(
            total,
            total.saturating_sub(warm.cache_read_input_tokens)
                + warm.cache_creation_input_tokens
                + warm.cache_read_input_tokens
        );
        assert_eq!(
            second_context.as_ref().unwrap()[KIRO_CACHE_WARM_CONTEXT_FIELD],
            json!(true)
        );
        assert_eq!(
            second_context.as_ref().unwrap()[KIRO_CACHE_ACTUAL_READ_CONTEXT_FIELD],
            json!(total)
        );
        assert_eq!(
            second_context.as_ref().unwrap()[KIRO_CACHE_SYNTHETIC_READ_CONTEXT_FIELD],
            json!(total * 99 / 100)
        );
        assert_eq!(
            second_context.as_ref().unwrap()[KIRO_CACHE_MATCH_KIND_CONTEXT_FIELD],
            json!("tail")
        );
    }

    #[tokio::test]
    async fn api_key_scope_is_isolated_from_upstream_account_rotation() {
        let runtime = RuntimeState::memory(MemoryRuntimeStateConfig::default());
        let plan = first_turn_plan("conversation-a", "continuation-a");
        let mut first_context = enabled_context("client-a");
        probe_and_seed_stable_cache_usage(&runtime, &plan, &mut first_context).await;
        commit_stable_cache_usage(&runtime, &plan, first_context.as_ref()).await;

        let mut rotated_plan = plan.clone();
        rotated_plan.key_id = "upstream-account-b".to_string();
        let mut same_client = enabled_context("client-a");
        let warm = probe_and_seed_stable_cache_usage(&runtime, &rotated_plan, &mut same_client)
            .await
            .unwrap();
        assert!(warm.cache_read_input_tokens > 0);

        let mut other_client = enabled_context("client-b");
        let cold = probe_and_seed_stable_cache_usage(&runtime, &rotated_plan, &mut other_client)
            .await
            .unwrap();
        assert_eq!(cold.cache_read_input_tokens, 0);

        let mut other_model_plan = rotated_plan.clone();
        other_model_plan.model_name = Some("claude-haiku-3.5".to_string());
        other_model_plan.body.json_body.as_mut().unwrap()["conversationState"]["currentMessage"]
            ["userInputMessage"]["modelId"] = json!("claude-haiku-3.5");
        let mut other_model = enabled_context("client-a");
        let cold = probe_and_seed_stable_cache_usage(&runtime, &other_model_plan, &mut other_model)
            .await
            .unwrap();
        assert_eq!(cold.cache_read_input_tokens, 0);
    }

    #[test]
    fn runtime_keys_are_fully_versioned_and_hashed() {
        let key = runtime_key("client:api-key", "claude-sonnet-4.6", &[7; 32]);
        assert!(CACHE_INDEX_KEY.starts_with("kiro:prompt-cache:v2:"));
        assert!(key.starts_with("kiro:prompt-cache:v2:"));
        assert!(!key.contains("api-key"));
        assert!(!key.contains("claude-sonnet"));
    }

    #[tokio::test]
    async fn probing_without_successful_commit_does_not_warm_cache() {
        let runtime = RuntimeState::memory(MemoryRuntimeStateConfig::default());
        let plan = first_turn_plan("conversation-a", "continuation-a");
        let mut first_context = enabled_context("client-probe-only");
        let first = probe_and_seed_stable_cache_usage(&runtime, &plan, &mut first_context)
            .await
            .unwrap();
        assert_eq!(first.cache_read_input_tokens, 0);

        let mut second_context = enabled_context("client-probe-only");
        let second = probe_and_seed_stable_cache_usage(&runtime, &plan, &mut second_context)
            .await
            .unwrap();
        assert_eq!(second.cache_read_input_tokens, 0);

        commit_stable_cache_usage(&runtime, &plan, second_context.as_ref()).await;
        let mut committed_context = enabled_context("client-probe-only");
        let committed = probe_and_seed_stable_cache_usage(&runtime, &plan, &mut committed_context)
            .await
            .unwrap();
        assert!(committed.cache_read_input_tokens > 0);
    }

    #[tokio::test]
    async fn one_hundred_turns_stay_at_target_across_client_formats_and_accounts() {
        let runtime = RuntimeState::memory(MemoryRuntimeStateConfig::default());
        let mut history = vec![
            json!({"userInputMessage": {
                "content": long_text("shared system"),
                "modelId": "claude-sonnet-4.6",
                "origin": "AI_EDITOR"
            }}),
            json!({"assistantResponseMessage": {
                "content": "I will follow these instructions."
            }}),
        ];
        let client_formats = ["claude:messages", "openai:chat", "openai:responses"];

        for turn in 0..100 {
            let user_content = format!("turn {turn}: {}", "new context ".repeat(20));
            let current = json!({"userInputMessage": {
                "content": user_content,
                "modelId": "claude-sonnet-4.6",
                "origin": "AI_EDITOR",
                "userInputMessageContext": {"tools": [{
                    "toolSpecification": {
                        "name": "Read",
                        "description": "read a file",
                        "inputSchema": {"json": {"type": "object"}}
                    }
                }]}
            }});
            let mut plan = envelope_plan(json!({
                "agentContinuationId": format!("continuation-{turn}"),
                "conversationId": "stable-conversation",
                "history": history,
                "currentMessage": current
            }));
            plan.client_api_format = client_formats[turn % client_formats.len()].to_string();
            plan.key_id = format!("upstream-account-{}", turn % 7);

            let mut context = enabled_context("client-100-turns");
            let usage = probe_and_seed_stable_cache_usage(&runtime, &plan, &mut context)
                .await
                .expect("long request should be cache eligible");
            let total = context.as_ref().unwrap()["input_tokens"].as_u64().unwrap();
            if turn == 0 {
                assert_eq!(usage.cache_read_input_tokens, 0);
            } else {
                assert_eq!(usage.cache_read_input_tokens, total * 99 / 100);
                assert_eq!(usage.cache_creation_input_tokens, 0);
            }
            commit_stable_cache_usage(&runtime, &plan, context.as_ref()).await;

            history.push(json!({"userInputMessage": {
                "content": user_content,
                "modelId": "claude-sonnet-4.6",
                "origin": "AI_EDITOR"
            }}));
            history.push(json!({"assistantResponseMessage": {
                "content": format!("answer {turn}")
            }}));
        }
    }
}
