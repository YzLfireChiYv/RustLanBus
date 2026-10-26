import { controlDomId } from './labels.ts'
import type { EndpointView, OutputTarget } from './types.ts'

function isSelected(targets: readonly OutputTarget[], id: string): boolean {
  return targets.some((target) => target !== 'main' && target.endpoint === id)
}

/** Edits the complete symbolic destination list; fixed/main aliases stay distinct. */
export function OutputTargets({
  id,
  targets,
  endpoints,
  blockedEndpointIds = [],
  disabled,
  onCommit,
}: {
  id: string
  targets: readonly OutputTarget[]
  endpoints: readonly EndpointView[]
  blockedEndpointIds?: readonly string[]
  disabled: boolean
  onCommit: (targets: readonly OutputTarget[]) => void
}) {
  const outputs = endpoints.filter((endpoint) => endpoint.direction === 'render')
  const missing = targets.flatMap((target) => target !== 'main' &&
    !outputs.some((endpoint) => endpoint.id === target.endpoint) ? [target.endpoint] : [])
  const setMain = (enabled: boolean) => onCommit(enabled
    ? targets.includes('main') ? targets : [...targets, 'main']
    : targets.filter((target) => target !== 'main'))
  const setEndpoint = (endpointId: string, enabled: boolean) => onCommit(enabled
    ? isSelected(targets, endpointId) ? targets : [...targets, { endpoint: endpointId }]
    : targets.filter((target) => target === 'main' || target.endpoint !== endpointId))

  return (
    <div>
      <label className="choice-row" htmlFor={controlDomId(id, 'main')}>
        <span>跟随主输出</span>
        <input
          id={controlDomId(id, 'main')}
          type="checkbox"
          checked={targets.includes('main')}
          disabled={disabled}
          onChange={(event) => setMain(event.currentTarget.checked)}
        />
      </label>
      {outputs.map((endpoint) => {
        const selected = isSelected(targets, endpoint.id)
        const blocked = blockedEndpointIds.includes(endpoint.id)
        const note = blocked ? '已用作系统声采集'
          : !endpoint.available ? '等待设备恢复' : !endpoint.supported ? '不支持' : null
        return (
          <label className="choice-row" key={endpoint.id} htmlFor={controlDomId(id, endpoint.id)}>
            <span>{endpoint.name}{note === null ? '' : `（${note}）`}</span>
            <input
              id={controlDomId(id, endpoint.id)}
              type="checkbox"
              checked={selected}
              disabled={disabled || (!selected && (!endpoint.supported || blocked))}
              onChange={(event) => setEndpoint(endpoint.id, event.currentTarget.checked)}
            />
          </label>
        )
      })}
      {missing.map((endpointId) => (
        <label className="choice-row" key={endpointId} htmlFor={controlDomId(id, endpointId)}>
          <span>{endpointId}（等待设备恢复）</span>
          <input
            id={controlDomId(id, endpointId)}
            type="checkbox"
            checked
            disabled={disabled}
            onChange={() => setEndpoint(endpointId, false)}
          />
        </label>
      ))}
      <p className="field-hint">
        可以同时选多个设备。全部取消后保留声音接收；本机监听也会停止向设备播放。
      </p>
    </div>
  )
}
