import { describe, expect, it } from 'vitest'
import {
  getProxyTargetName,
  hasProxyTarget,
  proxyPayloadFromTarget,
  proxyTargetValue,
} from '../proxyTarget'

describe('proxyTarget helpers', () => {
  it('round-trips a single proxy node target payload', () => {
    expect(proxyPayloadFromTarget('node-1')).toEqual({
      node_id: 'node-1',
      enabled: true,
    })
    expect(proxyTargetValue({ node_id: 'node-1', enabled: true })).toBe('node-1')
  })

  it('round-trips a proxy group target payload', () => {
    expect(proxyPayloadFromTarget('group:group-1')).toEqual({
      mode: 'group',
      group_id: 'group-1',
      enabled: true,
    })
    expect(proxyTargetValue({ mode: 'group', group_id: 'group-1', enabled: true })).toBe('group:group-1')
  })

  it('can respect disabled proxy payloads when computing a selectable target', () => {
    const proxy = { node_id: 'node-1', enabled: false }

    expect(hasProxyTarget(proxy)).toBe(true)
    expect(proxyTargetValue(proxy)).toBe('node-1')
    expect(hasProxyTarget(proxy, true)).toBe(false)
    expect(proxyTargetValue(proxy, { respectDisabled: true })).toBe('')
  })

  it('resolves display names for node and group targets', () => {
    const groups = [{ id: 'group-1', name: 'Primary Group' }]
    const nodes = [{ id: 'node-1', name: 'Tokyo Node' }]

    expect(getProxyTargetName({ group_id: 'group-1' }, groups, nodes)).toBe('组 · Primary Group')
    expect(getProxyTargetName({ node_id: 'node-1' }, groups, nodes)).toBe('Tokyo Node')
    expect(getProxyTargetName({ group_id: 'missing-group-id' }, groups, nodes)).toBe('组 · missing-...')
    expect(getProxyTargetName({ node_id: 'missing-node-id' }, groups, nodes)).toBe('missing-...')
  })
})
