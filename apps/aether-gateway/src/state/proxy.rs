use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_contracts::ProxySnapshot;
use aether_data::repository::proxy_nodes::{
    proxy_node_accepts_new_tunnels, remote_config_scheduling_state, StoredProxyGroup,
    StoredProxyGroupMember, StoredProxyNode,
};
use aether_data_contracts::repository::pool_scores::{
    GetPoolMemberScoresByIdsQuery, PoolMemberHardState, PoolMemberIdentity, PoolMemberProbeStatus,
    PoolScoreScope, StoredPoolMemberScore, UpsertPoolMemberScore, POOL_SCORE_CAPABILITY_PROXY,
    POOL_SCORE_SCOPE_KIND_PROXY_GROUP,
};
use aether_pool_core::POOL_SCORE_VERSION;
use serde_json::{json, Map, Value};

use super::AppState;
use crate::provider_transport::{GatewayProviderTransportSnapshot, TransportTunnelAffinityLookup};

const TUNNEL_BASE_URL_EXTRA_KEY: &str = "tunnel_base_url";
const TUNNEL_OWNER_INSTANCE_ID_EXTRA_KEY: &str = "tunnel_owner_instance_id";
const TUNNEL_OWNER_OBSERVED_AT_EXTRA_KEY: &str = "tunnel_owner_observed_at_unix_secs";
const PROXY_GROUP_STRATEGY_BALANCED_WEIGHTED: &str = "balanced_weighted";
const PROXY_GROUP_STRATEGY_STABLE_FAILOVER: &str = "stable_failover";
const PROXY_GROUP_STRATEGY_SUCCESS_RATE: &str = "success_rate";
const PROXY_GROUP_STRATEGY_MANUAL_PRIORITY: &str = "manual_priority";
const PROXY_GROUP_CIRCUIT_BREAKER_FAILURE_THRESHOLD: u64 = 3;
const PROXY_GROUP_CIRCUIT_BREAKER_COOLDOWN_SECS: u64 = 300;
static PROXY_GROUP_SELECTION_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub(crate) struct ProxyGroupMemberScoreSnapshot {
    pub(crate) group_id: String,
    pub(crate) node_id: String,
    pub(crate) score: f64,
    pub(crate) effective_score: f64,
    pub(crate) hard_state: PoolMemberHardState,
    pub(crate) score_reason: Value,
    pub(crate) sort_index: i32,
    pub(crate) enabled: bool,
    pub(crate) available: bool,
}

#[derive(Debug, Clone)]
struct ProxyGroupCandidate {
    node: StoredProxyNode,
    member: StoredProxyGroupMember,
    score: f64,
    effective_score: f64,
    hard_state: PoolMemberHardState,
    score_reason: Value,
    load_ratio: f64,
    failure_rate: f64,
    existing_score: Option<StoredPoolMemberScore>,
}

#[derive(Debug, Clone)]
struct ProxyGroupSelectionContext {
    source: &'static str,
    provider_id: String,
    endpoint_id: String,
    key_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProxyGroupStrategyKind {
    BalancedWeighted,
    StableFailover,
    SuccessRate,
    ManualPriority,
}

#[derive(Debug, Clone, Copy)]
struct ProxyGroupStrategyConfig {
    kind: ProxyGroupStrategyKind,
    top_n: usize,
    circuit_breaker_enabled: bool,
    circuit_breaker_failure_threshold: u64,
    circuit_breaker_cooldown_secs: u64,
}

impl AppState {
    pub(crate) async fn read_system_proxy_node_id(&self) -> Option<String> {
        self.read_system_config_json_value("system_proxy_node_id")
            .await
            .ok()
            .flatten()
            .and_then(|value| value.as_str().map(str::trim).map(ToOwned::to_owned))
            .filter(|value| !value.is_empty())
    }

    pub(crate) async fn resolve_proxy_node_snapshot(
        &self,
        node_id: Option<&str>,
    ) -> Option<ProxySnapshot> {
        let node_id = node_id.map(str::trim).filter(|value| !value.is_empty())?;
        let node = self.find_proxy_node(node_id).await.ok().flatten()?;
        if node.status.trim() != "online" {
            return None;
        }
        if !proxy_node_accepts_new_tunnels(&node) {
            return None;
        }
        if node.tunnel_mode && node.tunnel_connected {
            let mut extra = Map::new();
            let owner = self
                .lookup_tunnel_attachment_owner(node_id)
                .await
                .ok()
                .flatten();
            if let Some(owner) = owner {
                extra.insert(
                    TUNNEL_BASE_URL_EXTRA_KEY.to_string(),
                    Value::String(owner.relay_base_url),
                );
                extra.insert(
                    TUNNEL_OWNER_INSTANCE_ID_EXTRA_KEY.to_string(),
                    Value::String(owner.gateway_instance_id),
                );
                extra.insert(
                    TUNNEL_OWNER_OBSERVED_AT_EXTRA_KEY.to_string(),
                    json!(owner.observed_at_unix_secs),
                );
            } else if !self.tunnel.has_local_proxy(node_id) {
                return None;
            }
            return Some(ProxySnapshot {
                enabled: Some(true),
                mode: Some("tunnel".to_string()),
                node_id: Some(node.id),
                label: Some(node.name),
                url: None,
                extra: if extra.is_empty() {
                    None
                } else {
                    Some(Value::Object(extra))
                },
            });
        }
        if !node.is_manual {
            return None;
        }
        let proxy_url = node
            .proxy_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())?;
        Some(ProxySnapshot {
            enabled: Some(true),
            mode: proxy_mode_from_url(Some(proxy_url)),
            node_id: Some(node.id),
            label: Some(node.name),
            url: proxy_url_with_node_auth(
                proxy_url,
                node.proxy_username.as_deref(),
                node.proxy_password.as_deref(),
            )
            .or_else(|| Some(proxy_url.to_string())),
            extra: None,
        })
    }

    pub(crate) async fn resolve_system_proxy_snapshot(&self) -> Option<ProxySnapshot> {
        let node_id = self.read_system_proxy_node_id().await;
        self.resolve_proxy_node_snapshot(node_id.as_deref()).await
    }

    pub(crate) async fn resolve_transport_proxy_snapshot_with_tunnel_affinity(
        &self,
        transport: &GatewayProviderTransportSnapshot,
    ) -> Option<ProxySnapshot> {
        self.resolve_transport_proxy_with_source_with_tunnel_affinity(transport)
            .await
            .map(|(snapshot, _)| snapshot)
    }

