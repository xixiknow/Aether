import { describe, expect, it } from 'vitest'
import {
  buildPoolKeyBatchUpdatePatch,
  parsePoolKeyModelPatterns,
  type PoolKeyBatchEditState,
} from '../poolKeyBatchEdit'

function state(overrides: Partial<PoolKeyBatchEditState> = {}): PoolKeyBatchEditState {
  return {
    applyApiFormats: false,
    apiFormats: [],
    applyActive: false,
    isActive: true,
    applyInternalPriority: false,
    internalPriority: '0',
    applyRpmLimit: false,
    rpmLimit: '',
    applyConcurrentLimit: false,
    concurrentLimit: '',
    applyCacheTtl: false,
    cacheTtlMinutes: '5',
    applyProbeInterval: false,
    maxProbeIntervalMinutes: '32',
    applyNote: false,
    note: '',
    applyModels: false,
    modelMode: '',
    unrestrictedModels: true,
    selectedModels: [],
    includePatterns: '',
    excludePatterns: '',
    ...overrides,
  }
}

describe('buildPoolKeyBatchUpdatePatch', () => {
  it('only emits fields explicitly enabled by the operator', () => {
    const result = buildPoolKeyBatchUpdatePatch(state({
      applyApiFormats: true,
      apiFormats: ['openai:responses', 'openai:responses', ' openai:chat '],
      applyRpmLimit: true,
      rpmLimit: '',
    }))

    expect(result.error).toBeNull()
    expect(result.patch).toEqual({
      api_formats: ['openai:responses', 'openai:chat'],
      rpm_limit: null,
    })
  })

  it('builds a manual model policy and preserves explicit restrictions while disabling discovery', () => {
    const result = buildPoolKeyBatchUpdatePatch(state({
      applyModels: true,
      modelMode: 'manual',
      unrestrictedModels: false,
      selectedModels: ['gpt-5.6-sol', 'gpt-5.6-sol', 'gpt-5.6-luna'],
    }))

    expect(result.patch).toEqual({
      auto_fetch_models: false,
      allowed_models: ['gpt-5.6-sol', 'gpt-5.6-luna'],
      locked_models: [],
      model_include_patterns: [],
      model_exclude_patterns: [],
    })
  })

  it('builds automatic discovery filters and locked models', () => {
    const result = buildPoolKeyBatchUpdatePatch(state({
      applyModels: true,
      modelMode: 'automatic',
      selectedModels: ['gpt-5.6-sol'],
      includePatterns: 'gpt-*,\nclaude-*',
      excludePatterns: '*-preview, *-beta',
    }))

    expect(result.patch).toEqual({
      auto_fetch_models: true,
      locked_models: ['gpt-5.6-sol'],
      model_include_patterns: ['gpt-*', 'claude-*'],
      model_exclude_patterns: ['*-preview', '*-beta'],
    })
  })

  it('rejects empty fields and invalid ranges before the request is sent', () => {
    expect(buildPoolKeyBatchUpdatePatch(state()).error).toBe('请至少启用一个批量编辑字段')
    expect(buildPoolKeyBatchUpdatePatch(state({
      applyApiFormats: true,
    })).error).toBe('请至少选择一个支持的 API')
    expect(buildPoolKeyBatchUpdatePatch(state({
      applyCacheTtl: true,
      cacheTtlMinutes: '61',
    })).error).toBe('缓存 TTL 必须是 0-60 的整数')
    expect(buildPoolKeyBatchUpdatePatch(state({
      applyModels: true,
      modelMode: 'manual',
      unrestrictedModels: false,
    })).error).toBe('请至少选择一个允许的模型')
  })
})

describe('parsePoolKeyModelPatterns', () => {
  it('normalizes comma and line separated patterns', () => {
    expect(parsePoolKeyModelPatterns(' gpt-* , claude-*\ngpt-* ')).toEqual([
      'gpt-*',
      'claude-*',
    ])
  })
})
