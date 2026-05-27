import type { ProxyConfig } from '@/api/endpoints/types/provider'

export const PROXY_GROUP_TARGET_PREFIX = 'group:'

export interface ProxyTargetNodeSummary {
  id: string
  name: string
}

export interface ProxyTargetGroupSummary {
  id: string
  name: string
}

export function hasProxyTarget(proxy?: ProxyConfig | null, respectDisabled = false): boolean {
  return proxyTargetValue(proxy, { respectDisabled }) !== ''
}

export function proxyTargetValue(
  proxy?: ProxyConfig | null,
  options: { respectDisabled?: boolean } = {}
): string {
  if (options.respectDisabled && proxy?.enabled === false) return ''
  const groupId = proxy?.group_id?.trim()
  if (groupId) return `${PROXY_GROUP_TARGET_PREFIX}${groupId}`
  return proxy?.node_id?.trim() || ''
}

export function proxyPayloadFromTarget(target: string): ProxyConfig {
  const normalized = target.trim()
  if (normalized.startsWith(PROXY_GROUP_TARGET_PREFIX)) {
    return {
      mode: 'group',
      group_id: normalized.slice(PROXY_GROUP_TARGET_PREFIX.length).trim(),
      enabled: true,
    }
  }
  return { node_id: normalized, enabled: true }
}

export function getProxyTargetName(
  proxy: ProxyConfig | null | undefined,
  groups: readonly ProxyTargetGroupSummary[],
  nodes: readonly ProxyTargetNodeSummary[]
): string | null {
  const groupId = proxy?.group_id?.trim()
  if (groupId) {
    const group = groups.find(group => group.id === groupId)
    return group ? `组 · ${group.name}` : `组 · ${groupId.slice(0, 8)}...`
  }

  const nodeId = proxy?.node_id?.trim()
  if (!nodeId) return null
  const node = nodes.find(node => node.id === nodeId)
  return node ? node.name : `${nodeId.slice(0, 8)}...`
}
