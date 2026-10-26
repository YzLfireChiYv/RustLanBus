import { editMemberDirectionAction } from './bridge.ts'
import type {
  CoreGlobalRules,
  LocalSourceView,
  MemberView,
  OutputDeviceView,
  ParticipantAction,
  ParticipantViewState,
  PeerRouteView,
  ProjectedEndpointView,
  RoomRuleInput,
  RoomSessionView,
  RuleFileStatusView,
  RuleMatchInput,
} from './types.ts'

export type RuleDirection = 'send' | 'listen'
export type SwitchState = 'on' | 'off' | 'mixed'
export type StatusTone = 'positive' | 'warning' | 'info' | 'neutral'

export interface DirectionStatus {
  tone: StatusTone
  text: string
}

export interface MemberDevice {
  member: MemberView
  route: PeerRouteView | null
  ip: string | null
}

export interface MemberGroup {
  name: string
  devices: readonly MemberDevice[]
}

export interface DefaultRuleView {
  send: boolean
  listen: boolean
  additional: number
}

/** The address text the core matches: the endpoint IP without port or IPv6 zone. */
export function endpointIp(endpoint: ProjectedEndpointView): string | null {
  if (endpoint.kind === 'local_scope_required') return endpoint.ip
  const address = endpoint.address
  if (address.startsWith('[')) {
    const close = address.indexOf(']')
    if (close < 0) return null
    const inner = address.slice(1, close)
    const zone = inner.indexOf('%')
    return zone < 0 ? inner : inner.slice(0, zone)
  }
  const colon = address.lastIndexOf(':')
  return colon > 0 ? address.slice(0, colon) : null
}

/** Rows follow the exact current room name, in first-appearance order. */
export function groupMembers(
  members: readonly MemberView[],
  routes: readonly PeerRouteView[],
): MemberGroup[] {
  const groups: { name: string; devices: MemberDevice[] }[] = []
  for (const member of members) {
    const route = routes.find((candidate) => candidate.memberId === member.id) ?? null
    const name = route?.name ?? member.name
    const device: MemberDevice = {
      member,
      route,
      ip: route === null ? null : endpointIp(route.endpoint),
    }
    const group = groups.find((candidate) => candidate.name === name)
    if (group) group.devices.push(device)
    else groups.push({ name, devices: [device] })
  }
  return groups
}

export function switchState(values: readonly boolean[]): SwitchState {
  if (values.length > 0 && values.every(Boolean)) return 'on'
  return values.some(Boolean) ? 'mixed' : 'off'
}

function directionValues(device: MemberDevice, direction: RuleDirection): readonly boolean[] {
  if (device.route !== null && device.route.sources.length > 0) {
    return device.route.sources.map((source) => source[direction])
  }
  return [direction === 'send' ? device.member.sendAllowed : device.member.receiveAllowed]
}

export function deviceSwitchState(device: MemberDevice, direction: RuleDirection): SwitchState {
  return switchState(directionValues(device, direction))
}

export function groupSwitchState(group: MemberGroup, direction: RuleDirection): SwitchState {
  return switchState(group.devices.flatMap((device) => directionValues(device, direction)))
}

export function groupEditable(group: MemberGroup, joined: boolean): boolean {
  return joined && group.devices.every((device) => device.route !== null)
}

export function deviceEditable(device: MemberDevice, joined: boolean): boolean {
  return joined && device.route !== null && device.ip !== null
}

export function nameEditAction(
  name: string,
  direction: RuleDirection,
  enabled: boolean,
): ParticipantAction {
  return editMemberDirectionAction({ name }, direction, enabled)
}

export function deviceEditAction(
  name: string,
  ip: string,
  direction: RuleDirection,
  enabled: boolean,
): ParticipantAction {
  return editMemberDirectionAction({ ip, name }, direction, enabled)
}

function joinNames(names: readonly string[]): string {
  return names.join('、')
}

