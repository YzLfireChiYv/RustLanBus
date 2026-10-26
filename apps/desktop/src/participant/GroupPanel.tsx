import { useState } from 'react'
import { actionErrorMessage, controlDomId } from './labels.ts'
import { FieldError, StatusDot } from './primitives.tsx'
import { CardEditor } from './CardEditor.tsx'
import type { ManagementRequest } from './CardEditor.tsx'
import {
  backgroundWaitEntry,
  cancelChoice,
  cardFactLines,
  cardFacts,
  cardPresence,
  choiceFactLines,
  chooseEntry,
  createRoomEntry,
  createSavedEntry,
  creatorBind,
  discoveredFactLines,
  entryCancellationAllowed,
  joinDiscoveredEntry,
  joinSavedEntry,
  leaveRoomSelection,
  localNameLocked,
  portOrNull,
  refreshCardsAction,
  roomControlLines,
  roomStatus,
  servicePort,
  sessionFactLines,
  tokenLines,
  unknownJoinEntry,
  unknownRooms,
  withAddressCondition,
  type CreateRoomDraft,
  type RoomCreationDefaults,
  type RoomEntry,
  type RoomStatus,
  type UnknownJoinDraft,
} from './roomFlow.ts'
import type {
  DiscoveredRoomView,
  ParticipantAction,
  ParticipantViewState,
  RoomCardView,
  RoomSessionView,
} from './types.ts'

async function runAction(
  onAction: (action: ParticipantAction) => Promise<void>,
  action: ParticipantAction,
  setMessage: (message: string | null) => void,
) {
  setMessage(null)
  try {
    await onAction(action)
    return true
  } catch (error) {
    setMessage(actionErrorMessage(error))
    return false
  }
}

interface GroupPanelProps {
  state: ParticipantViewState
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
  onRequest?: ManagementRequest
  managementDisabled?: boolean
}

const emptyJoin: UnknownJoinDraft = {
  name: '',
  address: '',
  port: '',
  password: '',
  strictAutoJoin: false,
}

const emptyCreate: CreateRoomDraft = { ...emptyJoin, bind: '' }

function FactLines({ lines }: { lines: readonly string[] }) {
  return lines.map((line, index) => {
    const alert = line.includes('错误') || line.includes('失败')
    return (
      <p className={alert ? 'field-error' : 'readout'} key={`${index}-${line}`} role={alert ? 'alert' : undefined}>
        {line}
      </p>
    )
  })
}

function TextField({
  id,
  label,
  value,
  type = 'text',
  placeholder,
  hint,
  maxLength,
  disabled,
  onChange,
  onEnter,
}: {
  id: string
  label: string
  value: string
  type?: 'text' | 'password'
  placeholder?: string
  hint?: string
  maxLength?: number
  disabled: boolean
  onChange: (value: string) => void
  onEnter?: () => void
}) {
  return (
    <div className="field">
      <label className="field__label" htmlFor={id}>
        {label}
      </label>
      <input
        id={id}
        className="text-input"
        type={type}
        value={value}
        placeholder={placeholder}
        maxLength={maxLength}
        spellCheck={false}
        autoComplete="off"
        disabled={disabled}
        aria-describedby={hint ? `${id}-hint` : undefined}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          if (onEnter === undefined || event.key !== 'Enter' || event.nativeEvent.isComposing) return
          event.preventDefault()
          onEnter()
        }}
      />
      {hint ? (
        <p className="field-hint" id={`${id}-hint`}>
          {hint}
        </p>
      ) : null}
    </div>
  )
}

function AddressConditionField({
  id,
  checked,
  disabled,
  onChange,
}: {
  id: string
  checked: boolean
  disabled: boolean
  onChange: (checked: boolean) => void
}) {
  return (
    <div className="field">
      <label className="checkbox-field" htmlFor={id}>
        <input
          id={id}
          type="checkbox"
          checked={checked}
          disabled={disabled}
          aria-describedby={`${id}-hint`}
          onChange={(event) => onChange(event.target.checked)}
        />
        <span>自动进入时要求地址一致</span>
      </label>
      <p className="field-hint" id={`${id}-hint`}>
        勾选后，程序启动时只有在这个地址找到同名房间才会自动进入；手动加入不受影响。需要先填写地址。
      </p>
    </div>
  )
}

