import { useState } from 'react'
import { setRoomSourceMixAction } from './bridge.ts'
import { actionErrorMessage, controlDomId, formatGainDb, formatPan, formatPcmFormat } from './labels.ts'
import { groupEditable, switchState, type MemberGroup } from './memberModel.ts'
import { OutputTargets } from './OutputTargets.tsx'
import { DetailsBlock, FieldError, MeterReadout, SliderField, SourcePlaybackReadout, ToggleField } from './primitives.tsx'
import type {
  CoreMixPatch,
  ParticipantAction,
  EndpointView,
  OutputTarget,
  OutputDeviceView,
  PcmFormatView,
  PublicationState,
  RemoteSourceView,
  SourceKind,
  RuleMatchInput,
} from './types.ts'

const publicationLabel: Record<PublicationState, string> = {
  disabled: '未发布',
  unavailable: '已发布，暂不可用',
  active: '已发布',
}


interface SourceSlot {
  kind: SourceKind
  label: string
  entries: { memberId: string; ip: string | null; sending: boolean; listening: boolean; source: RemoteSourceView }[]
}

function sourceSlots(group: MemberGroup): SourceSlot[] {
  const slots: SourceSlot[] = []
  for (const device of group.devices) {
    for (const source of device.member.sources) {
      let slot = slots.find((candidate) => candidate.kind === source.kind)
      if (slot === undefined) {
        slot = { kind: source.kind, label: source.name, entries: [] }
        slots.push(slot)
      }
      slot.entries.push({
        memberId: device.member.id,
        ip: device.ip,
        sending: device.route?.sources.find((choice) => choice.kind === source.kind)?.send ?? false,
        listening: source.receiveAllowed,
        source,
      })
    }
  }
  return slots
}

function formatText(format: PcmFormatView | null): string {
  return format === null ? '' : ` · ${formatPcmFormat(format)}`
}

function sourceFact(source: RemoteSourceView, listening: boolean): string {
  const format = source.publication === 'disabled' ? '' : formatText(source.format)
  const playback = !listening ? '本机未选择接收'
    : source.mute ? '本机播放已静音'
      : source.available ? '本机输出路由已就绪' : '本机播放尚未就绪'
  return `${publicationLabel[source.publication]}${format} · ${playback}${source.solo ? ' · 已选择独奏' : ''}`
}

function targetsText(targets: readonly OutputTarget[], endpoints: readonly EndpointView[]): string {
  if (targets.length === 0) return '不送往输出设备'
  return targets.map((target) => target === 'main' ? '跟随主输出'
    : endpoints.find((endpoint) => endpoint.id === target.endpoint)?.name
      ?? `${target.endpoint}（等待设备恢复）`).join('、')
}

function sameTargets(left: readonly OutputTarget[], right: readonly OutputTarget[]): boolean {
  const key = (target: OutputTarget) => target === 'main' ? 'main' : `endpoint:${target.endpoint}`
  return left.length === right.length && left.every((target) => right.some((other) => key(target) === key(other)))
}