    pub(crate) async fn resolve_transport_proxy_source_with_tunnel_affinity(
        &self,
        transport: &GatewayProviderTransportSnapshot,
    ) -> Option<&'static str> {
        self.resolve_transport_proxy_with_source_with_tunnel_affinity(transport)
            .await
            .map(|(_, source)| source)
    }

    pub(crate) async fn resolve_configured_proxy_snapshot_with_tunnel_affinity(
        &self,
        raw: Option<&Value>,
    ) -> Option<ProxySnapshot> {
        self.resolve_configured_proxy_snapshot_with_selection_context(raw, None)
            .await
    }

    async fn resolve_configured_proxy_snapshot_with_selection_context(
        &self,
        raw: Option<&Value>,
        context: Option<&ProxyGroupSelectionContext>,
    ) -> Option<ProxySnapshot> {
        let object = raw?.as_object()?;
        if !proxy_enabled(object) {
            return None;
        }

        let mode = json_string_field(object, "mode");
        let group_id = json_string_field(object, "group_id");
        if group_id.is_some() || mode.as_deref() == Some("group") {
            return self
                .resolve_proxy_group_snapshot(group_id.as_deref()?, context)
                .await;
        }

        let node_id = json_string_field(object, "node_id");
        if let Some(snapshot) = self.resolve_proxy_node_snapshot(node_id.as_deref()).await {
            return Some(snapshot);
        }
        if let Some(node_id) = node_id.as_deref() {
            if !proxy_object_has_inline_url(object)
                && self.find_proxy_node(node_id).await.ok().flatten().is_some()
            {
                return None;
            }
        }

        proxy_snapshot_from_object(object)
    }

    async fn resolve_transport_proxy_with_source_with_tunnel_affinity(
        &self,
        transport: &GatewayProviderTransportSnapshot,
    ) -> Option<(ProxySnapshot, &'static str)> {
        let key_context = ProxyGroupSelectionContext::from_transport(transport, "key");
        if let Some(snapshot) = self
            .resolve_configured_proxy_snapshot_with_selection_context(
                transport.key.proxy.as_ref(),
                Some(&key_context),
            )
            .await
        {
            return Some((snapshot, "key"));
        }
        let endpoint_context = ProxyGroupSelectionContext::from_transport(transport, "endpoint");
        if let Some(snapshot) = self
            .resolve_configured_proxy_snapshot_with_selection_context(
                transport.endpoint.proxy.as_ref(),
                Some(&endpoint_context),
            )
            .await
        {
            return Some((snapshot, "endpoint"));
        }
        let provider_context = ProxyGroupSelectionContext::from_transport(transport, "provider");
        if let Some(snapshot) = self
            .resolve_configured_proxy_snapshot_with_selection_context(
                transport.provider.proxy.as_ref(),
                Some(&provider_context),
            )
            .await
        {
            return Some((snapshot, "provider"));
        }
        self.resolve_system_proxy_snapshot()
            .await
            .map(|snapshot| (snapshot, "system"))
    }

    pub(crate) async fn read_proxy_group_member_scores(
        &self,
        group_id: &str,
    ) -> Vec<ProxyGroupMemberScoreSnapshot> {
        let Some(group) = self.find_proxy_group(group_id).await.ok().flatten() else {
            return Vec::new();
        };
        let members = self
            .list_proxy_group_members(&group.id)
            .await
            .unwrap_or_default();
        let strategy = ProxyGroupStrategyConfig::from_group(&group);
        let mut candidates = self
            .evaluate_proxy_group_members(&group, members, &strategy, false)
            .await;
        apply_proxy_group_effective_scores(&group, &mut candidates, &strategy, current_unix_secs());
        candidates
            .into_iter()
            .map(|candidate| {
                let member = candidate.member;
                ProxyGroupMemberScoreSnapshot {
                    group_id: group.id.clone(),
                    node_id: member.node_id,
                    score: candidate.score,
                    effective_score: candidate.effective_score,
                    hard_state: candidate.hard_state,
                    score_reason: candidate.score_reason,
                    sort_index: member.sort_index,
                    enabled: member.enabled,
                    available: candidate.hard_state.schedulable(),
                }
            })
            .collect()
    }

    async fn resolve_proxy_group_snapshot(
        &self,
        group_id: &str,
        context: Option<&ProxyGroupSelectionContext>,
    ) -> Option<ProxySnapshot> {
        let group_id = group_id.trim();
        if group_id.is_empty() {
            return None;
        }
        let group = self.find_proxy_group(group_id).await.ok().flatten()?;
        if !group.enabled {
            return None;
        }
        let members = self.list_proxy_group_members(&group.id).await.ok()?;
        if members.is_empty() {
            return None;
        }

        let strategy = ProxyGroupStrategyConfig::from_group(&group);
        let candidates = self
            .evaluate_proxy_group_members(&group, members, &strategy, true)
            .await
            .into_iter()
            .filter(|candidate| candidate.hard_state.schedulable())
            .collect::<Vec<_>>();
        if candidates.is_empty() {
            return None;
        }

        let now_unix_secs = current_unix_secs();
        let selected =
            select_proxy_group_candidate(&group, candidates, &strategy, context, now_unix_secs)?;
        let mut snapshot = self
            .resolve_proxy_node_snapshot(Some(&selected.node.id))
            .await?;
        attach_proxy_group_extra(&mut snapshot, &group, &selected);
        self.persist_proxy_group_member_score(&group.id, &selected, Some(now_unix_secs))
            .await;
        Some(snapshot)
    }

    async fn evaluate_proxy_group_members(
        &self,
        group: &StoredProxyGroup,
        members: Vec<StoredProxyGroupMember>,
        strategy: &ProxyGroupStrategyConfig,
        persist_scores: bool,
    ) -> Vec<ProxyGroupCandidate> {
        if members.is_empty() {
            return Vec::new();
        }
        let nodes = self.list_proxy_nodes().await.unwrap_or_default();
        let nodes_by_id = nodes
            .into_iter()
            .map(|node| (node.id.clone(), node))
            .collect::<BTreeMap<_, _>>();
        let scope = proxy_group_score_scope();
        let score_ids = members
            .iter()
            .map(|member| {
                let identity = PoolMemberIdentity::proxy_group_member(
                    group.id.clone(),
                    member.node_id.clone(),
                );
                proxy_group_pool_score_id(&identity, &scope)
            })
            .collect::<Vec<_>>();
        let existing_scores = self
            .data
            .get_pool_member_scores_by_ids(&GetPoolMemberScoresByIdsQuery { ids: score_ids })
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|score| (score.member_id.clone(), score))
            .collect::<BTreeMap<_, _>>();
        let now_unix_secs = current_unix_secs();
        let mut candidates = Vec::new();

        for member in members {
            let node = nodes_by_id.get(&member.node_id).cloned();
            let existing = existing_scores.get(&member.node_id);
            let (hard_state, availability_reason) = self
                .proxy_group_member_hard_state(
                    &member,
                    node.as_ref(),
                    existing,
                    strategy,
                    now_unix_secs,
                )
                .await;
            let (score, score_reason, load_ratio, failure_rate) = proxy_group_member_score(
                &member,
                node.as_ref(),
                hard_state,
                availability_reason,
                existing,
                now_unix_secs,
            );
            let candidate = ProxyGroupCandidate {
                node: node.unwrap_or_else(|| missing_proxy_node(&member.node_id)),
                member,
                score,
                effective_score: score,
                hard_state,
                score_reason,
                load_ratio,
                failure_rate,
                existing_score: existing.cloned(),
            };
            if persist_scores {
                self.persist_proxy_group_member_score(&group.id, &candidate, None)
                    .await;
            }
            candidates.push(candidate);
        }

        candidates
    }

    async fn proxy_group_member_hard_state(
        &self,
        member: &StoredProxyGroupMember,
        node: Option<&StoredProxyNode>,
        existing: Option<&StoredPoolMemberScore>,
        strategy: &ProxyGroupStrategyConfig,
        now_unix_secs: u64,
    ) -> (PoolMemberHardState, &'static str) {
        if !member.enabled {
            return (PoolMemberHardState::Inactive, "member_disabled");
        }
        let Some(node) = node else {
            return (PoolMemberHardState::Inactive, "node_missing");
        };
        if node.status.trim() != "online" {
            return (PoolMemberHardState::Inactive, "node_offline");
        }
        if let Some(reason) = proxy_group_circuit_breaker_reason(existing, strategy, now_unix_secs)
        {
            return (PoolMemberHardState::Cooldown, reason);
        }
        if !proxy_node_accepts_new_tunnels(node) {
            return (
                PoolMemberHardState::Cooldown,
                "node_not_accepting_new_tunnels",
            );
        }
        if node.tunnel_mode {
            if !node.tunnel_connected {
                return (PoolMemberHardState::Cooldown, "tunnel_disconnected");
            }
            if self.tunnel.has_local_proxy(&node.id)
                || self
                    .lookup_tunnel_attachment_owner(&node.id)
                    .await
                    .ok()
                    .flatten()
                    .is_some()
            {
                return (PoolMemberHardState::Available, "tunnel_ready");
            }
            return (PoolMemberHardState::Cooldown, "tunnel_unowned");
        }
        if !node.is_manual {
            return (PoolMemberHardState::Inactive, "node_not_manual");
        }
        if node
            .proxy_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_none()
        {
            return (PoolMemberHardState::Inactive, "proxy_url_missing");
        }
        (PoolMemberHardState::Available, "manual_proxy_ready")
    }

    async fn persist_proxy_group_member_score(
        &self,
        group_id: &str,
        candidate: &ProxyGroupCandidate,
        scheduled_at: Option<u64>,
    ) {
        let now_unix_secs = current_unix_secs();
        let identity =
            PoolMemberIdentity::proxy_group_member(group_id.to_string(), candidate.node.id.clone());
        let scope = proxy_group_score_scope();
        let upsert = UpsertPoolMemberScore {
            id: proxy_group_pool_score_id(&identity, &scope),
            identity,
            scope,
            score: candidate.score,
            hard_state: candidate.hard_state,
            score_version: POOL_SCORE_VERSION,
            score_reason: candidate.score_reason.clone(),
            last_ranked_at: Some(now_unix_secs),
            last_scheduled_at: scheduled_at.or_else(|| {
                candidate
                    .existing_score
                    .as_ref()
                    .and_then(|score| score.last_scheduled_at)
            }),
            last_success_at: candidate
                .existing_score
                .as_ref()
                .and_then(|score| score.last_success_at),
            last_failure_at: candidate
                .existing_score
                .as_ref()
                .and_then(|score| score.last_failure_at),
            failure_count: candidate
                .existing_score
                .as_ref()
                .map(|score| score.failure_count)
                .unwrap_or(0),
            last_probe_attempt_at: candidate
                .existing_score
                .as_ref()
                .and_then(|score| score.last_probe_attempt_at),
            last_probe_success_at: candidate
                .existing_score
                .as_ref()
                .and_then(|score| score.last_probe_success_at),
            last_probe_failure_at: candidate
                .existing_score
                .as_ref()
                .and_then(|score| score.last_probe_failure_at),
            probe_failure_count: candidate
                .existing_score
                .as_ref()
                .map(|score| score.probe_failure_count)
                .unwrap_or(0),
            probe_status: candidate
                .existing_score
                .as_ref()
                .map(|score| score.probe_status)
                .unwrap_or(PoolMemberProbeStatus::Never),
            updated_at: now_unix_secs,
        };
        let _ = self.data.upsert_pool_member_score(upsert).await;
    }
}