function LocalNameField({
  localName,
  locked,
  disabled,
  onAction,
}: {
  localName: string
  locked: boolean
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const [draft, setDraft] = useState<string | null>(null)
  const [message, setMessage] = useState<string | null>(null)
  const value = locked ? localName : (draft ?? localName)
  const changed = !locked && draft !== null && draft !== localName

  function save() {
    if (disabled || !changed || draft === null) return
    if (!draft.trim()) {
      setMessage('请填写本机名称。')
      return
    }
    void runAction(onAction, { type: 'set_name', name: draft }, setMessage).then((applied) => {
      if (applied) setDraft(null)
    })
  }

  return (
    <div className="field">
      <label className="field__label" htmlFor="local-name">
        本机名称
      </label>
      <div className="field__row">
        <input
          id="local-name"
          className="text-input"
          type="text"
          value={value}
          maxLength={80}
          autoComplete="off"
          disabled={disabled || locked}
          aria-describedby={locked ? 'local-name-joined-hint' : undefined}
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if (event.key !== 'Enter' || event.nativeEvent.isComposing) return
            event.preventDefault()
            save()
          }}
        />
        <button className="button button--secondary" type="button" disabled={disabled || !changed} onClick={save}>
          保存名称
        </button>
      </div>
      {locked ? (
        <p className="field-hint" id="local-name-joined-hint">
          离开房间后可以修改本机名称。
        </p>
      ) : null}
      <FieldError id="local-name-error" message={message} />
    </div>
  )
}

function RoomStatusBlock({
  status,
  session,
  state,
  canCancel,
  waitAvailable,
  disabled,
  onCancel,
  onLeave,
  onWait,
}: {
  status: RoomStatus
  session: RoomSessionView | null
  state: ParticipantViewState
  canCancel: boolean
  waitAvailable: boolean
  disabled: boolean
  onCancel: () => void
  onLeave: () => void
  onWait: () => void
}) {
  const inside = status.stage === 'inside'
  const port = inside ? servicePort(session?.localService ?? null) : null
  const cardUnsaved = inside && session?.cardPersisted === false
  const roomUnsaved = session !== null && ['pending', 'external_change', 'write_failed'].includes(session.ruleFiles.room.state)
  const leaveAvailable = state.group.joined && status.stage !== 'leaving'
  const operationError = status.stage === 'failed' && session?.failureKind != null && state.room.error === session.failure
    ? null
    : state.room.error
  return (
    <div className={`room-status room-status--${status.tone}`} role="status">
      <p className="room-status__title">
        <StatusDot tone={status.tone} pulse={status.stage === 'entering'} />
        <strong>{status.title}</strong>
      </p>
      {status.detail ? <p className="room-status__detail">{status.detail}</p> : null}
      {port ? <p className="room-status__detail">本机提供这个房间的联络服务，端口 {port}。</p> : null}
      {cardUnsaved ? (
        <p className="room-status__detail room-status__detail--warning">
          这个房间的卡片还没有保存成功。处理好文件位置的问题后，可在成员区域重新读取规则文件来重试。
        </p>
      ) : null}
      {inside && state.room.autoEnter ? (
        <p className="room-status__detail">下次启动时会自动进入这个房间。</p>
      ) : null}
      {operationError ? (
        <p className="field-error" role="alert">
          房间操作没有完成：{operationError}
        </p>
      ) : null}
      {roomUnsaved ? (
        <p className="room-status__detail room-status__detail--warning">
          离开或切换房间会放弃本房间未保存的修改；未保存的全局修改会保留。
        </p>
      ) : null}
      {waitAvailable || canCancel || leaveAvailable ? (
        <div className="room-actions">
          {waitAvailable ? (
            <button className="button button--primary" type="button" disabled={disabled} onClick={onWait}>
              在后台继续等待
            </button>
          ) : null}
          {canCancel ? (
            <button className="button button--secondary" type="button" disabled={disabled} onClick={onCancel}>
              {status.stage === 'failed' ? '关闭提示' : '取消'}
            </button>
          ) : null}
          {leaveAvailable ? (
            <button className="button button--secondary" type="button" disabled={disabled} onClick={onLeave}>
              离开房间
            </button>
          ) : null}
        </div>
      ) : null}
    </div>
  )
}

