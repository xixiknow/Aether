import { defineStore } from 'pinia'
import { ref, computed } from 'vue'
import {
  proxyNodesApi,
  type ManualProxyNodeCreateRequest,
  type ProxyGroup,
  type ProxyGroupCreateRequest,
  type ProxyGroupMemberMutation,
  type ProxyGroupUpdateRequest,
  type ProxyNode,
  type ProxyNodeInstallSession,
  type ProxyNodeInstallSessionCreateRequest,
  type ProxyNodeUpgradeRolloutStatus,
} from '@/api/proxy-nodes'
import { parseApiError } from '@/utils/errorParser'

export const useProxyNodesStore = defineStore('proxy-nodes', () => {
  const nodes = ref<ProxyNode[]>([])
  const groups = ref<ProxyGroup[]>([])
  const rollout = ref<ProxyNodeUpgradeRolloutStatus | null>(null)
  const total = ref(0)
  const loading = ref(false)
  const error = ref<string | null>(null)
  /** 标记是否已加载过（避免重复请求） */
  const fetched = ref(false)

  /** 在线节点（可用于代理选择） */
  const onlineNodes = computed(() =>
    nodes.value.filter(n =>
      n.status === 'online'
      && n.remote_config?.scheduling_state !== 'draining'
      && n.remote_config?.scheduling_state !== 'cordoned'
    )
  )

  const enabledGroups = computed(() => groups.value.filter(group => group.enabled))

  async function fetchNodes(params?: { status?: string }) {
    loading.value = true
    error.value = null

    try {
      const data = await proxyNodesApi.listProxyNodes({ ...params, limit: 1000 })
      nodes.value = data.items
      rollout.value = data.rollout
      total.value = data.total
      await fetchGroups()
      fetched.value = true
    } catch (err: unknown) {
      rollout.value = null
      error.value = parseApiError(err, '获取代理节点列表失败')
    } finally {
      loading.value = false
    }
  }

  async function fetchGroups() {
    try {
      const data = await proxyNodesApi.listProxyGroups()
      groups.value = data.items
    } catch (err: unknown) {
      error.value = parseApiError(err, '获取代理组列表失败')
      throw err
    }
  }

  /** 确保节点列表已加载（懒加载，不重复请求） */
  async function ensureLoaded() {
    if (!fetched.value && !loading.value) {
      await fetchNodes()
    }
  }

  async function createManualNode(data: ManualProxyNodeCreateRequest) {
    loading.value = true
    error.value = null

    try {
      const result = await proxyNodesApi.createManualNode(data)
      // 重新获取列表以保持排序一致
      await fetchNodes()
      return result
    } catch (err: unknown) {
      error.value = parseApiError(err, '创建手动代理节点失败')
      throw err
    } finally {
      loading.value = false
    }
  }

  async function createGroup(data: ProxyGroupCreateRequest) {
    loading.value = true
    error.value = null
    try {
      const result = await proxyNodesApi.createProxyGroup(data)
      await fetchGroups()
      return result
    } catch (err: unknown) {
      error.value = parseApiError(err, '创建代理组失败')
      throw err
    } finally {
      loading.value = false
    }
  }

  async function updateGroup(groupId: string, data: ProxyGroupUpdateRequest) {
    loading.value = true
    error.value = null
    try {
      const result = await proxyNodesApi.updateProxyGroup(groupId, data)
      const index = groups.value.findIndex(group => group.id === groupId)
      if (index >= 0) groups.value[index] = result.group
      else groups.value.unshift(result.group)
      return result
    } catch (err: unknown) {
      error.value = parseApiError(err, '更新代理组失败')
      throw err
    } finally {
      loading.value = false
    }
  }

  async function deleteGroup(groupId: string) {
    loading.value = true
    error.value = null
    try {
      await proxyNodesApi.deleteProxyGroup(groupId)
      groups.value = groups.value.filter(group => group.id !== groupId)
    } catch (err: unknown) {
      error.value = parseApiError(err, '删除代理组失败')
      throw err
    } finally {
      loading.value = false
    }
  }

  async function upsertGroupMember(groupId: string, nodeId: string, data: ProxyGroupMemberMutation) {
    const result = await proxyNodesApi.upsertProxyGroupMember(groupId, nodeId, data)
    await fetchGroups()
    return result
  }

  async function updateGroupMember(groupId: string, nodeId: string, data: ProxyGroupMemberMutation) {
    const result = await proxyNodesApi.updateProxyGroupMember(groupId, nodeId, data)
    await fetchGroups()
    return result
  }

  async function deleteGroupMember(groupId: string, nodeId: string) {
    const result = await proxyNodesApi.deleteProxyGroupMember(groupId, nodeId)
    await fetchGroups()
    return result
  }

  async function createInstallSession(data: ProxyNodeInstallSessionCreateRequest): Promise<ProxyNodeInstallSession> {
    try {
      return await proxyNodesApi.createInstallSession(data)
    } catch (err: unknown) {
      error.value = parseApiError(err, '生成代理节点安装命令失败')
      throw err
    }
  }

  async function deleteNode(nodeId: string) {
    loading.value = true
    error.value = null

    try {
      await proxyNodesApi.deleteProxyNode(nodeId)
      nodes.value = nodes.value.filter(n => n.id !== nodeId)
      total.value = Math.max(0, total.value - 1)
      await fetchGroups()
    } catch (err: unknown) {
      error.value = parseApiError(err, '删除代理节点失败')
      throw err
    } finally {
      loading.value = false
    }
  }

  return {
    nodes,
    groups,
    rollout,
    total,
    loading,
    error,
    fetched,
    onlineNodes,
    enabledGroups,
    fetchNodes,
    fetchGroups,
    ensureLoaded,
    createManualNode,
    createGroup,
    updateGroup,
    deleteGroup,
    upsertGroupMember,
    updateGroupMember,
    deleteGroupMember,
    createInstallSession,
    deleteNode,
  }
})
