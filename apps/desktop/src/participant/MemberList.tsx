import { useState } from 'react'
import { DefaultRules } from './DefaultRules.tsx'
import { actionErrorMessage, contactLabel, controlDomId } from './labels.ts'
import { MemberDetail } from './MemberDetail.tsx'
import {
  deviceEditable,
  deviceEditAction,
  deviceSwitchState,
  directionRuleSource,
  groupEditable,
  groupMembers,
  groupSwitchState,
  incomingStatus,
  nameEditAction,
  outgoingStatus,
  type DirectionStatus,
  type MemberDevice,
  type MemberGroup,
  type RuleDirection,
} from './memberModel.ts'
import { EmptyState, FieldError, StatusDot, ToggleField } from './primitives.tsx'
import { RoomNotices } from './RoomNotices.tsx'
import { RoomRules } from './RoomRules.tsx'
import type {
  ContactState,
  LocalSourceView,
  OutputDeviceView,
  ParticipantAction,
  ParticipantViewState,
  RoomSessionView,
} from './types.ts'

const contactTone: Record<
  ContactState,
  'positive' | 'warning' | 'info' | 'neutral'
> = {
  connected: 'positive',
  reconnecting: 'warning',
  contacting: 'info',
  offline: 'neutral',
}

function toggled(list: readonly string[], name: string): readonly string[] {
  return list.includes(name) ? list.filter((item) => item !== name) : [...list, name]
}

function DirectionLine({ status }: { status: DirectionStatus }) {
  return (
    <p className={`direction-line direction-line--${status.tone}`}>
      <StatusDot tone={status.tone} />
      <span>{status.text}</span>
    </p>
  )
}

function DeviceFacts({
  device,
  localSources,
  session,
  stale = false,
  outputs,
}: {
  device: MemberDevice
  localSources: readonly LocalSourceView[]
  session: RoomSessionView | null
  stale?: boolean
  outputs: readonly OutputDeviceView[]
}) {
  const { member } = device
  const incoming: DirectionStatus = stale
    ? { tone: 'warning', text: '连接资料已过期，等待控制层恢复' }
    : incomingStatus(member, device.route, outputs)
  const outgoing = stale ? incoming : outgoingStatus(member, localSources, device.route)
  const listenSource = directionRuleSource(device.route, 'listen', session)
  const sendSource = directionRuleSource(device.route, 'send', session)
  return (
    <div className="member-facts">
      <p className="member-facts__address">
        <StatusDot tone={stale ? 'warning' : contactTone[member.contact]} pulse={!stale && member.contact === 'reconnecting'} />
        <span>{device.ip ?? '地址未知'}</span>
        <span>{stale ? `上次状态：${contactLabel[member.contact]}` : contactLabel[member.contact]}</span>
      </p>
      <DirectionLine status={incoming} />
      {outgoing.text === incoming.text ? null : <DirectionLine status={outgoing} />}
      {listenSource ? <p className="member-group__note">收听：{listenSource}</p> : null}
      {sendSource ? <p className="member-group__note">发送：{sendSource}</p> : null}
    </div>
  )
}

interface GroupRowProps {
  group: MemberGroup
  state: ParticipantViewState
  disabled: boolean
  split: boolean
  open: boolean
  error: string | null
  onEdit: (action: ParticipantAction) => void
  onToggleSplit: () => void
  onToggleOpen: () => void
  onAction: (action: ParticipantAction) => Promise<void>
}