export function incomingStatus(
  member: MemberView,
  route: PeerRouteView | null = null,
  outputs: readonly OutputDeviceView[] = [],
): DirectionStatus {
  const chosen = deviceSwitchState({ member, route, ip: null }, 'listen')
  if (chosen === 'off') return { tone: 'neutral', text: '未收听' }
  if (member.contact !== 'connected') return { tone: 'info', text: '等待接通' }
  const published = member.sources.filter((source) => source.receiveAllowed && source.publication !== 'disabled')
  if (published.length === 0 && !member.receiveReady) {
    return { tone: 'warning', text: '对方暂未向本机发送' }
  }
  if (published.length === 0) return { tone: 'warning', text: '对方没有发布声音' }
  if (published.every((source) => source.publication === 'unavailable')) {
    return { tone: 'warning', text: '对方已发布声音，来源暂不可用' }
  }
  const ready = published.filter((source) => source.receiveReady)
  if (ready.length === 0) return { tone: 'warning', text: '对方暂未向本机发送' }
  const observations = ready.flatMap((source) => outputs.flatMap((output) =>
    (output.sources ?? []).filter((value) => value.memberId === member.id && value.sourceId === source.id)
      .map((value) => ({ source, value }))))
  if (observations.length > 0) {
    const active = observations.filter(({ value }) => value.phase === 'running' && value.effectiveGain > 0)
    if (active.length > 0) {
      const names = [...new Set(active.map(({ source }) => source.name))]
      const pending = observations.some(({ value }) => value.phase !== 'running' && value.phase !== 'muted')
      return { tone: pending ? 'warning' : 'positive', text: `本机输出已接入：${joinNames(names)}${pending ? '（部分输出仍待处理）' : ''}` }
    }
    if (observations.every(({ value }) => value.phase === 'muted' || (value.phase === 'running' && value.effectiveGain === 0))) {
      return { tone: 'neutral', text: '接收路由已就绪，本机混音已静音' }
    }
    if (observations.some(({ value }) => value.phase === 'priming')) {
      return { tone: 'info', text: '正在准备本机播放' }
    }
    if (observations.some(({ value }) => value.phase === 'awaiting_current')) {
      return { tone: 'warning', text: '已接通，等待当前音频' }
    }
    if (observations.every(({ value }) => value.phase === 'unsupported')) {
      return { tone: 'warning', text: '所选输出不支持对方的声音格式' }
    }
    return { tone: 'warning', text: '接收方向已接通，等待输出设备' }
  }
  const playing = ready.filter((source) => source.available)
  if (playing.length === 0) {
    if (ready.every((source) => source.outputTargets.length === 0)) {
      return { tone: 'warning', text: '接收方向已接通，尚未选择输出设备' }
    }
    return { tone: 'warning', text: '对方的声音来源已就绪，尚未接入本机播放' }
  }
  const muted = playing.filter((source) => source.mute)
  if (muted.length === playing.length) {
    return { tone: 'neutral', text: `接收路由已就绪，本机已静音：${joinNames(muted.map((source) => source.name))}` }
  }
  const audibleChoices = playing.filter((source) => !source.mute)
  const notes = [
    ...(muted.length > 0 ? ['部分来源已静音'] : []),
    ...(playing.some((source) => source.solo) ? ['已选择独奏'] : []),
  ]
  return {
    tone: 'positive',
    text: `接收路由已就绪：${joinNames(audibleChoices.map((source) => source.name))}${notes.length ? `（${notes.join('，')}）` : ''}`,
  }
}

export function outgoingStatus(
  member: MemberView,
  localSources: readonly LocalSourceView[],
  route: PeerRouteView | null = null,
): DirectionStatus {
  if (deviceSwitchState({ member, route, ip: null }, 'send') === 'off') {
    return { tone: 'neutral', text: '不向对方发送' }
  }
  if (member.contact !== 'connected') return { tone: 'info', text: '等待接通' }
  if (!member.sendReady) return { tone: 'info', text: '发送尚未就绪，等待对方接收或确认' }
  const permitted = localSources.filter((source) =>
    route === null || route.sources.length === 0
      || route.sources.some((choice) => choice.kind === source.kind && choice.send))
  const active = permitted.filter((source) => source.publication === 'active'
    && member.sendingSourceIds.includes(source.id))
  if (active.length > 0) {
    return {
      tone: 'positive',
      text: `已向对方开放：${joinNames(active.map((source) => source.name))}`,
    }
  }
  if (permitted.some((source) => source.publication === 'active')) {
    return { tone: 'info', text: '发送尚未就绪，等待对方接收或确认' }
  }
  if (permitted.some((source) => source.publish)) {
    return { tone: 'warning', text: '本机声音尚未就绪' }
  }
  return { tone: 'warning', text: '本机没有发布声音' }
}