function ChoiceList({
  session,
  disabled,
  onChoose,
}: {
  session: RoomSessionView
  disabled: boolean
  onChoose: (instanceId: string) => void
}) {
  return (
    <ul className="room-list">
      {session.choices.map((choice) => (
        <li className="room-card" key={choice.instanceId}>
          <p className="room-card__name">
            <strong>{choice.name}</strong>
          </p>
          <p className="room-card__facts">
            {choice.candidates.length === 0 ? '没有可用地址' : choice.candidates.join('、')}
          </p>
          <div className="room-actions">
            <button
              className="button button--primary"
              type="button"
              disabled={disabled}
              onClick={() => onChoose(choice.instanceId)}
            >
              进入这个
            </button>
          </div>
        </li>
      ))}
    </ul>
  )
}

function CardItem({
  card,
  current,
  presence,
  disabled,
  onJoin,
  onCreate,
  onRequest,
  cardInUse,
  managementDisabled,
}: {
  card: RoomCardView
  current: boolean
  presence: readonly DiscoveredRoomView[]
  disabled: boolean
  onJoin: (card: RoomCardView) => void
  onCreate: (card: RoomCardView) => void
  onRequest?: ManagementRequest
  cardInUse: boolean
  managementDisabled: boolean
}) {
  const readable = card.error === null && card.name !== null
  return (
    <li className={current ? 'room-card room-card--current' : 'room-card'}>
      <p className="room-card__name">
        <strong>{card.name ?? '无法读取的卡片'}</strong>
        {current ? <span className="room-badge room-badge--current">当前房间</span> : null}
        {readable && presence.length === 1 ? (
          <span className="room-badge room-badge--found">局域网中可见</span>
        ) : null}
        {readable && presence.length > 1 ? (
          <span className="room-badge room-badge--found">发现 {presence.length} 个同名房间</span>
        ) : null}
      </p>
      {readable ? <p className="room-card__facts">{cardFacts(card).join(' · ')}</p> : null}
      {card.error ? (
        <p className="field-error" role="alert">
          卡片文件无法读取。请检查文件内容；程序没有改动这个文件。
        </p>
      ) : null}
      <details className="room-card__more">
        <summary>卡片详情</summary>
        <FactLines lines={cardFactLines(card)} />
      </details>
      {readable && !current ? (
        <div className="room-actions">
          <button className="button button--primary" type="button" disabled={disabled} onClick={() => onJoin(card)}>
            加入
          </button>
          <button className="button button--secondary" type="button" disabled={disabled} onClick={() => onCreate(card)}>
            在本机创建
          </button>
        </div>
      ) : null}
      {onRequest ? <CardEditor path={card.path} current={cardInUse} disabled={managementDisabled} onRequest={onRequest} /> : null}
    </li>
  )
}

function DiscoveredItem({
  room,
  disabled,
  onJoin,
}: {
  room: DiscoveredRoomView
  disabled: boolean
  onJoin: (name: string, password: string) => void
}) {
  const [open, setOpen] = useState(false)
  const [password, setPassword] = useState('')
  const passwordId = controlDomId('discovered-password', room.instanceId)
  return (
    <li className="room-card">
      <p className="room-card__name">
        <strong>{room.name}</strong>
      </p>
      <p className="room-card__facts">
        {room.candidates.length === 0 ? '没有可用地址' : room.candidates.join('、')}
      </p>
      {open ? (
        <div className="room-inline">
          <TextField
            id={passwordId}
            label="房间密码"
            type="password"
            value={password}
            hint="没有密码时留空。加入成功后会保存为房间卡片。"
            disabled={disabled}
            onChange={setPassword}
            onEnter={() => onJoin(room.name, password)}
          />
          <div className="room-actions">
            <button
              className="button button--primary"
              type="button"
              disabled={disabled}
              onClick={() => onJoin(room.name, password)}
            >
              加入
            </button>
            <button className="button button--secondary" type="button" onClick={() => setOpen(false)}>
              收起
            </button>
          </div>
        </div>
      ) : (
        <div className="room-actions">
          <button className="button button--secondary" type="button" disabled={disabled} onClick={() => setOpen(true)}>
            加入…
          </button>
        </div>
      )}
    </li>
  )
}