function MemberGroupRow({
  group,
  state,
  disabled,
  split,
  open,
  error,
  onEdit,
  onToggleSplit,
  onToggleOpen,
  onAction,
}: GroupRowProps) {
  const joined = state.group.joined
  const count = group.devices.length
  const duplicate = count > 1
  const editable = groupEditable(group, joined)
  const listen = groupSwitchState(group, 'listen')
  const send = groupSwitchState(group, 'send')
  const errorId = controlDomId('member-error', group.name)
  const detailId = controlDomId('member-detail', group.name)
  const describedBy = error ? errorId : undefined

  function commitName(direction: RuleDirection, enabled: boolean) {
    if (disabled || !editable) return
    onEdit(nameEditAction(group.name, direction, enabled))
  }

  function commitDevice(device: MemberDevice, direction: RuleDirection, enabled: boolean) {
    if (disabled || !deviceEditable(device, joined) || device.ip === null) return
    onEdit(deviceEditAction(group.name, device.ip, direction, enabled))
  }

  return (
    <li className="member-group">
      <div className="member-group__top">
        <div className="member-group__identity">
          <p className="member-group__name">
            <strong>{group.name}</strong>
            {duplicate ? <span className="member-badge">同名 {count} 台</span> : null}
          </p>
          {duplicate ? (
            <p className="member-group__note">右侧开关按名称设置，这 {count} 台一起改变。</p>
          ) : (
            <DeviceFacts device={group.devices[0]} localSources={state.localSources} session={state.room.session} stale={state.snapshotStatus === 'stale'} outputs={state.outputDevices} />
          )}
          {editable ? null : (
            <p className="member-group__note">当前房间成员资料尚未就绪，暂时不能修改。</p>
          )}
        </div>
        <div className="member-group__switches">
          <ToggleField
            id={controlDomId('listen', group.name)}
            label="收听"
            emphasis="primary"
            checked={listen === 'on'}
            mixed={listen === 'mixed'}
            disabled={disabled || !editable}
            describedBy={describedBy}
            onCommit={(next) => commitName('listen', next)}
          />
          <ToggleField
            id={controlDomId('send', group.name)}
            label="发送给对方"
            emphasis="secondary"
            checked={send === 'on'}
            mixed={send === 'mixed'}
            disabled={disabled || !editable}
            describedBy={describedBy}
            onCommit={(next) => commitName('send', next)}
          />
        </div>
      </div>
      {duplicate ? (
        <ul className="member-devices">
          {group.devices.map((device) => {
            const deviceLocked = disabled || !deviceEditable(device, joined)
            return (
              <li className="member-device" key={device.member.id}>
                <DeviceFacts device={device} localSources={state.localSources} session={state.room.session} stale={state.snapshotStatus === 'stale'} outputs={state.outputDevices} />
                {split ? (
                  <div className="member-group__switches">
                    <ToggleField
                      id={controlDomId('device-listen', device.member.id)}
                      label="收听"
                      emphasis="secondary"
                      checked={deviceSwitchState(device, 'listen') === 'on'}
                      mixed={deviceSwitchState(device, 'listen') === 'mixed'}
                      disabled={deviceLocked}
                      describedBy={describedBy}
                      onCommit={(next) => commitDevice(device, 'listen', next)}
                    />
                    <ToggleField
                      id={controlDomId('device-send', device.member.id)}
                      label="发送给对方"
                      emphasis="secondary"
                      checked={deviceSwitchState(device, 'send') === 'on'}
                      mixed={deviceSwitchState(device, 'send') === 'mixed'}
                      disabled={deviceLocked}
                      describedBy={describedBy}
                      onCommit={(next) => commitDevice(device, 'send', next)}
                    />
                  </div>
                ) : null}
              </li>
            )
          })}
          {split ? (
            <li className="member-device member-device--note">
              <p className="member-group__note">
                这里的开关作用于名称和地址都匹配的设备；同名同地址会一起改变。之后再用上方按名称的开关，会让这 {count} 台重新一致。
              </p>
            </li>
          ) : null}
        </ul>
      ) : null}
      <div className="member-group__actions">
        {duplicate ? (
          <button
            type="button"
            className="button button--quiet"
            aria-expanded={split}
            disabled={!editable}
            onClick={onToggleSplit}
          >
            {split ? '收起按地址设置' : '按地址分别设置'}
          </button>
        ) : null}
        <button
          type="button"
          className="button button--quiet"
          aria-expanded={open}
          aria-controls={open ? detailId : undefined}
          onClick={onToggleOpen}
        >
          {open ? '收起详情' : '详情'}
        </button>
      </div>
      <FieldError id={errorId} message={error} />
      {open ? (
        <MemberDetail id={detailId} group={group} joined={joined} disabled={disabled} onAction={onAction}
          endpoints={state.endpoints}
          outputDevices={state.outputDevices}
          blockedOutputIds={state.localSources.filter((source) => source.kind === 'system' && source.endpointId !== null).map((source) => source.endpointId!)}
          separateByAddress={split} />
      ) : null}
    </li>
  )
}

interface MemberListProps {
  state: ParticipantViewState
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}

export function MemberList({ state, disabled, onAction }: MemberListProps) {
  const [errors, setErrors] = useState<Record<string, string | null>>({})
  const [split, setSplit] = useState<readonly string[]>([])
  const [open, setOpen] = useState<readonly string[]>([])
  const groups = groupMembers(state.members, state.room.routes)
  const session = state.room.session

  function edit(name: string, action: ParticipantAction) {
    const setMessage = (message: string | null) => {
      setErrors((current) => ({ ...current, [name]: message }))
    }
    setMessage(null)
    void onAction(action).catch((error: unknown) => {
      setMessage(actionErrorMessage(error))
    })
  }

  return (
    <section className="panel member-panel" aria-labelledby="members-heading">
      <header className="member-panel__head">
        <h2 id="members-heading" className="panel__title">
          成员
        </h2>
        {state.members.length > 0 ? (
          <span className="member-panel__count">{state.members.length} 台设备</span>
        ) : null}
        <span className="member-panel__spacer" />
        <DefaultRules
          session={session}
          globalRulesPath={state.room.globalRulesPath}
          disabled={disabled}
          onAction={onAction}
        />
      </header>
      <RoomNotices
        session={session}
        joined={state.group.joined}
        disabled={disabled}
        onAction={onAction}
      />
      <div className="panel__body">
        {groups.length === 0 ? (
          <EmptyState>还没有其他成员。</EmptyState>
        ) : (
          <ul className="member-groups">
            {groups.map((group) => (
              <MemberGroupRow
                key={group.name}
                group={group}
                state={state}
                disabled={disabled}
                split={split.includes(group.name)}
                open={open.includes(group.name)}
                error={Object.hasOwn(errors, group.name) ? errors[group.name] : null}
                onEdit={(action) => edit(group.name, action)}
                onToggleSplit={() => setSplit((current) => toggled(current, group.name))}
                onToggleOpen={() => setOpen((current) => toggled(current, group.name))}
                onAction={onAction}
              />
            ))}
          </ul>
        )}
        <RoomRules
          session={session}
          joined={state.group.joined}
          disabled={disabled}
          onAction={onAction}
        />
      </div>
    </section>
  )
}
