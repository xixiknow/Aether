CREATE TABLE IF NOT EXISTS proxy_groups (
    id VARCHAR(64) PRIMARY KEY,
    name VARCHAR(255) NOT NULL,
    description VARCHAR(500),
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    strategy VARCHAR(64) NOT NULL DEFAULT 'balanced_weighted',
    top_n INT NOT NULL DEFAULT 3,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    KEY idx_proxy_groups_enabled (enabled, name)
);

CREATE TABLE IF NOT EXISTS proxy_group_members (
    group_id VARCHAR(64) NOT NULL,
    node_id VARCHAR(64) NOT NULL,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    manual_weight DOUBLE NOT NULL DEFAULT 1,
    sort_index INT NOT NULL DEFAULT 0,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (group_id, node_id),
    KEY idx_proxy_group_members_node_id (node_id),
    KEY idx_proxy_group_members_group_sort (group_id, enabled, sort_index),
    CONSTRAINT fk_proxy_group_members_group
        FOREIGN KEY (group_id) REFERENCES proxy_groups(id) ON DELETE CASCADE,
    CONSTRAINT fk_proxy_group_members_node
        FOREIGN KEY (node_id) REFERENCES proxy_nodes(id) ON DELETE CASCADE
);