function JoinForm({
  disabled,
  defaults,
  onSubmit,
}: {
  disabled: boolean
  defaults: RoomCreationDefaults
  onSubmit: (draft: UnknownJoinDraft) => void
}) {
  const [draft, setDraft] = useState<UnknownJoinDraft>(emptyJoin)
  const change = (patch: Partial<UnknownJoinDraft>) => {
    setDraft((current) => withAddressCondition({ ...current, ...patch }))
  }
  return (
    <details className="details room-form">
      <summary>按名称加入其他房间</summary>
      <div className="details__body">
        <TextField
          id="join-name"
          label="房间名称"
          value={draft.name}
          maxLength={80}
          disabled={disabled}
          onChange={(name) => change({ name })}
          onEnter={() => onSubmit(draft)}
        />
        <TextField
          id="join-password"
          label="房间密码"
          type="password"
          value={draft.password}
          hint="没有密码时留空。"
          disabled={disabled}
          onChange={(password) => change({ password })}
          onEnter={() => onSubmit(draft)}
        />
        <details className="details">
          <summary>指定地址（可选）</summary>
          <div className="details__body">
            <TextField
              id="join-address"
              label="房间地址"
              value={draft.address}
              placeholder="留空时按名称在局域网中查找"
              disabled={disabled}
              onChange={(address) => change({ address })}
            />
            <TextField
              id="join-port"
              label="端口"
              value={draft.port}
              placeholder={defaults.contactPort === null ? '留空时使用默认端口' : `留空时使用 ${defaults.contactPort}`}
              disabled={disabled}
              onChange={(port) => change({ port })}
            />
            <AddressConditionField
              id="join-strict"
              checked={draft.strictAutoJoin}
              disabled={disabled || draft.address.trim() === ''}
              onChange={(strictAutoJoin) => change({ strictAutoJoin })}
            />
          </div>
        </details>
        <div className="room-actions">
          <button className="button button--primary" type="button" disabled={disabled} onClick={() => onSubmit(draft)}>
            加入
          </button>
        </div>
        <p className="field-hint">加入成功后会保存为房间卡片。找不到这个房间时不会创建新房间。</p>
      </div>
    </details>
  )
}

function CreateForm({
  disabled,
  defaults,
  bindOverride,
  onBindOverride,
  onSubmit,
}: {
  disabled: boolean
  defaults: RoomCreationDefaults
  bindOverride: string
  onBindOverride: (value: string) => void
  onSubmit: (draft: CreateRoomDraft) => void
}) {
  const [draft, setDraft] = useState<CreateRoomDraft>(emptyCreate)
  const change = (patch: Partial<CreateRoomDraft>) => {
    setDraft((current) => withAddressCondition({ ...current, ...patch }))
  }
  return (
    <details className="details room-form">
      <summary>创建房间</summary>
      <div className="details__body">
        <TextField
          id="create-name"
          label="房间名称"
          value={draft.name}
          maxLength={80}
          disabled={disabled}
          onChange={(name) => change({ name })}
          onEnter={() => onSubmit(draft)}
        />
        <TextField
          id="create-password"
          label="房间密码"
          type="password"
          value={draft.password}
          hint="其他设备加入时需要输入。没有密码时留空。"
          disabled={disabled}
          onChange={(password) => change({ password })}
          onEnter={() => onSubmit(draft)}
        />
        <details className="details">
          <summary>高级设置</summary>
          <div className="details__body">
            <TextField
              id="create-port"
              label="房间端口"
              value={draft.port}
              placeholder={defaults.contactPort === null ? '留空时使用默认端口' : `留空时使用 ${defaults.contactPort}`}
              disabled={disabled}
              onChange={(port) => change({ port })}
            />
            <TextField
              id="create-bind"
              label="创建时的监听地址"
              value={bindOverride}
              placeholder={creatorBind('', portOrNull(draft.port), defaults) ?? '等待网络就绪，或填写监听地址'}
              hint="留空时使用本机网络对应的监听地址。这个设置也用于卡片上的「在本机创建」。"
              disabled={disabled}
              onChange={onBindOverride}
            />
            <TextField
              id="create-address"
              label="卡片中保存的房间地址"
              value={draft.address}
              placeholder="留空表示不填写"
              hint="以后从这张卡片加入时使用；不会改动这台电脑的网络设置。"
              disabled={disabled}
              onChange={(address) => change({ address })}
            />
            <AddressConditionField
              id="create-strict"
              checked={draft.strictAutoJoin}
              disabled={disabled || draft.address.trim() === ''}
              onChange={(strictAutoJoin) => change({ strictAutoJoin })}
            />
          </div>
        </details>
        <div className="room-actions">
          <button className="button button--primary" type="button" disabled={disabled} onClick={() => onSubmit(draft)}>
            创建
          </button>
        </div>
        <p className="field-hint">创建后本机提供这个房间的联络服务，并保存为房间卡片。</p>
      </div>
    </details>
  )
}

