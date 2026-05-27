CREATE TABLE IF NOT EXISTS proxy_groups (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    description TEXT,
    enabled INTEGER NOT NULL DEFAULT 1,
    strategy TEXT NOT NULL DEFAULT 'balanced_weighted',
    top_n INTEGER NOT NULL DEFAULT 3,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS proxy_group_members (
    group_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    enabled INTEGER NOT NULL DEFAULT 1,
    manual_weight REAL NOT NULL DEFAULT 1,
    sort_index INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (group_id, node_id),
    FOREIGN KEY (group_id) REFERENCES proxy_groups(id) ON DELETE CASCADE,
    FOREIGN KEY (node_id) REFERENCES proxy_nodes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_proxy_groups_enabled
    ON proxy_groups (enabled, name);

CREATE INDEX IF NOT EXISTS idx_proxy_group_members_node_id
    ON proxy_group_members (node_id);

CREATE INDEX IF NOT EXISTS idx_proxy_group_members_group_sort
    ON proxy_group_members (group_id, enabled, sort_index);
