import {
  cancelRoomAction,
  chooseRoomAction,
  createRoomAction,
  createSavedRoomAction,
  joinSavedRoomAction,
  joinUnknownRoomAction,
  leaveRoomAction,
  refreshRoomsAction,
  reloadRoomRulesAction,
} from './bridge.ts'
import type {
  DiscoveredRoomView,
  JoinModeName,
  ParticipantAction,
  RoomCardView,
  RoomPhase,
  RoomProfileInput,
  RoomSessionView,
  RoomTargetInput,
} from './types.ts'

export interface RoomEntry {
  action: ParticipantAction | null
  message: string | null
}

export interface ChoiceDraft {
  generation: string
  instanceId: string
}

export interface UnknownJoinDraft {
  name: string
  address: string
  port: string
  password: string
  strictAutoJoin: boolean
}

export interface CreateRoomDraft extends UnknownJoinDraft {
  bind: string
}

export interface RoomCreationDefaults {
  contactPort: number | null
  listenAddress: string | null
}

export interface SessionFacts {
  phase: RoomPhase
  profilePath: string | null
  savedAddress: string | null
  selectedEndpoints: readonly string[]
  localService: string | null
  cardPersisted: boolean | null
  failure: string | null
  discoveryFailure: string | null
}

export interface RoomControlFacts {
  error: string | null
  globalRulesPath: string
  autoEnter: boolean
}

export type RoomStage = 'outside' | 'entering' | 'choosing' | 'inside' | 'leaving' | 'failed'

export interface RoomStatus {
  stage: RoomStage
  tone: 'positive' | 'warning' | 'info' | 'neutral' | 'danger'
  title: string
  detail: string | null
}

const EMPTY_NAME = '请填写房间名称。'
const BAD_PORT = '端口需要是 0 到 65535 的整数。'
const MISSING_LISTENER = '暂时无法确定本机的监听地址。请等待网络就绪，或在高级设置中填写监听地址。'
const STALE_CHOICE = '这次选择已经过期，没有提交，也没有改投其他房间。'
const NEED_CHOICE = '请先点选一个当前候选。'

export function localNameLocked(groupJoined: boolean, phase: RoomPhase | null): boolean {
  if (groupJoined) return true
  if (phase == null || phase === 'idle' || phase === 'failed') return false
  return true
}

export function entryCancellationAllowed(groupJoined: boolean, phase: RoomPhase | null): boolean {
  return !groupJoined && phase !== null && ['resolving', 'connecting', 'choosing', 'contact_unavailable', 'failed'].includes(phase)
}

export function currentChoiceDraft(
  session: { phase: RoomPhase; generation: string; choices: readonly { instanceId: string }[] } | null,
  draft: ChoiceDraft | null,
): ChoiceDraft | null {
  if (session?.phase !== 'choosing' || draft === null || draft.generation !== session.generation) return null
  return session.choices.some((choice) => choice.instanceId === draft.instanceId) ? draft : null
}

export function phaseText(phase: RoomPhase): string {
  switch (phase) {
    case 'idle':
      return '空闲'
    case 'resolving':
      return '正在解析'
    case 'choosing':
      return '需要选择'
    case 'connecting':
      return '正在连接'
    case 'joined':
      return '目录已加入'
    case 'contact_unavailable':
      return '联络暂不可用'
    case 'leaving':
      return '正在离开'
    case 'stopped':
      return '会话已停止'
    case 'failed':
      return '未能完成'
  }
}

export function persistedText(cardPersisted: boolean | null): string {
  if (cardPersisted === true) return '已写入'
  if (cardPersisted === false) return '尚未写入'
  return '未知'
}

export function addressText(value: string | null): string {
  if (value == null || value.trim() === '') return '未填写'
  return value
}

export function pathCaption(kind: 'archive' | 'global'): string {
  if (kind === 'archive') return '当前档案'
  return '全局规则文件'
}

function presentPath(value: string | null | undefined): string {
  if (value == null || value.trim() === '') return '无'
  return value
}

function endpointListText(endpoints: readonly string[]): string {
  if (endpoints.length === 0) return '无'
  return endpoints.join('，')
}