function SourceRoutingControls({
  scopeId, kind, entries, condition, endpoints, blockedOutputIds, disabled, onAction,
}: {
  scopeId: string
  kind: SourceKind
  entries: SourceSlot['entries']
  condition: RuleMatchInput
  endpoints: readonly EndpointView[]
  blockedOutputIds: readonly string[]
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const [error, setError] = useState<string | null>(null)
  const listen = switchState(entries.map((entry) => entry.listening))
  const send = switchState(entries.map((entry) => entry.sending))
  const targets = entries[0].source.outputTargets
  const differentOutputs = entries.some((entry) => !sameTargets(targets, entry.source.outputTargets))
  const errorId = controlDomId('source-routing-error', scopeId, kind)
  async function commit(fields: { send?: boolean; listen?: boolean; outputs?: readonly OutputTarget[] }) {
    if (disabled) return
    setError(null)
    try {
      await onAction({ type: 'edit_room_rule', rule: {
        match: condition, source_kind: kind, ...fields,
      } })
    } catch (cause) { setError(actionErrorMessage(cause)) }
  }
  return (
    <div>
      <ToggleField
        id={controlDomId('source-listen', scopeId, kind)}
        label="收听这个来源"
        checked={listen === 'on'}
        mixed={listen === 'mixed'}
        disabled={disabled}
        describedBy={error ? errorId : undefined}
        onCommit={(value) => void commit({ listen: value })}
      />
      <ToggleField
        id={controlDomId('source-send', scopeId, kind)}
        label="向对方发送本机同类来源"
        checked={send === 'on'}
        mixed={send === 'mixed'}
        disabled={disabled}
        describedBy={error ? errorId : undefined}
        onCommit={(value) => void commit({ send: value })}
      />
      <DetailsBlock summary="这个来源的输出设备">
        {differentOutputs ? (
          <>
            <ul>{entries.map((entry) => (
              <li key={`${entry.memberId}:${entry.source.id}`}>
                {entry.ip ?? '地址未知'}：{targetsText(entry.source.outputTargets, endpoints)}
              </li>
            ))}</ul>
            <p className="field-hint">各设备的输出不同。下面列出首台设备的选择；修改时会把这份完整选择统一应用到同名设备的同类来源。</p>
          </>
        ) : null}
        <OutputTargets
          id={controlDomId('remote-outputs', scopeId, kind)}
          targets={targets}
          endpoints={endpoints}
          blockedEndpointIds={blockedOutputIds}
          disabled={disabled}
          onCommit={(outputs) => void commit({ outputs })}
        />
      </DetailsBlock>
      <FieldError id={errorId} message={error} />
    </div>
  )
}

function SourceMixControls({
  scopeId, kind, entries, condition, disabled, onAction,
}: {
  scopeId: string
  kind: SourceKind
  entries: SourceSlot['entries']
  condition: RuleMatchInput
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const [error, setError] = useState<string | null>(null)
  const first = entries[0].source
  const differentGain = entries.some((entry) => entry.source.gainDb !== first.gainDb)
  const differentPan = entries.some((entry) => entry.source.pan !== first.pan)
  const mute = switchState(entries.map((entry) => entry.source.mute))
  const solo = switchState(entries.map((entry) => entry.source.solo))
  const errorId = controlDomId('mix-error', scopeId, kind)

  async function commit(patch: CoreMixPatch) {
    if (disabled) return
    setError(null)
    try {
      await onAction(setRoomSourceMixAction(condition, kind, patch))
    } catch (cause) {
      setError(actionErrorMessage(cause))
    }
  }

  return (
    <>
      <SliderField
        id={controlDomId('mix-gain', scopeId, kind)}
        label="音量"
        value={differentGain ? 0 : first.gainDb}
        mixed={differentGain}
        min={-60}
        max={12}
        step={0.5}
        format={formatGainDb}
        disabled={disabled}
        onCommit={(gain_db) => void commit({ gain_db })}
      />
      <ToggleField
        id={controlDomId('mix-mute', scopeId, kind)}
        label="静音"
        checked={mute === 'on'}
        mixed={mute === 'mixed'}
        disabled={disabled}
        describedBy={error ? errorId : undefined}
        onCommit={(value) => void commit({ mute: value })}
      />
      <DetailsBlock summary="更多调整">
        <SliderField
          id={controlDomId('mix-pan', scopeId, kind)}
          label="声像"
          value={differentPan ? 0 : first.pan}
          mixed={differentPan}
          min={-1}
          max={1}
          step={0.01}
          format={formatPan}
          disabled={disabled}
          onCommit={(pan) => void commit({ pan })}
        />
        <ToggleField
          id={controlDomId('mix-solo', scopeId, kind)}
          label="独奏"
          checked={solo === 'on'}
          mixed={solo === 'mixed'}
          disabled={disabled}
          onCommit={(value) => void commit({ solo: value })}
        />
      </DetailsBlock>
      <FieldError id={errorId} message={error} />
    </>
  )
}

export function MemberDetail({
  id,
  group,
  joined,
  disabled,
  onAction,
  endpoints = [],
  blockedOutputIds = [],
  outputDevices = [],
  separateByAddress = false,
}: {
  id: string
  group: MemberGroup
  joined: boolean
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
  endpoints?: readonly EndpointView[]
  blockedOutputIds?: readonly string[]
  outputDevices?: readonly OutputDeviceView[]
  separateByAddress?: boolean
}) {
  const slots = sourceSlots(group)
  const duplicate = group.devices.length > 1
  const editable = groupEditable(group, joined)

  return (
    <div className="member-detail" id={id}>
      <h3 className="member-detail__title">声音来源与本机播放</h3>
      {slots.length === 0 ? (
        <p className="field-hint">对方还没有提供声音来源信息。</p>
      ) : (
        slots.map((slot) => {
          const { source } = slot.entries[0]
          const different = slot.entries.some(
            (entry) => entry.source.gainDb !== source.gainDb || entry.source.pan !== source.pan ||
              entry.source.mute !== source.mute || entry.source.solo !== source.solo,
          )
          const locked = disabled || !editable
          return (
            <article className="member-source" key={slot.kind}>
              <div className="member-source__head">
                <h4>{slot.label}</h4>
                <ul className="member-source__facts">
                  {slot.entries.map((entry) => (
                    <li key={`${entry.memberId}:${entry.source.id}`}>
                      {duplicate ? `${entry.ip ?? '地址未知'}：` : ''}
                      {sourceFact(entry.source, entry.listening)}
                      {entry.source.availabilityReason ? (
                        <p className={entry.listening && !entry.source.available ? 'field-error' : 'field-hint'}>
                          {entry.source.availabilityReason}
                        </p>
                      ) : null}
                      <MeterReadout
                        label={`${duplicate ? `${entry.ip ?? '地址未知'} ` : ''}${slot.label}接收峰值（播放前）`}
                        peakDb={entry.source.peakDb}
                      />
                      <SourcePlaybackReadout outputs={outputDevices} endpoints={endpoints}
                        memberId={entry.memberId} sourceId={entry.source.id} />
                      {different ? (
                        <p className="field-hint">
                          音量 {formatGainDb(entry.source.gainDb)}，声像 {formatPan(entry.source.pan)}，
                          静音{entry.source.mute ? '开启' : '关闭'}，独奏{entry.source.solo ? '开启' : '关闭'}
                        </p>
                      ) : null}
                    </li>
                  ))}
                </ul>
              </div>
              <SourceRoutingControls
                scopeId={group.name}
                kind={slot.kind}
                entries={slot.entries}
                condition={{ name: group.name }}
                endpoints={endpoints}
                blockedOutputIds={blockedOutputIds}
                disabled={disabled || !editable}
                onAction={onAction}
              />
              {duplicate && separateByAddress ? (
                <DetailsBlock summary="按地址设置这个来源">
                  {slot.entries.map((entry) => (
                    <div key={`${entry.memberId}:${entry.source.id}`}>
                      <h4>{entry.ip ?? '地址未知'}</h4>
                      {entry.ip === null ? (
                        <p className="field-hint">地址尚未确认，暂时不能按地址保存设置。</p>
                      ) : (
                        <>
                          <SourceRoutingControls
                            scopeId={entry.memberId}
                            kind={slot.kind}
                            entries={[entry]}
                            condition={{ name: group.name, ip: entry.ip }}
                            endpoints={endpoints}
                            blockedOutputIds={blockedOutputIds}
                            disabled={locked}
                            onAction={onAction}
                          />
                          <SourceMixControls
                            scopeId={entry.memberId}
                            kind={slot.kind}
                            entries={[entry]}
                            condition={{ name: group.name, ip: entry.ip }}
                            disabled={locked}
                            onAction={onAction}
                          />
                        </>
                      )}
                    </div>
                  ))}
                </DetailsBlock>
              ) : null}
              {different ? (
                <p className="field-hint">
                  各设备的播放设置不同；修改某一项会统一该项，其余设置各自保留。
                </p>
              ) : null}
              <SourceMixControls
                scopeId={group.name}
                kind={slot.kind}
                entries={slot.entries}
                condition={{ name: group.name }}
                disabled={locked}
                onAction={onAction}
              />
            </article>
          )
        })
      )}
      {slots.length > 0 ? (
        <p className="field-hint">来源开关、播放设置和输出按名称与声音来源类型保存；按地址展开后的操作同时匹配名称和 IP。发送开关不改变本机来源的发布状态。</p>
      ) : null}
    </div>
  )
}