impl ProxyGroupSelectionContext {
    fn from_transport(transport: &GatewayProviderTransportSnapshot, source: &'static str) -> Self {
        Self {
            source,
            provider_id: transport.provider.id.clone(),
            endpoint_id: transport.endpoint.id.clone(),
            key_id: transport.key.id.clone(),
        }
    }

    fn sticky_key(&self, group_id: &str) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            group_id, self.source, self.provider_id, self.endpoint_id, self.key_id
        )
    }
}

impl ProxyGroupStrategyConfig {
    fn from_group(group: &StoredProxyGroup) -> Self {
        let strategy = group.strategy.trim().to_ascii_lowercase();
        let kind = match strategy.as_str() {
            PROXY_GROUP_STRATEGY_BALANCED_WEIGHTED | "balanced" | "weighted_top_n" => {
                ProxyGroupStrategyKind::BalancedWeighted
            }
            PROXY_GROUP_STRATEGY_STABLE_FAILOVER | "stable" | "failover" | "sticky" => {
                ProxyGroupStrategyKind::StableFailover
            }
            PROXY_GROUP_STRATEGY_SUCCESS_RATE | "success" | "success_rate_first" => {
                ProxyGroupStrategyKind::SuccessRate
            }
            PROXY_GROUP_STRATEGY_MANUAL_PRIORITY | "manual" | "manual_first" => {
                ProxyGroupStrategyKind::ManualPriority
            }
            _ => ProxyGroupStrategyKind::BalancedWeighted,
        };
        Self {
            kind,
            top_n: group.top_n.max(1) as usize,
            circuit_breaker_enabled: true,
            circuit_breaker_failure_threshold: PROXY_GROUP_CIRCUIT_BREAKER_FAILURE_THRESHOLD,
            circuit_breaker_cooldown_secs: PROXY_GROUP_CIRCUIT_BREAKER_COOLDOWN_SECS,
        }
    }

    fn uses_top_n(self) -> bool {
        !matches!(
            self.kind,
            ProxyGroupStrategyKind::StableFailover | ProxyGroupStrategyKind::ManualPriority
        )
    }

    fn load_penalty(self) -> f64 {
        match self.kind {
            ProxyGroupStrategyKind::BalancedWeighted => 28.0,
            ProxyGroupStrategyKind::StableFailover => 8.0,
            ProxyGroupStrategyKind::SuccessRate => 14.0,
            ProxyGroupStrategyKind::ManualPriority => 10.0,
        }
    }

    fn failure_penalty(self) -> f64 {
        match self.kind {
            ProxyGroupStrategyKind::SuccessRate => 35.0,
            ProxyGroupStrategyKind::StableFailover => 10.0,
            ProxyGroupStrategyKind::BalancedWeighted => 15.0,
            ProxyGroupStrategyKind::ManualPriority => 8.0,
        }
    }
}

fn select_proxy_group_candidate(
    group: &StoredProxyGroup,
    mut candidates: Vec<ProxyGroupCandidate>,
    strategy: &ProxyGroupStrategyConfig,
    context: Option<&ProxyGroupSelectionContext>,
    now_unix_secs: u64,
) -> Option<ProxyGroupCandidate> {
    if candidates.is_empty() {
        return None;
    }

    sort_proxy_group_candidates_by_score(&mut candidates);
    if strategy.uses_top_n() {
        candidates.truncate(strategy.top_n.max(1));
    }
    apply_proxy_group_effective_scores(group, &mut candidates, strategy, now_unix_secs);

    match strategy.kind {
        ProxyGroupStrategyKind::BalancedWeighted => {
            select_weighted_proxy_group_candidate(group, &candidates, context, now_unix_secs)
        }
        ProxyGroupStrategyKind::StableFailover => {
            select_stable_failover_proxy_group_candidate(group, &mut candidates, context)
        }
        ProxyGroupStrategyKind::SuccessRate => {
            sort_proxy_group_candidates_by_effective_score(&mut candidates);
            candidates.into_iter().next()
        }
        ProxyGroupStrategyKind::ManualPriority => {
            candidates.sort_by(|left, right| {
                right
                    .member
                    .manual_weight
                    .partial_cmp(&left.member.manual_weight)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(left.member.sort_index.cmp(&right.member.sort_index))
                    .then(
                        right
                            .effective_score
                            .partial_cmp(&left.effective_score)
                            .unwrap_or(std::cmp::Ordering::Equal),
                    )
                    .then(left.node.id.cmp(&right.node.id))
            });
            candidates.into_iter().next()
        }
    }
}

fn sort_proxy_group_candidates_by_score(candidates: &mut [ProxyGroupCandidate]) {
    candidates.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.member.sort_index.cmp(&right.member.sort_index))
            .then(left.node.id.cmp(&right.node.id))
    });
}

fn sort_proxy_group_candidates_by_effective_score(candidates: &mut [ProxyGroupCandidate]) {
    candidates.sort_by(|left, right| {
        right
            .effective_score
            .partial_cmp(&left.effective_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.member.sort_index.cmp(&right.member.sort_index))
            .then(left.node.id.cmp(&right.node.id))
    });
}

fn apply_proxy_group_effective_scores(
    group: &StoredProxyGroup,
    candidates: &mut [ProxyGroupCandidate],
    strategy: &ProxyGroupStrategyConfig,
    now_unix_secs: u64,
) {
    let bucket = now_unix_secs / 60;
    for candidate in candidates {
        candidate.effective_score =
            proxy_group_effective_score(&group.id, candidate, bucket, strategy);
    }
}