export function sessionFactLines(session: SessionFacts): readonly string[] {
  const lines = [
    `房间阶段 ${phaseText(session.phase)}`,
    `${pathCaption('archive')} ${presentPath(session.profilePath)}`,
    `已保存地址 ${addressText(session.savedAddress)}`,
    `选中端点 ${endpointListText(session.selectedEndpoints)}`,
    `创建者监听 ${session.localService == null || session.localService.trim() === '' ? '无' : session.localService}`,
    `卡片写入 ${persistedText(session.cardPersisted)}`,
  ]
  if (session.failure) lines.push(`房间失败说明 ${session.failure}`)
  if (session.discoveryFailure) lines.push(`房间发现失败 ${session.discoveryFailure}`)
  return lines
}

export function roomControlLines(room: RoomControlFacts): readonly string[] {
  const lines = [
    `${pathCaption('global')} ${presentPath(room.globalRulesPath)}`,
    `下次启动自动进入 ${room.autoEnter ? '是' : '否'}`,
  ]
  if (room.error) lines.push(`房间控制错误 ${room.error}`)
  return lines
}

export function cardFactLines(
  card: Pick<RoomCardView, 'path' | 'name' | 'enteredAddress' | 'port' | 'hasPassword' | 'error'>,
): readonly string[] {
  const lines = [
    `卡片 ${card.path}`,
    `卡片名称 ${card.name ?? '无'}`,
    `填写地址 ${addressText(card.enteredAddress)}`,
    `填写端口 ${card.port == null ? '未填写' : String(card.port)}`,
    `有密码 ${card.hasPassword ? '是' : '否'}`,
  ]
  if (card.error) lines.push(`卡片错误 ${card.error}`)
  return lines
}

function instanceFactLines(
  label: string,
  room: { name: string; instanceId: string; candidates: readonly string[] },
): readonly string[] {
  return [
    `${label} ${room.name}`,
    `实例 ${room.instanceId}`,
    `运行候选 ${room.candidates.length === 0 ? '无' : room.candidates.join('，')}`,
  ]
}

export function discoveredFactLines(room: {
  name: string
  instanceId: string
  candidates: readonly string[]
}): readonly string[] {
  return instanceFactLines('发现房间', room)
}

export function choiceFactLines(room: {
  name: string
  instanceId: string
  candidates: readonly string[]
}): readonly string[] {
  return instanceFactLines('待选房间', room)
}

function refused(message: string): RoomEntry {
  return { action: null, message }
}

function accepted(action: ParticipantAction): RoomEntry {
  return { action, message: null }
}

function optionalPort(value: string): { ok: true; port?: number } | { ok: false } {
  const trimmed = value.trim()
  if (trimmed === '') return { ok: true }
  if (!/^\d+$/.test(trimmed)) return { ok: false }
  const port = Number(trimmed)
  if (!Number.isInteger(port) || port > 65535) return { ok: false }
  return { ok: true, port }
}

function blankToNull(value: string): string | null {
  const trimmed = value.trim()
  return trimmed === '' ? null : trimmed
}

function targetFromDraft(name: string, draft: UnknownJoinDraft, port: number | undefined): RoomTargetInput {
  const target: RoomTargetInput = {
    name,
    address: blankToNull(draft.address),
    password: draft.password,
    strict_auto_join: draft.strictAutoJoin,
  }
  if (port !== undefined) target.port = port
  return target
}

export function unknownJoinEntry(draft: UnknownJoinDraft): RoomEntry {
  const name = draft.name
  if (name.trim() === '') return refused(EMPTY_NAME)
  const port = optionalPort(draft.port)
  if (!port.ok) return refused(BAD_PORT)
  return accepted(joinUnknownRoomAction(targetFromDraft(name, draft, port.port)))
}

/** Discovered candidates stay display-only. The join target does not copy them. */
export function joinDiscoveredEntry(
  name: string,
  password: string,
  strictAutoJoin: boolean,
): RoomEntry {
  return unknownJoinEntry({
    name,
    address: '',
    port: '',
    password,
    strictAutoJoin,
  })
}

/**
 * Keep the core-selected address family and scope. An explicit listener owns
 * its port; otherwise only replace the port of the supplied default listener.
 */
export function creatorBind(
  override: string,
  port: number | null | undefined,
  defaults: RoomCreationDefaults,
): string | null {
  const explicit = override.trim()
  if (explicit !== '') return explicit
  if (defaults.listenAddress === null || defaults.contactPort === null) return null
  const separator = defaults.listenAddress.lastIndexOf(':')
  if (separator < 0) return null
  const selectedPort = port == null || port === 0 ? defaults.contactPort : port
  return `${defaults.listenAddress.slice(0, separator + 1)}${selectedPort}`
}

