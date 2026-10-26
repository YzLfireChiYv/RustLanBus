import { useState } from 'react'
import {
  actionErrorMessage,
  arrivalAgeText,
  controlDomId,
  formatGainDb,
  outputPhaseLabel,
  sourcePlayoutLabel,
} from './labels.ts'
import {
  DetailsBlock,
  FieldError,
  MeterReadout,
  SelectField,
  SliderField,
  ToggleField,
} from './primitives.tsx'
import type {
  EndpointView,
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

type MasterSlot = 'master' | 'output' | 'reconnect' | 'devices'

function playbackOptionLabel(endpoint: EndpointView): string {
  if (!endpoint.supported) return `${endpoint.name}（不支持）`
  if (!endpoint.available) return `${endpoint.name}（不可用）`
  return endpoint.name
}

function playbackDetail(endpoint: EndpointView): string {
  const parts = [
    endpoint.name,
    endpoint.available ? '可用' : '不可用',
    endpoint.supported ? '支持' : '不支持',
  ]
  if (endpoint.reason !== null) parts.push(endpoint.reason)
  return parts.join('，')
}

export function MasterStrip({
  state,
  disabled,
  onAction,
}: {
  state: ParticipantViewState
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const [errors, setErrors] = useState<Record<MasterSlot, string | null>>({
    master: null,
    output: null,
    reconnect: null,
    devices: null,
  })
  const renderEndpoints = state.endpoints.filter(
    (endpoint) => endpoint.direction === 'render',
  )
  const selectedOutputId = state.outputEndpointId
  const outputIsUnknown =
    selectedOutputId !== null &&
    !renderEndpoints.some((endpoint) => endpoint.id === selectedOutputId)
  const anyOverload = state.overloaded || state.outputDevices.some((output) => output.overloaded === true)
  const sourceName = (memberId: string | null, sourceId: string) => {
    if (memberId === null) return `本机 · ${state.localSources.find((source) => source.id === sourceId)?.name ?? sourceId}`
    const member = state.members.find((entry) => entry.id === memberId)
    return `${member?.name ?? memberId} · ${member?.sources.find((source) => source.id === sourceId)?.name ?? sourceId}`
  }

  const setSlot = (slot: MasterSlot) => (message: string | null) => {
    setErrors((current) => ({ ...current, [slot]: message }))
  }

  return (
    <section className="panel" aria-labelledby="master-heading">
      <h2 id="master-heading" className="panel__title">
        总输出
      </h2>
      <p className="panel__hint">
        总音量只改变本机听到的比例。限幅默认关闭。超过满刻度时显示过载。
      </p>
      <div className="panel__body">
        {state.snapshotStatus === 'stale' ? <p className="field-error" role="status">下列输出为最后一次读取的状态，正在等待控制层恢复。</p> : null}
        <div className="master-strip">
          <SliderField
            disabled={disabled}
            format={formatGainDb}
            id="master-gain"
            label="总音量"
            max={12}
            min={-60}
            onCommit={(gainDb) =>
              runAction(
                onAction,
                {
                  type: 'set_master',
                  gainDb,
                  limiterEnabled: state.limiterEnabled,
                },
                setSlot('master'),
              )
            }
            step={0.5}
            value={state.masterGainDb}
          />
          <ToggleField
            checked={state.limiterEnabled}
            describedBy={
              errors.master ? controlDomId('master', 'error') : undefined
            }
            disabled={disabled}
            id="master-limiter"
            label="限幅"
            onCommit={(next) =>
              runAction(
                onAction,
                {
                  type: 'set_master',
                  gainDb: state.masterGainDb,
                  limiterEnabled: next,
                },
                setSlot('master'),
              )
            }
          />
          <p className="panel__hint">
            关闭时保留相对音量。过载只提示，不自动压低。
          </p>
          <FieldError
            id={controlDomId('master', 'error')}
            message={errors.master}
          />
          {anyOverload ? (
            <p className="overload-banner" role="status">
              有输出发生过载。相对音量仍按当前总音量保留，请查看各输出的电平。
            </p>
          ) : null}
          <MeterReadout
            label="代表输出峰值"
            overloaded={state.outputPeakDb !== null && state.outputPeakDb > 0}
            peakDb={state.outputPeakDb}
          />
          {state.outputDevices.length > 1 ? <p className="field-hint">此峰值来自一个代表输出，各设备的运行状态分别列在下方。</p> : null}
          <SelectField
            describedBy={
              errors.output ? controlDomId('output', 'error') : undefined
            }
            disabled={disabled}
            id="output-endpoint"
            label="主输出设备"
            onCommit={(endpointId) =>
              runAction(
                onAction,
                { type: 'select_output', endpointId },
                setSlot('output'),
              )
            }
            value={state.outputEndpointId ?? ''}
          >
            <option value="">系统默认播放设备</option>
            {outputIsUnknown && selectedOutputId !== null ? (
              <option value={selectedOutputId}>未知设备</option>
            ) : null}
            {renderEndpoints.map((endpoint) => (
              <option
                disabled={!endpoint.supported || state.localSources.some((source) =>
                  source.kind === 'system' && source.endpointId === endpoint.id)}
                key={endpoint.id}
                value={endpoint.id}
              >
                {playbackOptionLabel(endpoint)}
              </option>
            ))}
          </SelectField>
          <p className="field-hint">新接听的来源默认跟随主输出。更改这里会移动选择“跟随主输出”的来源；明确指定的设备保持原选择。</p>
          <FieldError
            id={controlDomId('output', 'error')}
            message={errors.output}
          />
          <DetailsBlock summary="播放设备详情">
            {renderEndpoints.length === 0 ? (
              <p>没有播放设备。</p>
            ) : (
              <ul>
                {renderEndpoints.map((endpoint) => (
                  <li key={endpoint.id}>{playbackDetail(endpoint)}</li>
                ))}
              </ul>
            )}
          </DetailsBlock>
          {state.outputDevices.length > 0 ? (
            <DetailsBlock summary="各输出的实际运行状态">
              <ul>
                {state.outputDevices.map((output) => {
                  const name = renderEndpoints.find((endpoint) => endpoint.id === output.endpointId)?.name ?? output.endpointId
                  return (
                    <li key={output.endpointId}>
                      <strong>{name}</strong>{state.mainOutputId === output.endpointId ? '（主输出）' : ''}：{outputPhaseLabel(output.phase)}
                      {output.rateHz !== null && output.channels !== null ? ` · ${output.rateHz / 1000} kHz · ${output.channels} 声道` : ''}
                      {output.lastError ? <p className="field-error">{output.lastError}</p> : null}
                      {output.overloaded ? <p className="overload-banner" role="status">此输出发生过载。</p> : null}
                      <MeterReadout label={`${name}输出峰值`} peakDb={output.peakDb ?? null} overloaded={output.overloaded ?? false} />
                      {(output.sources?.length ?? 0) > 0 ? (
                        <ul>
                          {output.sources!.map((source) => (
                            <li key={`${source.memberId ?? 'local'}:${source.sourceId}`}>
                              {sourceName(source.memberId, source.sourceId)}：{sourcePlayoutLabel(source.phase)}
                              <p className="readout">{arrivalAgeText(source.lastArrivalAgeMs)}</p>
                            </li>
                          ))}
                        </ul>
                      ) : null}
                      {output.sourceErrors?.map((error) => (
                        <p className="field-error" key={`${error.memberId ?? 'local'}:${error.sourceId}`}>
                          {sourceName(error.memberId, error.sourceId)}：{error.detail}
                        </p>
                      ))}
                    </li>
                  )
                })}
              </ul>
            </DetailsBlock>
          ) : null}
          <ToggleField
            checked={state.autoReconnect}
            describedBy={
              errors.reconnect ? controlDomId('reconnect', 'error') : undefined
            }
            disabled={disabled}
            id="auto-reconnect"
            label="房间内断线后自动恢复直连"
            onCommit={(enabled) =>
              runAction(
                onAction,
                { type: 'set_auto_reconnect', enabled },
                setSlot('reconnect'),
              )
            }
          />
          <p className="panel__hint">
            此项控制已认识成员的对等连接恢复。重新启动后是否进入房间由房间的已保存入房选择决定；登录 Windows 后启动程序在“Windows 启动设置”中单独控制。
          </p>
          <FieldError
            id={controlDomId('reconnect', 'error')}
            message={errors.reconnect}
          />
          <button
            aria-describedby={
              errors.devices ? controlDomId('devices', 'error') : undefined
            }
            className="button button--secondary"
            disabled={disabled}
            onClick={() => {
              void runAction(
                onAction,
                { type: 'refresh_devices' },
                setSlot('devices'),
              )
            }}
            type="button"
          >
            刷新设备
          </button>
          <FieldError
            id={controlDomId('devices', 'error')}
            message={errors.devices}
          />
        </div>
      </div>
    </section>
  )
}
