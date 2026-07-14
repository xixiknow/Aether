import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { createApp, defineComponent, h, nextTick, type App } from 'vue'

import ProviderFormDialog from '../ProviderFormDialog.vue'

const apiMocks = vi.hoisted(() => ({
  createProvider: vi.fn(),
  updateProvider: vi.fn(),
}))

const toastMocks = vi.hoisted(() => ({
  success: vi.fn(),
  error: vi.fn(),
}))

vi.mock('@/api/endpoints', () => ({
  createProvider: apiMocks.createProvider,
  updateProvider: apiMocks.updateProvider,
  normalizePoolAdvancedConfig: (value: unknown) => value ?? null,
}))

vi.mock('@/composables/useToast', () => ({
  useToast: () => toastMocks,
}))

vi.mock('@/i18n', () => ({
  useI18n: () => ({ legacyT: (value: string) => value }),
  setI18nLocale: vi.fn(),
}))

vi.mock('@/components/ui', async () => {
  const { defineComponent, h } = await import('vue')
  const passthrough = (name: string, tag = 'div') => defineComponent({
    name,
    setup(_props, { attrs, slots }) {
      return () => h(tag, attrs, slots.default?.())
    },
  })

  return {
    Dialog: defineComponent({
      name: 'DialogStub',
      props: { modelValue: { type: Boolean, default: false } },
      setup(props, { slots }) {
        return () => props.modelValue
          ? h('section', { 'data-testid': 'dialog' }, [slots.default?.(), slots.footer?.()])
          : null
      },
    }),
    Button: passthrough('ButtonStub', 'button'),
    Input: defineComponent({
      name: 'InputStub',
      inheritAttrs: false,
      props: {
        modelValue: { type: [String, Number], default: '' },
        type: { type: String, default: 'text' },
      },
      emits: ['update:modelValue'],
      setup(props, { attrs, emit }) {
        return () => h('input', {
          ...attrs,
          type: props.type,
          value: props.modelValue,
          onInput: (event: Event) => {
            const value = (event.target as HTMLInputElement).value
            emit('update:modelValue', props.type === 'number' ? Number(value) : value)
          },
        })
      },
    }),
    Label: passthrough('LabelStub', 'label'),
    Select: passthrough('SelectStub'),
    SelectTrigger: passthrough('SelectTriggerStub'),
    SelectValue: passthrough('SelectValueStub'),
    SelectContent: passthrough('SelectContentStub'),
    SelectItem: passthrough('SelectItemStub'),
    Switch: passthrough('SwitchStub', 'button'),
  }
})

const mounted: Array<{ app: App, root: HTMLElement }> = []

async function settle() {
  for (let index = 0; index < 4; index += 1) {
    await Promise.resolve()
    await nextTick()
  }
}

function provider() {
  return {
    id: 'provider-kiro',
    name: 'kiro-pool',
    provider_type: 'kiro',
    description: '',
    website: '',
    provider_priority: 100,
    keep_priority_on_conversion: false,
    is_active: true,
    billing_type: 'pay_as_you_go',
    pool_advanced: null,
    kiro_simulated_cache_enabled: true,
    kiro_simulated_cache_target_percent: 97,
    kiro_simulated_cache_ttl_secs: 21_600,
    total_endpoints: 1,
    active_endpoints: 1,
    total_keys: 1,
    active_keys: 1,
    total_models: 1,
    active_models: 1,
    global_model_ids: [],
    avg_health_score: 1,
    unhealthy_endpoints: 0,
    api_formats: ['claude:messages'],
    endpoint_health_details: [],
    ops_configured: false,
    created_at: '2026-01-01T00:00:00Z',
    updated_at: '2026-01-01T00:00:00Z',
  }
}

async function mountDialog() {
  const root = document.createElement('div')
  document.body.appendChild(root)
  const app = createApp(ProviderFormDialog, {
    modelValue: true,
    provider: provider(),
  })
  app.mount(root)
  mounted.push({ app, root })
  await settle()
  return root
}

beforeEach(() => {
  apiMocks.createProvider.mockReset()
  apiMocks.updateProvider.mockReset()
  apiMocks.updateProvider.mockImplementation(async (_id, payload) => ({
    ...provider(),
    ...payload,
  }))
  toastMocks.success.mockReset()
  toastMocks.error.mockReset()
})

afterEach(() => {
  for (const item of mounted.splice(0)) {
    item.app.unmount()
    item.root.remove()
  }
})

describe('ProviderFormDialog Kiro stable cache settings', () => {
  it('hydrates and submits the target percentage and TTL', async () => {
    const root = await mountDialog()

    expect(root.textContent).toContain('稳态 Token 命中目标 (%)')
    expect(root.textContent).toContain('缓存有效期')
    const target = root.querySelector<HTMLInputElement>('#kiro-cache-target')
    expect(target?.value).toBe('97')

    const submit = [...root.querySelectorAll('button')]
      .find((button) => button.textContent?.trim() === '保存')
    expect(submit).toBeTruthy()
    submit?.click()
    await settle()

    expect(apiMocks.updateProvider).toHaveBeenCalledTimes(1)
    expect(apiMocks.updateProvider).toHaveBeenCalledWith(
      'provider-kiro',
      expect.objectContaining({
        config: {
          kiro: {
            simulated_cache_enabled: true,
            simulated_cache_target_percent: 97,
            simulated_cache_ttl_secs: 21_600,
          },
        },
      }),
    )
  })
})