/** A valid port for previews; invalid text gives null and is refused on submit. */
export function portOrNull(value: string): number | null {
  const port = optionalPort(value)
  return port.ok && port.port !== undefined ? port.port : null
}

/** The strict condition needs an entered address; clearing the address clears it. */
export function withAddressCondition<T extends UnknownJoinDraft>(draft: T): T {
  if (draft.address.trim() !== '' || !draft.strictAutoJoin) return draft
  return { ...draft, strictAutoJoin: false }
}

export function createRoomEntry(draft: CreateRoomDraft, defaults: RoomCreationDefaults): RoomEntry {
  const name = draft.name
  if (name.trim() === '') return refused(EMPTY_NAME)
  const port = optionalPort(draft.port)
  if (!port.ok) return refused(BAD_PORT)
  const bind = creatorBind(draft.bind, port.port, defaults)
  if (bind === null) return refused(MISSING_LISTENER)
  const profile: RoomProfileInput = {
    schema: 1,
    room: targetFromDraft(name, draft, port.port),
    rules: [],
  }
  return accepted(createRoomAction(profile, bind))
}

export function createSavedEntry(
  path: string,
  override: string,
  port: number | null,
  defaults: RoomCreationDefaults,
): RoomEntry {
  const bind = creatorBind(override, port, defaults)
  return bind === null ? refused(MISSING_LISTENER) : accepted(createSavedRoomAction(path, bind))
}

export function joinSavedEntry(path: string, mode: JoinModeName): RoomEntry {
  return accepted(joinSavedRoomAction(path, mode))
}

export function chooseFromClick(
  sessionGeneration: string,
  choices: readonly { instanceId: string }[],
  instanceId: string,
): ChoiceDraft | null {
  if (!choices.some((choice) => choice.instanceId === instanceId)) return null
  return { generation: sessionGeneration, instanceId }
}

export function confirmChoiceEntry(
  phase: RoomPhase,
  sessionGeneration: string,
  choices: readonly { instanceId: string }[],
  draft: ChoiceDraft | null,
): RoomEntry {
  if (draft == null) return refused(NEED_CHOICE)
  if (currentChoiceDraft({ phase, generation: sessionGeneration, choices }, draft) === null) {
    return refused(STALE_CHOICE)
  }
  return accepted(chooseRoomAction(draft.generation, draft.instanceId))
}

/** One click on a candidate submits it, after the same validity check as a confirmed draft. */
export function chooseEntry(
  session: { phase: RoomPhase; generation: string; choices: readonly { instanceId: string }[] },
  instanceId: string,
): RoomEntry {
  const draft = chooseFromClick(session.generation, session.choices, instanceId)
  if (draft === null) return refused(STALE_CHOICE)
  return confirmChoiceEntry(session.phase, session.generation, session.choices, draft)
}

/**
 * After a manual join of a saved card fails, the same card can keep waiting
 * in the background through the core's automatic mode.
 */
export function backgroundWaitEntry(
  stage: RoomStage,
  lastJoinedCard: string | null,
  profilePath: string | null,
  failureKind: RoomSessionView['failureKind'],
): RoomEntry | null {
  if (stage !== 'failed' || failureKind !== 'unavailable' || lastJoinedCard === null || profilePath !== lastJoinedCard) return null
  return joinSavedEntry(lastJoinedCard, 'automatic')
}

export function roomFailureText(kind: RoomSessionView['failureKind'], detail: string | null): string | null {
  switch (kind) {
    case 'unavailable':
      return '暂时找不到或联系不上这个房间。可以稍后重试。'
    case 'name_mismatch':
      return '填写的地址对应的房间名称不一致。请检查房间名称或地址后重新加入。'
    case 'password_rejected':
      return '房间密码不正确。请检查房间卡片中的密码后重新加入。'
    case 'protocol_mismatch':
      return '房间联络服务版本不兼容。请同时更新电脑端和路由器上的 RustLanBus 服务。'
    case 'contact_rejected':
      return '房间联络服务拒绝了这次加入。请检查连接详情。'
    case 'configuration':
      return '房间配置无法使用。请检查房间卡片和规则文件。'
    case 'listener_failed':
      return '无法启动本机的房间联络服务。请检查监听地址和端口。'
    case 'discovery_failed':
      return '局域网发现暂时不可用。请检查网络，或填写房间地址后重试。'
    case 'address_required':
      return '需要指定可用的房间地址后再加入。'
    case 'selection_expired':
      return '这次房间选择已经过期。请重新查找并选择房间。'
    case null:
      return detail
  }
}