/** Global rule 1 is the new-member default while it matches all members and sources. */
export function defaultRuleView(rules: CoreGlobalRules | null): DefaultRuleView | null {
  const first = rules?.rules[0]
  if (rules == null || first === undefined || first.match !== 'all' || first.source_kind !== undefined) return null
  return {
    send: first.send ?? true,
    listen: first.listen ?? false,
    additional: rules.rules.length - 1,
  }
}

/** The core patches rule 1 from its current state, preserving later rules. */
export function defaultRuleAction(
  rules: CoreGlobalRules,
  direction: RuleDirection,
  enabled: boolean,
): ParticipantAction | null {
  const first = rules.rules[0]
  if (first === undefined || first.match !== 'all' || first.source_kind !== undefined) return null
  return direction === 'send'
    ? { type: 'set_global_defaults', send: enabled }
    : { type: 'set_global_defaults', listen: enabled }
}

export function matchText(match: RuleMatchInput): string {
  if (match === 'all') return '全部成员'
  if (match.ip !== undefined && match.name !== undefined) {
    return `名称「${match.name}」且地址 ${match.ip}`
  }
  if (match.name !== undefined) return `名称「${match.name}」`
  return `地址 ${match.ip ?? ''}`
}

export function assignmentText(rule: RoomRuleInput): string {
  const parts: string[] = []
  if (rule.send !== undefined) parts.push(rule.send ? '发送给对方' : '不发送给对方')
  if (rule.listen !== undefined) parts.push(rule.listen ? '收听' : '不收听')
  if (rule.outputs !== undefined) {
    parts.push(rule.outputs.length === 0 ? '不送往输出设备'
      : `输出：${rule.outputs.map((target) => target === 'main' ? '跟随主输出' : target.endpoint).join('、')}`)
  }
  const scope = rule.source_kind === 'system' ? '系统声音：' : rule.source_kind === 'microphone' ? '麦克风：' : ''
  return scope + parts.join('，')
}

/** Selected destinations also count while disconnected: reconnection must not
 * turn a loopback source into the program's own playback feedback path. */
export function selectedOutputEndpointIds(state: ParticipantViewState): readonly string[] {
  const selected = new Set<string>()
  if (state.outputEndpointId !== null) selected.add(state.outputEndpointId)
  if (state.mainOutputId !== null) selected.add(state.mainOutputId)
  for (const targets of [
    ...state.localSources.map((source) => source.outputTargets),
    ...state.room.routes.flatMap((route) => [route.outputs, ...route.sources.map((source) => source.outputs)]),
  ]) {
    for (const target of targets) {
      if (target !== 'main') selected.add(target.endpoint)
    }
  }
  return [...selected]
}

/** Describe only the assignment index supplied by the core. */
export function directionRuleSource(
  route: PeerRouteView | null,
  direction: RuleDirection,
  session: RoomSessionView | null,
): string | null {
  if (route === null || session === null) return null
  const origin = direction === 'send' ? route.sendFrom : route.listenFrom
  if (origin == null || !Number.isSafeInteger(origin.index) || origin.index < 0) return null
  const rule = origin.list === 'global'
    ? session.globalRules?.rules[origin.index]
    : origin.list === 'room' ? session.rules[origin.index] : undefined
  if (rule === undefined || rule[direction] === undefined) return null
  if (origin.list === 'global') {
    if (rule.match === 'all') return null
    return `全局规则第 ${origin.index + 1} 条（${matchText(rule.match)}）`
  }
  if (rule.match === 'all' || rule.match.ip === undefined) return null
  return `本房间规则第 ${origin.index + 1} 条，按地址单独设置（${matchText(rule.match)}）`
}

export function ruleFileNeedsAttention(file: RuleFileStatusView): boolean {
  return file.state === 'pending' || file.state === 'external_change' || file.state === 'write_failed'
}