function EntrySections({
  state,
  session,
  currentCard,
  disabled,
  bindOverride,
  onBindOverride,
  onJoinCard,
  onCreateCard,
  onJoinDiscovered,
  onJoinUnknown,
  onCreate,
  onRefresh,
  onRequest,
  managementDisabled,
}: {
  state: ParticipantViewState
  session: RoomSessionView | null
  currentCard: string | null
  disabled: boolean
  bindOverride: string
  onBindOverride: (value: string) => void
  onJoinCard: (card: RoomCardView) => void
  onCreateCard: (card: RoomCardView) => void
  onJoinDiscovered: (name: string, password: string) => void
  onJoinUnknown: (draft: UnknownJoinDraft) => void
  onCreate: (draft: CreateRoomDraft) => void
  onRefresh: () => void
  onRequest?: ManagementRequest
  managementDisabled: boolean
}) {
  const discovered = session?.discoveredRooms ?? []
  const others = unknownRooms(discovered, state.room.cards, session?.instanceId ?? null)
  return (
    <div className="room-sections">
      <section className="room-section" aria-labelledby="saved-rooms-heading">
        <div className="room-section__head">
          <h3 id="saved-rooms-heading">已保存的房间</h3>
          <button className="button button--quiet" type="button" disabled={disabled} onClick={onRefresh}>
            刷新
          </button>
        </div>
        {state.room.cards.length === 0 ? (
          <p className="field-hint">还没有保存的房间。加入或创建房间成功后会自动保存。</p>
        ) : (
          <ul className="room-list">
            {state.room.cards.map((card) => (
              <CardItem
                key={card.path}
                card={card}
                current={card.path === currentCard}
                presence={cardPresence(card, discovered)}
                disabled={disabled}
                onJoin={onJoinCard}
                onCreate={onCreateCard}
                onRequest={onRequest}
                managementDisabled={managementDisabled}
                cardInUse={session !== null && session.profilePath === card.path && session.phase !== 'idle' && session.phase !== 'stopped'}
              />
            ))}
          </ul>
        )}
      </section>
      <section className="room-section" aria-labelledby="other-rooms-heading">
        <h3 id="other-rooms-heading">局域网中的其他房间</h3>
        {session?.discoveryFailure ? (
          <p className="field-error" role="alert">
            局域网发现暂时不可用：{session.discoveryFailure}
          </p>
        ) : null}
        {others.length === 0 ? (
          <p className="field-hint">暂时没有发现未保存的房间。</p>
        ) : (
          <ul className="room-list">
            {others.map((room) => (
              <DiscoveredItem key={room.instanceId} room={room} disabled={disabled} onJoin={onJoinDiscovered} />
            ))}
          </ul>
        )}
      </section>
      <JoinForm disabled={disabled} defaults={state.room.creationDefaults} onSubmit={onJoinUnknown} />
      <CreateForm
        disabled={disabled}
        defaults={state.room.creationDefaults}
        bindOverride={bindOverride}
        onBindOverride={onBindOverride}
        onSubmit={onCreate}
      />
    </div>
  )
}