fn select_weighted_proxy_group_candidate(
    group: &StoredProxyGroup,
    candidates: &[ProxyGroupCandidate],
    context: Option<&ProxyGroupSelectionContext>,
    now_unix_secs: u64,
) -> Option<ProxyGroupCandidate> {
    let total_weight = candidates
        .iter()
        .map(proxy_group_candidate_selection_weight)
        .sum::<f64>();
    if total_weight <= 0.0 || !total_weight.is_finite() {
        return candidates.first().cloned();
    }

    let seed = dynamic_proxy_group_selection_seed(group, context, now_unix_secs);
    let mut cursor = ((seed as f64 / u64::MAX as f64) * total_weight).clamp(0.0, total_weight);
    for candidate in candidates {
        let weight = proxy_group_candidate_selection_weight(candidate);
        if cursor <= weight {
            return Some(candidate.clone());
        }
        cursor -= weight;
    }
    candidates.last().cloned()
}

fn select_stable_failover_proxy_group_candidate(
    group: &StoredProxyGroup,
    candidates: &mut [ProxyGroupCandidate],
    context: Option<&ProxyGroupSelectionContext>,
) -> Option<ProxyGroupCandidate> {
    if let Some(context) = context {
        let sticky_key = context.sticky_key(&group.id);
        candidates.sort_by(|left, right| {
            let left_score = proxy_group_sticky_score(&sticky_key, left);
            let right_score = proxy_group_sticky_score(&sticky_key, right);
            right_score
                .partial_cmp(&left_score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(left.member.sort_index.cmp(&right.member.sort_index))
                .then(
                    right
                        .effective_score
                        .partial_cmp(&left.effective_score)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(left.node.id.cmp(&right.node.id))
        });
    } else {
        candidates.sort_by(|left, right| {
            left.member
                .sort_index
                .cmp(&right.member.sort_index)
                .then(
                    right
                        .effective_score
                        .partial_cmp(&left.effective_score)
                        .unwrap_or(std::cmp::Ordering::Equal),
                )
                .then(left.node.id.cmp(&right.node.id))
        });
    }
    candidates.first().cloned()
}

fn proxy_group_candidate_selection_weight(candidate: &ProxyGroupCandidate) -> f64 {
    let score_weight = (candidate.effective_score.max(0.0) / 100.0).powf(1.5);
    let manual_weight = candidate.member.manual_weight.clamp(0.05, 2.0);
    let load_weight = (1.0 - candidate.load_ratio.clamp(0.0, 0.95)).max(0.05);
    (score_weight * manual_weight * load_weight).max(0.001)
}

fn proxy_group_sticky_score(sticky_key: &str, candidate: &ProxyGroupCandidate) -> f64 {
    let raw = format!("{}:{}", sticky_key, candidate.node.id);
    let hash_score = (stable_hash(raw.as_bytes()) as f64 / u64::MAX as f64).clamp(0.000_001, 1.0);
    let quality = proxy_group_candidate_selection_weight(candidate).max(0.001);
    hash_score * quality
}

fn dynamic_proxy_group_selection_seed(
    group: &StoredProxyGroup,
    context: Option<&ProxyGroupSelectionContext>,
    now_unix_secs: u64,
) -> u64 {
    let counter = PROXY_GROUP_SELECTION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or((now_unix_secs as i64).saturating_mul(1_000_000_000));
    let context_key = context
        .map(|context| context.sticky_key(&group.id))
        .unwrap_or_else(|| group.id.clone());
    let raw = format!("{}:{}:{}:{}", group.id, context_key, nanos, counter);
    stable_hash(raw.as_bytes())
}

fn proxy_group_circuit_breaker_reason(
    existing: Option<&StoredPoolMemberScore>,
    strategy: &ProxyGroupStrategyConfig,
    now_unix_secs: u64,
) -> Option<&'static str> {
    if !strategy.circuit_breaker_enabled {
        return None;
    }
    let existing = existing?;
    if existing.failure_count >= strategy.circuit_breaker_failure_threshold
        && existing.last_failure_at.is_some_and(|last_failure| {
            now_unix_secs.saturating_sub(last_failure) <= strategy.circuit_breaker_cooldown_secs
        })
    {
        return Some("circuit_breaker_open");
    }
    if existing.probe_failure_count >= strategy.circuit_breaker_failure_threshold
        && existing.last_probe_failure_at.is_some_and(|last_failure| {
            now_unix_secs.saturating_sub(last_failure) <= strategy.circuit_breaker_cooldown_secs
        })
    {
        return Some("probe_circuit_breaker_open");
    }
    None
}

fn proxy_group_member_score(
    member: &StoredProxyGroupMember,
    node: Option<&StoredProxyNode>,
    hard_state: PoolMemberHardState,
    availability_reason: &'static str,
    existing: Option<&StoredPoolMemberScore>,
    now_unix_secs: u64,
) -> (f64, Value, f64, f64) {
    let load_ratio = node.map(proxy_node_load_ratio).unwrap_or(1.0);
    let failure_rate = node.map(proxy_node_failure_rate).unwrap_or(1.0);
    let latency_score = node.map(proxy_node_latency_score).unwrap_or(0.0);
    let freshness_score = node
        .map(|node| proxy_node_freshness_score(node, existing, now_unix_secs))
        .unwrap_or(0.0);
    let scheduling_state =
        node.and_then(|node| remote_config_scheduling_state(node.remote_config.as_ref()));
    let manual_multiplier = member.manual_weight.clamp(0.0, 2.0);
    let accepts_new = node.map(proxy_node_accepts_new_tunnels).unwrap_or(false);
    let route_ready = matches!(availability_reason, "manual_proxy_ready" | "tunnel_ready");
    let available = hard_state.schedulable();
    let base_score = if available {
        20.0 + if accepts_new { 15.0 } else { 0.0 }
            + if route_ready { 15.0 } else { 0.0 }
            + ((1.0 - failure_rate).clamp(0.0, 1.0) * 20.0)
            + (latency_score * 15.0)
            + ((1.0 - load_ratio).clamp(0.0, 1.0) * 10.0)
            + (freshness_score * 5.0)
    } else {
        0.0
    };
    let score = (base_score * manual_multiplier).clamp(0.0, 100.0);
    let reason = json!({
        "availability": availability_reason,
        "member_enabled": member.enabled,
        "manual_weight": member.manual_weight,
        "manual_multiplier": manual_multiplier,
        "node_status": node.map(|node| node.status.as_str()),
        "scheduling_state": scheduling_state,
        "tunnel_mode": node.map(|node| node.tunnel_mode),
        "tunnel_connected": node.map(|node| node.tunnel_connected),
        "has_proxy_url": node.map(|node| {
            node.proxy_url
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty())
        }),
        "failure_rate": failure_rate,
        "avg_latency_ms": node.and_then(|node| node.avg_latency_ms),
        "latency_score": latency_score,
        "active_connections": node.map(|node| node.active_connections),
        "estimated_max_concurrency": node.and_then(|node| node.estimated_max_concurrency),
        "load_ratio": load_ratio,
        "freshness_score": freshness_score,
        "last_heartbeat_at_unix_secs": node.and_then(|node| node.last_heartbeat_at_unix_secs),
        "last_probe_success_at": existing.and_then(|score| score.last_probe_success_at),
        "last_failure_at": existing.and_then(|score| score.last_failure_at),
        "failure_count": existing.map(|score| score.failure_count).unwrap_or(0),
        "probe_failure_count": existing.map(|score| score.probe_failure_count).unwrap_or(0),
        "hard_state": hard_state.as_database(),
        "score": score,
    });
    (score, reason, load_ratio, failure_rate)
}

fn proxy_node_failure_rate(node: &StoredProxyNode) -> f64 {
    if node.total_requests <= 0 {
        return 0.0;
    }
    let failures = node.failed_requests + node.dns_failures + node.stream_errors;
    (failures.max(0) as f64 / node.total_requests.max(1) as f64).clamp(0.0, 1.0)
}

fn proxy_node_latency_score(node: &StoredProxyNode) -> f64 {
    let Some(latency_ms) = node.avg_latency_ms else {
        return 0.75;
    };
    if !latency_ms.is_finite() || latency_ms <= 0.0 {
        return 0.75;
    }
    (1.0 - (latency_ms / 5_000.0)).clamp(0.0, 1.0)
}

