mod memory;
mod mysql;
mod postgres;
mod sqlite;
mod types;

pub use memory::InMemoryProxyNodeRepository;
pub use mysql::MysqlProxyNodeReadRepository;
pub use postgres::SqlxProxyNodeRepository;
pub use sqlite::SqliteProxyNodeReadRepository;
pub use types::{
    bucket_start_unix_secs, build_tunnel_error_event_detail, build_tunnel_metrics_sample,
    log_reported_tunnel_error_event, normalize_proxy_node_scheduling_state,
    proxy_node_accepts_new_tunnels, proxy_reported_version,
    reconcile_remote_config_after_heartbeat, remote_config_scheduling_state,
    remote_config_upgrade_target, ProxyGroupCreateMutation, ProxyGroupMemberUpdateMutation,
    ProxyGroupMemberUpsertMutation, ProxyGroupUpdateMutation, ProxyNodeEventQuery,
    ProxyNodeHeartbeatMutation, ProxyNodeManualCreateMutation, ProxyNodeManualUpdateMutation,
    ProxyNodeMetricsCleanupSummary, ProxyNodeMetricsStep, ProxyNodeReadRepository,
    ProxyNodeRegistrationMutation, ProxyNodeRemoteConfigMutation, ProxyNodeTrafficMutation,
    ProxyNodeTunnelStatusMutation, ProxyNodeWriteRepository, StoredProxyFleetMetricsBucket,
    StoredProxyGroup, StoredProxyGroupMember, StoredProxyNode, StoredProxyNodeEvent,
    StoredProxyNodeMetricsBucket, TunnelErrorEventRecord, DEFAULT_PROXY_GROUP_STRATEGY,
    PROXY_NODE_EVENT_TYPE_TUNNEL_ERROR, PROXY_NODE_SCHEDULING_STATE_CORDONED,
    PROXY_NODE_SCHEDULING_STATE_DRAINING,
};
