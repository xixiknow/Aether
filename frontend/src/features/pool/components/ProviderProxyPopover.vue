<template>
  <Popover
    :open="open"
    @update:open="emit('update:open', $event)"
  >
    <PopoverTrigger as-child>
      <Button
        variant="ghost"
        size="icon"
        class="h-8 w-8"
        :class="selectedValue ? 'text-blue-600' : ''"
        :disabled="saving"
        :title="title"
      >
        <Globe class="w-3.5 h-3.5" />
      </Button>
    </PopoverTrigger>
    <PopoverContent
      class="w-72 p-3"
      side="bottom"
      align="end"
    >
      <div class="space-y-2">
        <div class="flex items-center justify-between">
          <span class="text-xs font-medium">提供商代理目标</span>
          <Button
            v-if="selectedValue"
            variant="ghost"
            size="sm"
            class="h-6 px-2 text-[10px] text-muted-foreground"
            :disabled="saving"
            @click="emit('clear')"
          >
            清除
          </Button>
        </div>
        <ProxyNodeSelect
          :model-value="selectedValue"
          include-groups
          trigger-class="h-8"
          @update:model-value="emit('select', $event)"
        />
        <p class="text-[10px] text-muted-foreground">
          {{ selectedValue ? '当前使用提供商独立代理目标' : '未设置，使用系统默认网络出口' }}
        </p>
      </div>
    </PopoverContent>
  </Popover>
</template>

<script setup lang="ts">
import { computed } from 'vue'
import { Globe } from 'lucide-vue-next'
import { Button, Popover, PopoverTrigger, PopoverContent } from '@/components/ui'
import ProxyNodeSelect from '@/features/providers/components/ProxyNodeSelect.vue'

const props = defineProps<{
  open: boolean
  nodeId: string | null | undefined
  targetValue?: string | null
  saving: boolean
  title: string
}>()

const emit = defineEmits<{
  'update:open': [value: boolean]
  select: [nodeId: string]
  clear: []
}>()

const selectedValue = computed(() => props.targetValue || props.nodeId || '')
</script>
