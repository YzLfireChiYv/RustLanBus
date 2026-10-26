import { useState } from 'react'
import { selectedOutputEndpointIds } from './memberModel.ts'
import { OutputTargets } from './OutputTargets.tsx'
import {
  actionErrorMessage,
  captureLabel,
  controlDomId,
  formatGainDb,
  formatPan,
  formatPcmFormat,
  sourceKindLabel,
} from './labels.ts'
import {
  DetailsBlock,
  EmptyState,
  FieldError,
  Icon,
  MeterReadout,
  SelectField,
  SliderField,
  SourcePlaybackReadout,
  ToggleField,
} from './primitives.tsx'
import type {
  EndpointView,
  LocalSourceView,
  OutputDeviceView,
  ParticipantAction,
  ParticipantViewState,
} from './types.ts'

async function runAction(
  onAction: (action: ParticipantAction) => Promise<void>,
  action: ParticipantAction,
  setMessage: (message: string | null) => void,
) {
  setMessage(null)
  try {
    await onAction(action)
  } catch (error) {
    setMessage(actionErrorMessage(error))
  }
}

function captureEndpoints(
  state: ParticipantViewState,
  source: LocalSourceView,
): readonly EndpointView[] {
  return state.endpoints.filter(
    (endpoint) => endpoint.direction === (source.kind === 'system' ? 'render' : 'capture'),
  )
}

function endpointOptionLabel(
  endpoint: EndpointView,
  outputLoopback: boolean,
): string {
  if (outputLoopback) return `${endpoint.name}（已选为输出，不可回环采集）`
  if (!endpoint.supported) return `${endpoint.name}（不支持）`
  if (!endpoint.available) return `${endpoint.name}（不可用）`
  return endpoint.name
}

function endpointDetail(endpoint: EndpointView, outputLoopback: boolean): string {
  const parts = [
    endpoint.name,
    endpoint.available ? '可用' : '不可用',
    endpoint.supported ? '支持' : '不支持',
  ]
  if (endpoint.reason !== null) parts.push(endpoint.reason)
  if (outputLoopback) {
    parts.push(
      '此设备也是当前播放设备，回环采集会再次发送收听到的声音。请选择独立的系统声音来源。',
    )
  }
  return parts.join('，')
}