fn proxy_node_load_ratio(node: &StoredProxyNode) -> f64 {
    let max_concurrency = node.estimated_max_concurrency.unwrap_or(100).max(1) as f64;
    (node.active_connections.max(0) as f64 / max_concurrency).clamp(0.0, 1.5)
}

fn proxy_node_freshness_score(
    node: &StoredProxyNode,
    existing: Option<&StoredPoolMemberScore>,
    now_unix_secs: u64,
) -> f64 {
    if node.is_manual && !node.tunnel_mode {
        return 1.0;
    }
    if existing
        .and_then(|score| score.last_probe_success_at)
        .is_some_and(|last_probe| now_unix_secs.saturating_sub(last_probe) <= 300)
    {
        return 1.0;
    }
    let Some(last_heartbeat) = node.last_heartbeat_at_unix_secs else {
        return 0.35;
    };
    let age = now_unix_secs.saturating_sub(last_heartbeat);
    let heartbeat = node.heartbeat_interval.max(5) as u64;
    if age <= heartbeat.saturating_mul(3) {
        1.0
    } else if age <= heartbeat.saturating_mul(10) {
        0.55
    } else {
        0.2
    }
}

fn proxy_group_effective_score(
    group_id: &str,
    candidate: &ProxyGroupCandidate,
    selection_bucket: u64,
    strategy: &ProxyGroupStrategyConfig,
) -> f64 {
    let rotation_raw = format!("{}:{}:{}", group_id, candidate.node.id, selection_bucket);
    let rotation_bonus = (stable_hash(rotation_raw.as_bytes()) % 1_000) as f64 / 1_000_000.0;
    candidate.score
        - (candidate.load_ratio.clamp(0.0, 1.0) * strategy.load_penalty())
        - (candidate.failure_rate.clamp(0.0, 1.0) * strategy.failure_penalty())
        - (candidate.node.active_connections.max(0) as f64 * 0.02)
        + rotation_bonus
}

fn attach_proxy_group_extra(
    snapshot: &mut ProxySnapshot,
    group: &StoredProxyGroup,
    candidate: &ProxyGroupCandidate,
) {
    let mut extra = match snapshot.extra.take() {
        Some(Value::Object(map)) => map,
        Some(value) => {
            let mut map = Map::new();
            map.insert("resolved_proxy_extra".to_string(), value);
            map
        }
        None => Map::new(),
    };
    extra.insert(
        "proxy_group_id".to_string(),
        Value::String(group.id.clone()),
    );
    extra.insert(
        "proxy_group_name".to_string(),
        Value::String(group.name.clone()),
    );
    extra.insert(
        "proxy_group_strategy".to_string(),
        Value::String(group.strategy.clone()),
    );
    extra.insert("proxy_group_top_n".to_string(), json!(group.top_n));
    extra.insert(
        "proxy_group_member_score".to_string(),
        json!(candidate.score),
    );
    extra.insert(
        "proxy_group_member_effective_score".to_string(),
        json!(candidate.effective_score),
    );
    extra.insert(
        "proxy_group_member_load_ratio".to_string(),
        json!(candidate.load_ratio),
    );
    extra.insert(
        "proxy_group_member_score_reason".to_string(),
        candidate.score_reason.clone(),
    );
    snapshot.extra = Some(Value::Object(extra));
}

fn proxy_group_score_scope() -> PoolScoreScope {
    PoolScoreScope {
        capability: POOL_SCORE_CAPABILITY_PROXY.to_string(),
        scope_kind: POOL_SCORE_SCOPE_KIND_PROXY_GROUP.to_string(),
        scope_id: None,
    }
}

fn proxy_group_pool_score_id(identity: &PoolMemberIdentity, scope: &PoolScoreScope) -> String {
    let raw = format!(
        "{}:{}:{}:{}:{}:{}:{}",
        identity.pool_kind,
        identity.pool_id,
        identity.member_kind,
        identity.member_id,
        scope.capability,
        scope.scope_kind,
        scope.scope_id.as_deref().unwrap_or("*")
    );
    format!(
        "pms-{:016x}-{:016x}",
        stable_hash(raw.as_bytes()),
        stable_hash(identity.member_id.as_bytes())
    )
}

