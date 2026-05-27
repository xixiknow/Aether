<template>
  <div class="space-y-1.5">
    <Select
      :model-value="selectValue"
      :disabled="proxyNodesStore.loading || optionCount === 0"
      @update:model-value="(v: string) => $emit('update:modelValue', v)"
    >
      <SelectTrigger :class="triggerClass">
        <SelectValue
          :placeholder="proxyNodesStore.loading
            ? '加载节点列表中...'
            : optionCount === 0
              ? includeGroups ? '暂无可用代理目标' : '暂无可用节点'
              : includeGroups ? '选择代理目标...' : '选择代理节点...'"
        />
      </SelectTrigger>
      <SelectContent>
        <SelectItem
          v-for="group in groupOptions"
          :key="`group:${group.id}`"
          :value="`group:${group.id}`"
        >
          组 · {{ group.name }} · {{ group.available_member_count }}/{{ group.member_count }} 可用
        </SelectItem>
        <SelectItem
          v-for="node in nodeOptions"
          :key="node.id"
          :value="node.id"
        >
          {{ node.name }}{{ node.region ? ` · ${formatRegion(node.region, '')}` : '' }} ({{ node.ip }}:{{ node.port }})
        </SelectItem>
      </SelectContent>
    </Select>
  </div>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import {
  Select,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
} from '@/components/ui'
import { useProxyNodesStore } from '@/stores/proxy-nodes'
import { formatRegion } from '@/utils/region'

const props = defineProps<{
  modelValue: string
  triggerClass?: string
  includeGroups?: boolean
}>()

defineEmits<{
  'update:modelValue': [value: string]
}>()

const proxyNodesStore = useProxyNodesStore()

const includeGroups = computed(() => props.includeGroups === true)

const selectValue = computed(() => props.modelValue || '')

/** 在线节点 + 保留当前已选节点（可能已离线） */
const nodeOptions = computed(() => {
  const online = proxyNodesStore.onlineNodes
  if (props.modelValue) {
    const found = online.find(n => n.id === props.modelValue)
    if (!found) {
      const allNode = proxyNodesStore.nodes.find(n => n.id === props.modelValue)
      if (allNode) return [allNode, ...online]
    }
  }
  return online
})

const groupOptions = computed(() => {
  if (!includeGroups.value) return []
  const groups = proxyNodesStore.enabledGroups
  const selectedGroupId = props.modelValue.startsWith('group:')
    ? props.modelValue.slice('group:'.length)
    : ''
  if (selectedGroupId) {
    const found = groups.find(group => group.id === selectedGroupId)
    if (!found) {
      const allGroup = proxyNodesStore.groups.find(group => group.id === selectedGroupId)
      if (allGroup) return [allGroup, ...groups]
    }
  }
  return groups
})

const optionCount = computed(() => nodeOptions.value.length + groupOptions.value.length)

/** 供父组件调用：启用代理时懒加载节点列表 */
function ensureLoaded() {
  proxyNodesStore.ensureLoaded()
}

defineExpose({ ensureLoaded })
</script>