function Diagnostics({ state, session }: { state: ParticipantViewState; session: RoomSessionView | null }) {
  return (
    <details className="details room-diagnostics">
      <summary>连接详情</summary>
      <div className="details__body">
        <FactLines lines={roomControlLines(state.room)} />
        {session ? (
          <>
            <FactLines lines={[...sessionFactLines(session), ...tokenLines(session)]} />
            {session.choices.map((choice) => (
              <FactLines key={`choice-${choice.instanceId}`} lines={choiceFactLines(choice)} />
            ))}
            {session.discoveredRooms.map((room) => (
              <FactLines key={`found-${room.instanceId}`} lines={discoveredFactLines(room)} />
            ))}
          </>
        ) : (
          <p className="readout">房间会话 无</p>
        )}
      </div>
    </details>
  )
}

export function GroupPanel({ state, disabled, onAction, onRequest, managementDisabled = disabled }: GroupPanelProps) {
  const session = state.room.session
  const joined = state.group.joined
  const phase = session?.phase ?? null
  const status = roomStatus(session, joined)
  const nameLocked = localNameLocked(joined, phase)
  const canCancel = entryCancellationAllowed(joined, phase)
  const [message, setMessage] = useState<string | null>(null)
  const [lastJoinedCard, setLastJoinedCard] = useState<string | null>(null)
  const [bindOverride, setBindOverride] = useState('')
  const waitEntry = backgroundWaitEntry(status.stage, lastJoinedCard, session?.profilePath ?? null, session?.failureKind ?? null)
  const occupied = status.stage !== 'outside' && status.stage !== 'failed'
  const currentCard = occupied ? (session?.profilePath ?? null) : null

  function submit(entry: RoomEntry, joinedCard?: string | null) {
    if (disabled) return
    if (entry.action === null) {
      setMessage(entry.message)
      return
    }
    if (joinedCard !== undefined) setLastJoinedCard(joinedCard)
    void runAction(onAction, entry.action, setMessage)
  }

  function leave() {
    if (disabled) return
    setLastJoinedCard(null)
    void runAction(onAction, leaveRoomSelection().action, setMessage)
  }

  function cancel() {
    if (disabled || !canCancel) return
    setLastJoinedCard(null)
    void runAction(onAction, cancelChoice().action, setMessage)
  }

  const entry = (
    <EntrySections
      state={state}
      session={session}
      currentCard={currentCard}
      disabled={disabled}
      bindOverride={bindOverride}
      onBindOverride={setBindOverride}
      onJoinCard={(card) => submit(joinSavedEntry(card.path, 'manual'), card.path)}
      onCreateCard={(card) => submit(createSavedEntry(card.path, bindOverride, card.port, state.room.creationDefaults), null)}
      onJoinDiscovered={(name, password) => submit(joinDiscoveredEntry(name, password, false), null)}
      onJoinUnknown={(draft) => submit(unknownJoinEntry(draft), null)}
      onCreate={(draft) => submit(createRoomEntry({ ...draft, bind: bindOverride }, state.room.creationDefaults), null)}
      onRefresh={() => {
        if (disabled) return
        void runAction(onAction, refreshCardsAction(), setMessage)
      }}
      onRequest={onRequest}
      managementDisabled={managementDisabled}
    />
  )

  return (
    <section className="panel room-panel" aria-labelledby="room-heading">
      <h2 id="room-heading" className="panel__title">
        房间
      </h2>
      <div className="panel__body">
        <LocalNameField
          localName={state.localName}
          locked={nameLocked}
          disabled={disabled}
          onAction={onAction}
        />
        <RoomStatusBlock
          status={status}
          session={session}
          state={state}
          canCancel={canCancel}
          waitAvailable={waitEntry !== null}
          disabled={disabled}
          onCancel={cancel}
          onLeave={leave}
          onWait={() => {
            if (waitEntry !== null) submit(waitEntry, null)
          }}
        />
        {status.stage === 'choosing' && session ? (
          <ChoiceList
            session={session}
            disabled={disabled}
            onChoose={(instanceId) => submit(chooseEntry(session, instanceId))}
          />
        ) : null}
        <FieldError id="room-panel-error" message={message} />
        {occupied ? null : entry}
        {status.stage === 'inside' ? (
          <details className="details room-switch">
            <summary>切换到其他房间</summary>
            <div className="details__body">{entry}</div>
          </details>
        ) : null}
        <Diagnostics state={state} session={session} />
      </div>
    </section>
  )
}