fn stable_hash(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn current_unix_secs() -> u64 {
    chrono::Utc::now().timestamp().max(0) as u64
}

fn missing_proxy_node(node_id: &str) -> StoredProxyNode {
    StoredProxyNode::new(
        node_id.to_string(),
        node_id.to_string(),
        "0.0.0.0".to_string(),
        0,
        false,
        "missing".to_string(),
        30,
        0,
        0,
        0,
        0,
        0,
        false,
        false,
        0,
    )
    .expect("missing proxy node placeholder should build")
}

fn proxy_enabled(object: &Map<String, Value>) -> bool {
    object
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(true)
}

fn proxy_snapshot_from_object(object: &Map<String, Value>) -> Option<ProxySnapshot> {
    let mode = json_string_field(object, "mode");
    let node_id = json_string_field(object, "node_id");
    let label = json_string_field(object, "label");
    let url = json_string_field(object, "url").or_else(|| json_string_field(object, "proxy_url"));

    if node_id.is_none() && url.is_none() {
        return None;
    }

    let mut extra = Map::new();
    for (key, value) in object {
        if matches!(
            key.as_str(),
            "enabled" | "mode" | "node_id" | "label" | "url" | "proxy_url"
        ) {
            continue;
        }
        extra.insert(key.clone(), value.clone());
    }

    Some(ProxySnapshot {
        enabled: object.get("enabled").and_then(Value::as_bool),
        mode,
        node_id,
        label,
        url,
        extra: if extra.is_empty() {
            None
        } else {
            Some(Value::Object(extra))
        },
    })
}

fn proxy_object_has_inline_url(object: &Map<String, Value>) -> bool {
    json_string_field(object, "url").is_some() || json_string_field(object, "proxy_url").is_some()
}

fn json_string_field(object: &Map<String, Value>, key: &str) -> Option<String> {
    object
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn proxy_mode_from_url(proxy_url: Option<&str>) -> Option<String> {
    let proxy_url = proxy_url?.trim();
    if proxy_url.is_empty() {
        return None;
    }
    let scheme = url::Url::parse(proxy_url)
        .ok()
        .map(|value| value.scheme().to_ascii_lowercase())
        .unwrap_or_default();
    if scheme.starts_with("socks") {
        Some("socks".to_string())
    } else {
        Some("http".to_string())
    }
}

fn proxy_url_with_node_auth(
    proxy_url: &str,
    username: Option<&str>,
    password: Option<&str>,
) -> Option<String> {
    let username = username.map(str::trim).filter(|value| !value.is_empty())?;
    let mut parsed = url::Url::parse(proxy_url).ok()?;
    if parsed.set_username(username).is_err() {
        return None;
    }
    let password = password.map(str::trim).filter(|value| !value.is_empty());
    if parsed.set_password(password).is_err() {
        return None;
    }
    Some(parsed.to_string())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use aether_data::repository::pool_scores::InMemoryPoolMemberScoreRepository;
    use aether_data::repository::proxy_nodes::{
        InMemoryProxyNodeRepository, ProxyNodeReadRepository, StoredProxyGroup,
        StoredProxyGroupMember, StoredProxyNode,
    };
    use aether_data_contracts::repository::pool_scores::{
        ListPoolMemberScoresQuery, PoolMemberHardState, PoolMemberIdentity, PoolMemberProbeStatus,
        PoolScoreReadRepository, StoredPoolMemberScore, POOL_KIND_PROXY_GROUP,
    };
    use serde_json::json;

    use super::proxy_url_with_node_auth;
    use crate::provider_transport::snapshot::{
        GatewayProviderTransportEndpoint, GatewayProviderTransportKey,
        GatewayProviderTransportProvider, GatewayProviderTransportSnapshot,
    };
    use crate::{data::GatewayDataState, AppState};

    #[test]
    fn proxy_url_with_node_auth_omits_empty_password_separator() {
        assert_eq!(
            proxy_url_with_node_auth("socks5://proxy.example:1080", Some("alice"), None).as_deref(),
            Some("socks5://alice@proxy.example:1080")
        );
    }

    #[tokio::test]
    async fn resolve_proxy_node_snapshot_rejects_unroutable_tunnel_node() {
        let repository = Arc::new(InMemoryProxyNodeRepository::seed(vec![sample_tunnel_node(
            "proxy-node-stale",
        )]));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(GatewayDataState::with_proxy_node_repository_for_tests(
                repository,
            ));

        let snapshot = state
            .resolve_proxy_node_snapshot(Some("proxy-node-stale"))
            .await;

        assert_eq!(snapshot, None);
    }

    #[tokio::test]
    async fn resolve_proxy_node_snapshot_keeps_tunnel_node_with_owner_hint() {
        let repository = Arc::new(InMemoryProxyNodeRepository::seed(vec![sample_tunnel_node(
            "proxy-node-owned",
        )]));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(
                GatewayDataState::with_proxy_node_repository_for_tests(repository)
                    .with_system_config_values_for_tests(vec![(
                        "tunnel.attachments.proxy-node-owned".to_string(),
                        json!({
                            "gateway_instance_id": "gateway-owner",
                            "relay_base_url": "http://gateway-owner.internal",
                            "conn_count": 1,
                            "observed_at_unix_secs": 4_102_444_800u64,
                        }),
                    )]),
            );

        let snapshot = state
            .resolve_proxy_node_snapshot(Some("proxy-node-owned"))
            .await
            .expect("owned tunnel snapshot should resolve");

        assert_eq!(snapshot.mode.as_deref(), Some("tunnel"));
        assert_eq!(snapshot.node_id.as_deref(), Some("proxy-node-owned"));
        assert_eq!(
            snapshot
                .extra
                .as_ref()
                .and_then(|extra| extra.get("tunnel_base_url"))
                .and_then(serde_json::Value::as_str),
            Some("http://gateway-owner.internal")
        );
    }

    #[tokio::test]
    async fn resolve_configured_proxy_snapshot_rejects_unroutable_stored_tunnel_reference() {
        let repository = Arc::new(InMemoryProxyNodeRepository::seed(vec![sample_tunnel_node(
            "proxy-node-stale",
        )]));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(GatewayDataState::with_proxy_node_repository_for_tests(
                repository,
            ));

        let snapshot = state
            .resolve_configured_proxy_snapshot_with_tunnel_affinity(Some(&json!({
                "node_id": "proxy-node-stale",
                "enabled": true,
            })))
            .await;

        assert_eq!(snapshot, None);
    }

    #[tokio::test]
    async fn resolve_configured_proxy_snapshot_keeps_inline_url_when_stored_node_is_unroutable() {
        let repository = Arc::new(InMemoryProxyNodeRepository::seed(vec![sample_tunnel_node(
            "proxy-node-stale",
        )]));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(GatewayDataState::with_proxy_node_repository_for_tests(
                repository,
            ));

        let snapshot = state
            .resolve_configured_proxy_snapshot_with_tunnel_affinity(Some(&json!({
                "node_id": "proxy-node-stale",
                "url": "http://proxy.example:8080",
                "enabled": true,
            })))
            .await
            .expect("inline proxy URL should still resolve");

        assert_eq!(snapshot.node_id.as_deref(), Some("proxy-node-stale"));
        assert_eq!(snapshot.url.as_deref(), Some("http://proxy.example:8080"));
    }

    #[tokio::test]
    async fn resolve_configured_proxy_snapshot_resolves_proxy_group_member() {
        let repository = Arc::new(InMemoryProxyNodeRepository::seed_with_proxy_groups(
            vec![sample_manual_node(
                "proxy-node-1",
                "http://proxy.example:8080",
            )],
            vec![sample_proxy_group("proxy-group-1")],
            vec![sample_proxy_group_member(
                "proxy-group-1",
                "proxy-node-1",
                true,
            )],
        ));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(GatewayDataState::with_proxy_node_repository_for_tests(
                repository,
            ));

        let snapshot = state
            .resolve_configured_proxy_snapshot_with_tunnel_affinity(Some(&json!({
                "mode": "group",
                "group_id": "proxy-group-1",
                "enabled": true,
            })))
            .await
            .expect("proxy group should resolve to member node");

        assert_eq!(snapshot.node_id.as_deref(), Some("proxy-node-1"));
        assert_eq!(snapshot.url.as_deref(), Some("http://proxy.example:8080"));
        assert_eq!(
            snapshot
                .extra
                .as_ref()
                .and_then(|extra| extra.get("proxy_group_id"))
                .and_then(serde_json::Value::as_str),
            Some("proxy-group-1")
        );
    }

    #[tokio::test]
    async fn resolve_configured_proxy_snapshot_treats_disabled_group_member_as_unavailable() {
        let repository = Arc::new(InMemoryProxyNodeRepository::seed_with_proxy_groups(
            vec![sample_manual_node(
                "proxy-node-1",
                "http://proxy.example:8080",
            )],
            vec![sample_proxy_group("proxy-group-1")],
            vec![sample_proxy_group_member(
                "proxy-group-1",
                "proxy-node-1",
                false,
            )],
        ));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(GatewayDataState::with_proxy_node_repository_for_tests(
                repository,
            ));

        let snapshot = state
            .resolve_configured_proxy_snapshot_with_tunnel_affinity(Some(&json!({
                "group_id": "proxy-group-1",
                "enabled": true,
            })))
            .await;

        assert_eq!(snapshot, None);
    }

    #[tokio::test]
    async fn resolve_configured_proxy_snapshot_filters_unusable_proxy_group_members() {
        let mut offline = sample_manual_node("offline-node", "http://offline.example:8080");
        offline.status = "offline".to_string();

        let mut draining = sample_manual_node("draining-node", "http://draining.example:8080");
        draining.remote_config = Some(json!({ "scheduling_state": "draining" }));

        let mut missing_url = sample_manual_node("missing-url-node", "http://missing.example:8080");
        missing_url.proxy_url = None;

        let stale_tunnel = sample_tunnel_node("stale-tunnel-node");
        let ready = sample_manual_node("ready-node", "http://ready.example:8080");

        let repository = Arc::new(InMemoryProxyNodeRepository::seed_with_proxy_groups(
            vec![offline, draining, missing_url, stale_tunnel, ready],
            vec![sample_proxy_group("proxy-group-1")],
            vec![
                sample_proxy_group_member("proxy-group-1", "offline-node", true),
                sample_proxy_group_member("proxy-group-1", "draining-node", true),
                sample_proxy_group_member("proxy-group-1", "missing-url-node", true),
                sample_proxy_group_member("proxy-group-1", "stale-tunnel-node", true),
                sample_proxy_group_member("proxy-group-1", "ready-node", true),
            ],
        ));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(GatewayDataState::with_proxy_node_repository_for_tests(
                repository,
            ));

        let snapshot = state
            .resolve_configured_proxy_snapshot_with_tunnel_affinity(Some(&json!({
                "mode": "group",
                "group_id": "proxy-group-1",
                "enabled": true,
            })))
            .await
            .expect("ready group member should be selected");

        assert_eq!(snapshot.node_id.as_deref(), Some("ready-node"));
        assert_eq!(snapshot.url.as_deref(), Some("http://ready.example:8080"));

        let scores = state.read_proxy_group_member_scores("proxy-group-1").await;
        assert_eq!(scores.iter().filter(|score| score.available).count(), 1);
        assert!(scores
            .iter()
            .any(|score| score.node_id == "ready-node" && score.available));
    }

    #[tokio::test]
    async fn resolve_configured_proxy_snapshot_prefers_high_score_group_member() {
        let repository = Arc::new(InMemoryProxyNodeRepository::seed_with_proxy_groups(
            vec![
                sample_manual_node("low-score-node", "http://low.example:8080"),
                sample_manual_node("high-score-node", "http://high.example:8080"),
            ],
            vec![sample_proxy_group("proxy-group-1")],
            vec![
                sample_proxy_group_member_with_weight("proxy-group-1", "low-score-node", 0.25, 0),
                sample_proxy_group_member_with_weight("proxy-group-1", "high-score-node", 1.0, 1),
            ],
        ));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(GatewayDataState::with_proxy_node_repository_for_tests(
                repository,
            ));

        let snapshot = state
            .resolve_configured_proxy_snapshot_with_tunnel_affinity(Some(&json!({
                "group_id": "proxy-group-1",
                "enabled": true,
            })))
            .await
            .expect("high score group member should resolve");

        assert_eq!(snapshot.node_id.as_deref(), Some("high-score-node"));
    }

    #[tokio::test]
    async fn resolve_configured_proxy_snapshot_topn_selection_uses_load_not_fixed_first() {
        let mut busy = sample_manual_node("busy-node", "http://busy.example:8080");
        busy.active_connections = 100;
        busy.estimated_max_concurrency = Some(100);

        let mut idle = sample_manual_node("idle-node", "http://idle.example:8080");
        idle.active_connections = 0;
        idle.estimated_max_concurrency = Some(100);

        let repository = Arc::new(InMemoryProxyNodeRepository::seed_with_proxy_groups(
            vec![busy, idle],
            vec![sample_proxy_group("proxy-group-1")],
            vec![
                sample_proxy_group_member_with_weight("proxy-group-1", "busy-node", 1.2, 0),
                sample_proxy_group_member_with_weight("proxy-group-1", "idle-node", 1.0, 1),
            ],
        ));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(GatewayDataState::with_proxy_node_repository_for_tests(
                repository,
            ));

        let snapshot = state
            .resolve_configured_proxy_snapshot_with_tunnel_affinity(Some(&json!({
                "group_id": "proxy-group-1",
                "enabled": true,
            })))
            .await
            .expect("loaded topn group should resolve");

        assert_eq!(snapshot.node_id.as_deref(), Some("idle-node"));
    }

    #[tokio::test]
    async fn resolve_configured_proxy_snapshot_filters_circuit_broken_group_members() {
        let proxy_node_repository = Arc::new(InMemoryProxyNodeRepository::seed_with_proxy_groups(
            vec![
                sample_manual_node("cooldown-node", "http://cooldown.example:8080"),
                sample_manual_node("ready-node", "http://ready.example:8080"),
            ],
            vec![sample_proxy_group_with_strategy(
                "proxy-group-1",
                "balanced_weighted",
                2,
            )],
            vec![
                sample_proxy_group_member_with_weight("proxy-group-1", "cooldown-node", 2.0, 0),
                sample_proxy_group_member_with_weight("proxy-group-1", "ready-node", 1.0, 1),
            ],
        ));
        let mut cooldown_score = sample_proxy_group_score("proxy-group-1", "cooldown-node");
        cooldown_score.failure_count = 3;
        cooldown_score.last_failure_at = Some(super::current_unix_secs());
        let pool_score_repository = Arc::new(InMemoryPoolMemberScoreRepository::seed(vec![
            cooldown_score,
        ]));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(
                GatewayDataState::with_proxy_node_repository_for_tests(proxy_node_repository)
                    .with_pool_score_repository_for_tests(pool_score_repository),
            );

        let snapshot = state
            .resolve_configured_proxy_snapshot_with_tunnel_affinity(Some(&json!({
                "group_id": "proxy-group-1",
                "enabled": true,
            })))
            .await
            .expect("ready group member should resolve");

        assert_eq!(snapshot.node_id.as_deref(), Some("ready-node"));
        let scores = state.read_proxy_group_member_scores("proxy-group-1").await;
        let cooldown = scores
            .iter()
            .find(|score| score.node_id == "cooldown-node")
            .expect("cooldown node score should exist");
        assert!(!cooldown.available);
        assert_eq!(
            cooldown
                .score_reason
                .get("availability")
                .and_then(serde_json::Value::as_str),
            Some("circuit_breaker_open")
        );
    }

    #[tokio::test]
    async fn resolve_transport_proxy_stable_failover_keeps_sticky_member_for_same_context() {
        let proxy_node_repository = Arc::new(InMemoryProxyNodeRepository::seed_with_proxy_groups(
            vec![
                sample_manual_node("proxy-node-a", "http://a.example:8080"),
                sample_manual_node("proxy-node-b", "http://b.example:8080"),
            ],
            vec![sample_proxy_group_with_strategy(
                "proxy-group-1",
                "stable_failover",
                2,
            )],
            vec![
                sample_proxy_group_member_with_weight("proxy-group-1", "proxy-node-a", 1.0, 0),
                sample_proxy_group_member_with_weight("proxy-group-1", "proxy-node-b", 1.0, 1),
            ],
        ));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(GatewayDataState::with_proxy_node_repository_for_tests(
                proxy_node_repository,
            ));
        let transport = sample_transport(
            None,
            None,
            Some(json!({
                "mode": "group",
                "group_id": "proxy-group-1",
                "enabled": true,
            })),
        );

        let first = state
            .resolve_transport_proxy_snapshot_with_tunnel_affinity(&transport)
            .await
            .expect("first sticky proxy should resolve")
            .node_id
            .expect("first sticky proxy should include node id");

        for _ in 0..5 {
            let next = state
                .resolve_transport_proxy_snapshot_with_tunnel_affinity(&transport)
                .await
                .expect("next sticky proxy should resolve")
                .node_id
                .expect("next sticky proxy should include node id");
            assert_eq!(next, first);
        }
    }

    #[tokio::test]
    async fn resolve_transport_proxy_keeps_key_endpoint_provider_system_fallback_order() {
        let repository = Arc::new(InMemoryProxyNodeRepository::seed(vec![sample_manual_node(
            "system-node",
            "http://system.example:8080",
        )]));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(
                GatewayDataState::with_proxy_node_repository_for_tests(repository)
                    .with_system_config_values_for_tests(vec![(
                        "system_proxy_node_id".to_string(),
                        json!("system-node"),
                    )]),
            );

        let all_configured = sample_transport(
            Some(json!({ "url": "http://provider.example:8080", "enabled": true })),
            Some(json!({ "url": "http://endpoint.example:8080", "enabled": true })),
            Some(json!({ "url": "http://key.example:8080", "enabled": true })),
        );
        let key_snapshot = state
            .resolve_transport_proxy_snapshot_with_tunnel_affinity(&all_configured)
            .await
            .expect("key proxy should resolve");
        assert_eq!(key_snapshot.url.as_deref(), Some("http://key.example:8080"));
        assert_eq!(
            state
                .resolve_transport_proxy_source_with_tunnel_affinity(&all_configured)
                .await,
            Some("key")
        );

        let key_disabled = sample_transport(
            Some(json!({ "url": "http://provider.example:8080", "enabled": true })),
            Some(json!({ "url": "http://endpoint.example:8080", "enabled": true })),
            Some(json!({ "url": "http://key.example:8080", "enabled": false })),
        );
        let endpoint_snapshot = state
            .resolve_transport_proxy_snapshot_with_tunnel_affinity(&key_disabled)
            .await
            .expect("endpoint proxy should resolve");
        assert_eq!(
            endpoint_snapshot.url.as_deref(),
            Some("http://endpoint.example:8080")
        );
        assert_eq!(
            state
                .resolve_transport_proxy_source_with_tunnel_affinity(&key_disabled)
                .await,
            Some("endpoint")
        );

        let endpoint_disabled = sample_transport(
            Some(json!({ "url": "http://provider.example:8080", "enabled": true })),
            Some(json!({ "url": "http://endpoint.example:8080", "enabled": false })),
            None,
        );
        let provider_snapshot = state
            .resolve_transport_proxy_snapshot_with_tunnel_affinity(&endpoint_disabled)
            .await
            .expect("provider proxy should resolve");
        assert_eq!(
            provider_snapshot.url.as_deref(),
            Some("http://provider.example:8080")
        );
        assert_eq!(
            state
                .resolve_transport_proxy_source_with_tunnel_affinity(&endpoint_disabled)
                .await,
            Some("provider")
        );

        let no_configured_proxy = sample_transport(None, None, None);
        let system_snapshot = state
            .resolve_transport_proxy_snapshot_with_tunnel_affinity(&no_configured_proxy)
            .await
            .expect("system proxy should resolve");
        assert_eq!(
            system_snapshot.url.as_deref(),
            Some("http://system.example:8080")
        );
        assert_eq!(
            state
                .resolve_transport_proxy_source_with_tunnel_affinity(&no_configured_proxy)
                .await,
            Some("system")
        );
    }

    #[tokio::test]
    async fn delete_proxy_node_removes_proxy_group_memberships_and_scores() {
        let proxy_node_repository = Arc::new(InMemoryProxyNodeRepository::seed_with_proxy_groups(
            vec![sample_manual_node(
                "proxy-node-1",
                "http://proxy.example:8080",
            )],
            vec![sample_proxy_group("proxy-group-1")],
            vec![sample_proxy_group_member(
                "proxy-group-1",
                "proxy-node-1",
                true,
            )],
        ));
        let pool_score_repository = Arc::new(InMemoryPoolMemberScoreRepository::seed(vec![
            sample_proxy_group_score("proxy-group-1", "proxy-node-1"),
        ]));
        let state = AppState::new()
            .expect("state should build")
            .with_data_state_for_tests(
                GatewayDataState::with_proxy_node_repository_for_tests(Arc::clone(
                    &proxy_node_repository,
                ))
                .with_pool_score_repository_for_tests(Arc::clone(&pool_score_repository)),
            );

        let deleted = state
            .delete_proxy_node("proxy-node-1")
            .await
            .expect("delete should succeed")
            .expect("node should exist");
        assert_eq!(deleted.id, "proxy-node-1");

        assert!(proxy_node_repository
            .list_proxy_group_members("proxy-group-1")
            .await
            .expect("members should list")
            .is_empty());

        let scores = pool_score_repository
            .list_pool_member_scores(&ListPoolMemberScoresQuery {
                pool_kind: POOL_KIND_PROXY_GROUP.to_string(),
                pool_id: "proxy-group-1".to_string(),
                capability: None,
                scope_kind: None,
                scope_id: None,
                hard_states: Vec::new(),
                probe_statuses: None,
                offset: 0,
                limit: 10,
            })
            .await
            .expect("scores should list");
        assert!(scores.is_empty());
    }

    fn sample_tunnel_node(id: &str) -> StoredProxyNode {
        StoredProxyNode::new(
            id.to_string(),
            id.to_string(),
            "127.0.0.1".to_string(),
            0,
            false,
            "online".to_string(),
            15,
            1,
            0,
            0,
            0,
            0,
            true,
            true,
            1,
        )
        .expect("sample tunnel node should build")
    }

    fn sample_manual_node(id: &str, proxy_url: &str) -> StoredProxyNode {
        StoredProxyNode::new(
            id.to_string(),
            id.to_string(),
            "127.0.0.1".to_string(),
            8080,
            true,
            "online".to_string(),
            15,
            0,
            100,
            0,
            0,
            0,
            false,
            false,
            1,
        )
        .expect("sample manual node should build")
        .with_manual_proxy_fields(Some(proxy_url.to_string()), None, None)
        .with_runtime_fields(
            None,
            None,
            Some(4_102_444_800u64),
            Some(80.0),
            None,
            None,
            Some(100),
            None,
            None,
            None,
            None,
        )
    }

    fn sample_proxy_group(id: &str) -> StoredProxyGroup {
        sample_proxy_group_with_strategy(id, "balanced_weighted", 2)
    }

    fn sample_proxy_group_with_strategy(id: &str, strategy: &str, top_n: i32) -> StoredProxyGroup {
        StoredProxyGroup::new(
            id.to_string(),
            id.to_string(),
            true,
            strategy.to_string(),
            top_n,
        )
        .expect("sample proxy group should build")
    }

    fn sample_proxy_group_member(
        group_id: &str,
        node_id: &str,
        enabled: bool,
    ) -> StoredProxyGroupMember {
        StoredProxyGroupMember::new(group_id.to_string(), node_id.to_string(), enabled, 1.0, 0)
            .expect("sample proxy group member should build")
    }

    fn sample_proxy_group_member_with_weight(
        group_id: &str,
        node_id: &str,
        manual_weight: f64,
        sort_index: i32,
    ) -> StoredProxyGroupMember {
        StoredProxyGroupMember::new(
            group_id.to_string(),
            node_id.to_string(),
            true,
            manual_weight,
            sort_index,
        )
        .expect("sample proxy group member should build")
    }

    fn sample_transport(
        provider_proxy: Option<serde_json::Value>,
        endpoint_proxy: Option<serde_json::Value>,
        key_proxy: Option<serde_json::Value>,
    ) -> GatewayProviderTransportSnapshot {
        GatewayProviderTransportSnapshot {
            provider: GatewayProviderTransportProvider {
                id: "provider-1".to_string(),
                name: "Provider".to_string(),
                provider_type: "openai".to_string(),
                website: None,
                is_active: true,
                keep_priority_on_conversion: false,
                enable_format_conversion: false,
                concurrent_limit: None,
                max_retries: None,
                proxy: provider_proxy,
                request_timeout_secs: None,
                stream_first_byte_timeout_secs: None,
                config: None,
            },
            endpoint: GatewayProviderTransportEndpoint {
                id: "endpoint-1".to_string(),
                provider_id: "provider-1".to_string(),
                api_format: "openai".to_string(),
                api_family: None,
                endpoint_kind: None,
                is_active: true,
                base_url: "https://api.example".to_string(),
                header_rules: None,
                body_rules: None,
                max_retries: None,
                custom_path: None,
                config: None,
                format_acceptance_config: None,
                proxy: endpoint_proxy,
            },
            key: GatewayProviderTransportKey {
                id: "key-1".to_string(),
                provider_id: "provider-1".to_string(),
                name: "Key".to_string(),
                auth_type: "api_key".to_string(),
                is_active: true,
                api_formats: None,
                auth_type_by_format: None,
                allow_auth_channel_mismatch_formats: None,
                allowed_models: None,
                capabilities: None,
                rate_multipliers: None,
                global_priority_by_format: None,
                expires_at_unix_secs: None,
                proxy: key_proxy,
                fingerprint: None,
                decrypted_api_key: "sk-test".to_string(),
                decrypted_auth_config: None,
            },
        }
    }

    fn sample_proxy_group_score(group_id: &str, node_id: &str) -> StoredPoolMemberScore {
        let identity = PoolMemberIdentity::proxy_group_member(group_id, node_id);
        let scope = super::proxy_group_score_scope();
        StoredPoolMemberScore {
            id: super::proxy_group_pool_score_id(&identity, &scope),
            pool_kind: identity.pool_kind,
            pool_id: identity.pool_id,
            member_kind: identity.member_kind,
            member_id: identity.member_id,
            capability: scope.capability,
            scope_kind: scope.scope_kind,
            scope_id: scope.scope_id,
            score: 80.0,
            hard_state: PoolMemberHardState::Available,
            score_version: 1,
            score_reason: json!({}),
            last_ranked_at: Some(1),
            last_scheduled_at: None,
            last_success_at: None,
            last_failure_at: None,
            failure_count: 0,
            last_probe_attempt_at: None,
            last_probe_success_at: None,
            last_probe_failure_at: None,
            probe_failure_count: 0,
            probe_status: PoolMemberProbeStatus::Never,
            updated_at: 1,
        }
    }
}