function LocalSourceCard({
  captures,
  disabled,
  message,
  onAction,
  outputEndpointIds,
  outputDevices,
  renderEndpoints,
  blockedOutputIds,
  setMessage,
  source,
}: {
  captures: readonly EndpointView[]
  disabled: boolean
  message: string | null
  onAction: (action: ParticipantAction) => Promise<void>
  outputEndpointIds: readonly string[]
  outputDevices: readonly OutputDeviceView[]
  renderEndpoints: readonly EndpointView[]
  blockedOutputIds: readonly string[]
  setMessage: (message: string | null) => void
  source: LocalSourceView
}) {
  const endpointMissing =
    source.endpointId !== null &&
    !captures.some((endpoint) => endpoint.id === source.endpointId)
  const captureProblem = source.captureState === 'error' || source.captureState === 'unsupported'
    || (source.captureState === 'missing' && source.endpointId !== null)

  function isOutputLoopback(endpoint: EndpointView): boolean {
    return (
      source.kind === 'system' &&
      endpoint.direction === 'render' &&
      outputEndpointIds.includes(endpoint.id)
    )
  }

  return (
    <article className="source-card">
      <header className="source-card__head">
        <Icon name={source.kind === 'system' ? 'wave' : 'mic'} />
        <h3>{source.name}</h3>
        <span className="readout">
          {sourceKindLabel[source.kind]} · {captureLabel[source.captureState]}
        </span>
        {source.available || source.captureState === 'idle' ? null : <span className="readout">当前不可用</span>}
      </header>
      {source.format ? <p className="readout">{source.captureState === 'idle' ? '上次已知格式：' : ''}{formatPcmFormat(source.format)}</p> : null}
      {source.availabilityReason ? <p className={captureProblem ? 'field-error' : 'field-hint'}>{source.availabilityReason}</p> : null}
      <FieldError id={controlDomId('local-error', source.id)} message={message} />
      <ToggleField
        id={controlDomId('publish', source.id)}
        label="发布"
        checked={source.publish}
        disabled={disabled}
        onCommit={(publish) => {
          return runAction(
            onAction,
            {
              type: 'set_local_source',
              sourceId: source.id,
              publish,
              monitor: source.monitor,
            },
            setMessage,
          )
        }}
      />
      <div>
        <ToggleField
          id={controlDomId('monitor', source.id)}
          label="本机监听"
          checked={source.monitor}
          disabled={disabled}
          onCommit={(monitor) => {
            return runAction(
              onAction,
              {
                type: 'set_local_source',
                sourceId: source.id,
                publish: source.publish,
                monitor,
              },
              setMessage,
            )
          }}
        />
        <p className="panel__hint">
          打开后可以在本机听到这个来源。它和是否发布无关。
        </p>
        <p className={source.monitor && !source.monitorReady ? 'field-error' : 'readout'} role="status">
          {source.monitor ? source.monitorReady ? '本机监听路由已就绪' : '本机监听等待就绪' : '本机监听已关闭'}
          {source.monitorAvailabilityReason ? `：${source.monitorAvailabilityReason}` : ''}
        </p>
        {source.monitor ? <SourcePlaybackReadout outputs={outputDevices} endpoints={renderEndpoints}
          memberId={null} sourceId={source.id} /> : null}
      </div>
      <DetailsBlock summary="监听输出">
        <OutputTargets
          id={controlDomId('local-outputs', source.id)}
          targets={source.outputTargets}
          endpoints={renderEndpoints}
          blockedEndpointIds={blockedOutputIds}
          disabled={disabled}
          onCommit={(targets) => void runAction(onAction, {
            type: 'set_local_outputs', sourceId: source.id, targets,
          }, setMessage)}
        />
        <p className="field-hint">更改输出设备会保留当前发布和本机监听开关。</p>
      </DetailsBlock>
      <SelectField
        id={controlDomId('endpoint', source.id)}
        label="采集设备"
        value={source.endpointId ?? ''}
        disabled={disabled}
        onCommit={(endpointId) => {
          return runAction(
            onAction,
            {
              type: 'select_source_endpoint',
              sourceId: source.id,
              endpointId,
            },
            setMessage,
          )
        }}
      >
        <option value="">尚未选择</option>
        {endpointMissing && source.endpointId !== null ? (
          <option value={source.endpointId}>未知设备</option>
        ) : null}
        {captures.map((endpoint) => (
          <option
            key={endpoint.id}
            value={endpoint.id}
            disabled={!endpoint.supported || isOutputLoopback(endpoint)}
          >
            {endpointOptionLabel(endpoint, isOutputLoopback(endpoint))}
          </option>
        ))}
      </SelectField>
      <DetailsBlock summary="设备详情">
        {captures.length === 0 ? (
          <p>没有采集设备。</p>
        ) : (
          <ul>
            {captures.map((endpoint) => (
              <li key={endpoint.id}>
                {endpointDetail(endpoint, isOutputLoopback(endpoint))}
              </li>
            ))}
          </ul>
        )}
      </DetailsBlock>
      <div>
        <h4>本机监听</h4>
        <SliderField
          id={controlDomId('local-gain', source.id)}
          label="监听音量"
          value={source.gainDb}
          min={-60}
          max={12}
          step={0.5}
          format={formatGainDb}
          disabled={disabled}
          onCommit={(gainDb) => {
            return runAction(
              onAction,
              {
                type: 'set_source_mix',
                memberId: null,
                sourceId: source.id,
                gainDb,
                pan: source.pan,
                mute: source.mute,
                solo: source.solo,
              },
              setMessage,
            )
          }}
        />
        <SliderField
          id={controlDomId('local-pan', source.id)}
          label="声像"
          value={source.pan}
          min={-1}
          max={1}
          step={0.01}
          format={formatPan}
          disabled={disabled}
          onCommit={(pan) => {
            return runAction(
              onAction,
              {
                type: 'set_source_mix',
                memberId: null,
                sourceId: source.id,
                gainDb: source.gainDb,
                pan,
                mute: source.mute,
                solo: source.solo,
              },
              setMessage,
            )
          }}
        />
        <ToggleField
          id={controlDomId('local-mute', source.id)}
          label="静音"
          checked={source.mute}
          disabled={disabled}
          onCommit={(mute) => {
            return runAction(
              onAction,
              {
                type: 'set_source_mix',
                memberId: null,
                sourceId: source.id,
                gainDb: source.gainDb,
                pan: source.pan,
                mute,
                solo: source.solo,
              },
              setMessage,
            )
          }}
        />
        <ToggleField
          id={controlDomId('local-solo', source.id)}
          label="独奏"
          checked={source.solo}
          disabled={disabled}
          onCommit={(solo) => {
            return runAction(
              onAction,
              {
                type: 'set_source_mix',
                memberId: null,
                sourceId: source.id,
                gainDb: source.gainDb,
                pan: source.pan,
                mute: source.mute,
                solo,
              },
              setMessage,
            )
          }}
        />
      </div>
      <MeterReadout label={`${source.name}采集峰值`} peakDb={source.peakDb} />
    </article>
  )
}

export function LocalSources({
  state,
  disabled,
  onAction,
}: {
  state: ParticipantViewState
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const [errors, setErrors] = useState<Record<string, string | null>>({})

  return (
    <section className="panel" aria-labelledby="local-sources-heading">
      <h2 id="local-sources-heading" className="panel__title">
        本机来源
      </h2>
      <p className="panel__hint">
        系统声音和麦克风各自独立。发布保留原来的声音。音量、声像、静音和独奏只影响本机监听。监听默认关闭。
      </p>
      <div className="panel__body">
        {state.snapshotStatus === 'stale' ? <p className="field-error" role="status">下列来源为最后一次读取的状态，正在等待控制层恢复。</p> : null}
        {state.localSources.length === 0 ? (
          <EmptyState>引擎还没有提供本机来源。</EmptyState>
        ) : (
          state.localSources.map((source) => (
            <LocalSourceCard
              key={source.id}
              captures={captureEndpoints(state, source)}
              disabled={disabled}
              message={Object.hasOwn(errors, source.id) ? errors[source.id] : null}
              onAction={onAction}
              outputEndpointIds={selectedOutputEndpointIds(state)}
              outputDevices={state.outputDevices}
              renderEndpoints={state.endpoints}
              blockedOutputIds={state.localSources.flatMap((candidate) =>
                candidate.kind === 'system' && candidate.endpointId !== null ? [candidate.endpointId] : [])}
              setMessage={(message) => {
                setErrors((current) => ({ ...current, [source.id]: message }))
              }}
              source={source}
            />
          ))
        )}
      </div>
    </section>
  )
}