function roomLabel(name: string | null | undefined): string {
  return name == null || name === '' ? '房间' : `房间「${name}」`
}

export function roomStatus(
  session: Pick<RoomSessionView, 'phase' | 'roomName' | 'failureKind' | 'failure'> | null,
  joined: boolean,
): RoomStatus {
  if (session === null) {
    return { stage: 'outside', tone: 'neutral', title: '尚未进入房间', detail: null }
  }
  const room = roomLabel(session.roomName)
  switch (session.phase) {
    case 'choosing':
      return {
        stage: 'choosing',
        tone: 'info',
        title: `需要选择${room}`,
        detail: '局域网里有多个同名房间，请按地址选择要进入的一个。',
      }
    case 'leaving':
      return { stage: 'leaving', tone: 'info', title: `正在离开${room}`, detail: null }
    case 'joined':
      return joined
        ? { stage: 'inside', tone: 'positive', title: room, detail: '已进入。' }
        : { stage: 'entering', tone: 'info', title: `正在进入${room}`, detail: null }
    case 'contact_unavailable':
      return joined
        ? {
            stage: 'inside',
            tone: 'warning',
            title: room,
            detail: '联络服务暂不可用。已经建立的声音继续，程序正在后台重新联系。',
          }
        : {
            stage: 'entering',
            tone: 'warning',
            title: `正在等待${room}`,
            detail: '暂时找不到或联系不上这个房间，程序会在后台继续尝试。',
          }
    case 'resolving':
      return { stage: 'entering', tone: 'info', title: `正在查找${room}`, detail: null }
    case 'connecting':
      return { stage: 'entering', tone: 'info', title: `正在连接${room}`, detail: null }
    case 'failed':
      return {
        stage: 'failed', tone: 'danger', title: `没能进入${room}`,
        detail: roomFailureText(session.failureKind, session.failure),
      }
    case 'idle':
    case 'stopped':
      return {
        stage: 'outside',
        tone: 'neutral',
        title: '尚未进入房间',
        detail: session.phase === 'stopped' ? '房间会话已停止。' : null,
      }
  }
}

/** The unknown-room pool: discovered rooms that no saved card names exactly. */
export function unknownRooms(
  discovered: readonly DiscoveredRoomView[],
  cards: readonly Pick<RoomCardView, 'name'>[],
  currentInstance: string | null,
): DiscoveredRoomView[] {
  return discovered.filter(
    (room) => room.instanceId !== currentInstance && !cards.some((card) => card.name === room.name),
  )
}

/** Discovered rooms a card would match by its exact name. */
export function cardPresence(
  card: Pick<RoomCardView, 'name'>,
  discovered: readonly DiscoveredRoomView[],
): DiscoveredRoomView[] {
  if (card.name === null) return []
  return discovered.filter((room) => room.name === card.name)
}

export function cardFacts(
  card: Pick<RoomCardView, 'enteredAddress' | 'port' | 'hasPassword' | 'strictAutoJoin'>,
): readonly string[] {
  const facts = [
    card.enteredAddress == null || card.enteredAddress.trim() === ''
      ? '未填写地址，按名称查找'
      : `地址 ${card.enteredAddress}`,
  ]
  if (card.port !== null) facts.push(`端口 ${card.port}`)
  facts.push(card.hasPassword ? '有密码' : '无密码')
  if (card.strictAutoJoin) facts.push('自动进入时要求地址一致')
  return facts
}

export function servicePort(localService: string | null): string | null {
  if (localService == null) return null
  const colon = localService.lastIndexOf(':')
  return colon < 0 ? null : localService.slice(colon + 1)
}

export function tokenLines(session: Pick<RoomSessionView, 'generation' | 'instanceId'>): readonly string[] {
  return [`代次 ${session.generation}`, `实例 ${session.instanceId ?? '无'}`]
}

export function cancelChoice(): { action: ParticipantAction; draft: null } {
  return { action: cancelRoomAction(), draft: null }
}

export function leaveRoomSelection(): { action: ParticipantAction; draft: null } {
  return { action: leaveRoomAction(), draft: null }
}

export function refreshCardsAction(): ParticipantAction {
  return refreshRoomsAction()
}

export function reloadRulesAction(): ParticipantAction {
  return reloadRoomRulesAction()
}
