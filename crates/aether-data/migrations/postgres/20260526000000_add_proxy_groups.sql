CREATE TABLE IF NOT EXISTS public.proxy_groups (
    id character varying(64) PRIMARY KEY,
    name character varying(255) NOT NULL,
    description character varying(500),
    enabled boolean NOT NULL DEFAULT true,
    strategy character varying(64) NOT NULL DEFAULT 'balanced_weighted',
    top_n integer NOT NULL DEFAULT 3,
    created_at timestamp with time zone NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at timestamp with time zone NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE TABLE IF NOT EXISTS public.proxy_group_members (
    group_id character varying(64) NOT NULL,
    node_id character varying(64) NOT NULL,
    enabled boolean NOT NULL DEFAULT true,
    manual_weight double precision NOT NULL DEFAULT 1,
    sort_index integer NOT NULL DEFAULT 0,
    created_at timestamp with time zone NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at timestamp with time zone NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (group_id, node_id)
);

DO $mig$ BEGIN
  ALTER TABLE ONLY public.proxy_group_members
    ADD CONSTRAINT proxy_group_members_group_id_fkey
    FOREIGN KEY (group_id) REFERENCES public.proxy_groups(id) ON DELETE CASCADE;
EXCEPTION
  WHEN duplicate_object THEN NULL;
END $mig$;

DO $mig$ BEGIN
  ALTER TABLE ONLY public.proxy_group_members
    ADD CONSTRAINT proxy_group_members_node_id_fkey
    FOREIGN KEY (node_id) REFERENCES public.proxy_nodes(id) ON DELETE CASCADE;
EXCEPTION
  WHEN duplicate_object THEN NULL;
END $mig$;

CREATE INDEX IF NOT EXISTS idx_proxy_groups_enabled
    ON public.proxy_groups USING btree (enabled, name);

CREATE INDEX IF NOT EXISTS idx_proxy_group_members_node_id
    ON public.proxy_group_members USING btree (node_id);

CREATE INDEX IF NOT EXISTS idx_proxy_group_members_group_sort
    ON public.proxy_group_members USING btree (group_id, enabled, sort_index);
