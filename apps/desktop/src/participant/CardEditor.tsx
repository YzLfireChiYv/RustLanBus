import { useEffect, useRef, useState, type FormEvent } from 'react'
import { actionErrorMessage, controlDomId } from './labels.ts'
import { FieldError } from './primitives.tsx'
import type { ParticipantAction, RoomProfileInput, RoomTargetInput } from './types.ts'

export type ManagementRequest = (action: ParticipantAction) => Promise<{ management?: unknown }>

interface CardDocument {
  path: string
  revision: string
  profile: RoomProfileInput | null
  error: string | null
}

interface Draft {
  name: string
  address: string
  port: string
  password: string
  strictAutoJoin: boolean
}

function readDocument(value: unknown): CardDocument {
  if (value === null || typeof value !== 'object' || !('kind' in value) || value.kind !== 'card'
    || !('document' in value) || value.document === null || typeof value.document !== 'object') {
    throw new Error('共同控制层没有返回卡片内容，请确认程序版本。')
  }
  const document = value.document as Record<string, unknown>
  if (typeof document.path !== 'string' || typeof document.revision !== 'string') {
    throw new Error('卡片内容缺少文件位置或版本标识。')
  }
  if (document.profile !== null) {
    const profile = document.profile as RoomProfileInput | undefined
    if (!profile || typeof profile.room?.name !== 'string' || typeof profile.room.password !== 'string') {
      throw new Error('卡片目标格式不兼容，原文件未被改写。')
    }
  }
  return {
    path: document.path,
    revision: document.revision,
    profile: document.profile as RoomProfileInput | null,
    error: typeof document.error === 'string' ? document.error : null,
  }
}

function toDraft(target: RoomTargetInput): Draft {
  return {
    name: target.name,
    address: target.address ?? '',
    port: target.port == null ? '' : String(target.port),
    password: target.password,
    strictAutoJoin: target.strict_auto_join,
  }
}

function toTarget(draft: Draft): RoomTargetInput {
  if (!draft.name.trim()) throw new Error('请填写房间名称。')
  const address = draft.address.trim()
  const target: RoomTargetInput = {
    name: draft.name,
    address: address || null,
    password: draft.password,
    strict_auto_join: address !== '' && draft.strictAutoJoin,
  }
  if (draft.port.trim() !== '') {
    if (!/^\d+$/.test(draft.port.trim()) || Number(draft.port) > 65535) {
      throw new Error('端口需要是 0 到 65535 的整数；留空使用默认端口。')
    }
    target.port = Number(draft.port)
  }
  return target
}

export interface CardEditorProps {
  path: string
  current: boolean
  disabled: boolean
  onRequest: ManagementRequest
}

export function CardEditor({ path, current, disabled, onRequest }: CardEditorProps) {
  const [loaded, setLoaded] = useState<CardDocument | null>(null)
  const [draft, setDraft] = useState<Draft | null>(null)
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [showPassword, setShowPassword] = useState(false)
  const [confirmReload, setConfirmReload] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [deleted, setDeleted] = useState(false)
  const inFlight = useRef(false)
  const sequence = useRef(0)
  useEffect(() => () => { sequence.current += 1 }, [])
  const locked = disabled || pending
  const id = controlDomId('card-editor', path)
  const dirty = loaded?.profile != null && draft !== null
    && JSON.stringify(draft) !== JSON.stringify(toDraft(loaded.profile.room))

  async function work(operation: () => Promise<void>) {
    if (disabled || inFlight.current) return
    inFlight.current = true
    setPending(true)
    setError(null)
    setNotice(null)
    const started = sequence.current
    try { await operation() } catch (cause) {
      if (started === sequence.current) setError(actionErrorMessage(cause))
    } finally {
      inFlight.current = false
      if (started === sequence.current) setPending(false)
    }
  }

  async function reload() {
    await work(async () => {
      const started = sequence.current
      const result = await onRequest({ type: 'read_room_card', path })
      if (started !== sequence.current) return
      const document = readDocument(result.management)
      setLoaded(document)
      setDraft(document.profile ? toDraft(document.profile.room) : null)
      setShowPassword(false)
      setConfirmReload(false)
      setConfirmDelete(false)
    })
  }

  async function save(event: FormEvent) {
    event.preventDefault()
    if (!loaded || !draft) return
    await work(async () => {
      const target = toTarget(draft)
      const started = sequence.current
      const result = await onRequest({ type: 'save_room_card', path, target, expectedRevision: loaded.revision })
      if (started !== sequence.current) return
      const document = readDocument(result.management)
      setLoaded(document)
      setDraft(document.profile ? toDraft(document.profile.room) : null)
      setConfirmReload(false)
      setConfirmDelete(false)
      setNotice(current ? '卡片已保存；新目标在下次进入或创建房间时生效。' : '卡片已保存。')
    })
  }

  async function remove() {
    if (current || !loaded || !confirmDelete) return
    await work(async () => {
      const started = sequence.current
      const result = await onRequest({ type: 'delete_room_card', path, expectedRevision: loaded.revision })
      if (started !== sequence.current) return
      if (result.management === null || typeof result.management !== 'object'
        || !('kind' in result.management) || result.management.kind !== 'card_deleted') {
        throw new Error('共同控制层尚未确认删除，请刷新卡片列表。')
      }
      setDeleted(true)
    })
  }

  function update<K extends keyof Draft>(key: K, value: Draft[K]) {
    setDraft((old) => old === null ? null : {
      ...old, [key]: value,
      ...(key === 'address' && String(value).trim() === '' ? { strictAutoJoin: false } : {}),
    })
    setNotice(null)
    setConfirmReload(false)
    setConfirmDelete(false)
  }

  if (deleted) return null
  return (
    <details className="room-card__more card-editor" onToggle={(event) => {
      if (event.currentTarget.open && loaded === null && !inFlight.current) void reload()
    }}>
      <summary>编辑或删除卡片{dirty ? ' · 有未保存草稿' : ''}</summary>
      <div className="card-editor__body" aria-busy={pending}>
        <p className="field-hint">编辑保存的房间名称、地址和密码。卡片中的收发、输出和混音规则继续由原文件保存。</p>
        {current ? <p className="field-hint">这是当前使用的卡片。修改目标在下次进入或创建时生效；删除前请先离开房间。</p> : null}
        {loaded === null ? (
          <button className="button button--secondary" type="button" disabled={locked} onClick={() => void reload()}>
            {pending ? '正在读取…' : '读取卡片'}
          </button>
        ) : null}
        {loaded?.profile && draft ? (
          <form className="card-editor__form" onSubmit={(event) => void save(event)}>
            <label className="field" htmlFor={`${id}-name`}>
              <span className="field__label">房间名称</span>
              <input id={`${id}-name`} className="text-input" value={draft.name} maxLength={80}
                autoComplete="off" disabled={locked} onChange={(event) => update('name', event.target.value)} />
            </label>
            <label className="field" htmlFor={`${id}-address`}>
              <span className="field__label">保存地址</span>
              <input id={`${id}-address`} className="text-input" value={draft.address} spellCheck={false}
                placeholder="可留空" disabled={locked} onChange={(event) => update('address', event.target.value)} />
              <span className="field-hint">留空就不限定地址；扫描到的地址不会自动填入。</span>
            </label>
            <label className="field" htmlFor={`${id}-port`}>
              <span className="field__label">联络端口</span>
              <input id={`${id}-port`} className="text-input" inputMode="numeric" value={draft.port}
                placeholder="默认端口" disabled={locked} onChange={(event) => update('port', event.target.value)} />
            </label>
            <label className="field" htmlFor={`${id}-password`}>
              <span className="field__label">卡片密码</span>
              <input id={`${id}-password`} className="text-input" type={showPassword ? 'text' : 'password'}
                value={draft.password} autoComplete="off" spellCheck={false} disabled={locked}
                onChange={(event) => update('password', event.target.value)} />
            </label>
            <label className="checkbox-field">
              <input type="checkbox" checked={showPassword} disabled={locked} onChange={(event) => setShowPassword(event.target.checked)} />
              <span>在本表单中显示密码</span>
            </label>
            <label className="checkbox-field">
              <input type="checkbox" checked={draft.strictAutoJoin} disabled={locked || !draft.address.trim()}
                onChange={(event) => update('strictAutoJoin', event.target.checked)} />
              <span>自动进入时要求名称和保存地址同时匹配</span>
            </label>
            <div className="room-actions">
              <button className="button button--primary" type="submit" disabled={locked || !loaded.revision || Boolean(loaded.error)}>保存卡片</button>
            </div>
          </form>
        ) : null}
        {loaded?.error ? <p className="field-error" role="alert">卡片目前不能作为有效配置使用：{loaded.error}</p> : null}
        {loaded !== null ? (
          <>
            <p className="readout">文件：{loaded.path}</p>
            <div className="room-actions">
              <button className="button button--secondary" type="button" disabled={locked} onClick={() => {
                if (dirty) setConfirmReload(true)
                else void reload()
              }}>重新读取卡片内容</button>
              <button className="button button--secondary" type="button" disabled={locked || current || !loaded.revision}
                onClick={() => { setConfirmDelete(true); setConfirmReload(false) }}>删除卡片…</button>
            </div>
            {current ? <p className="field-hint">这里读取活动卡片的有效内容。文件在软件外改动时，请先处理“本房间规则”中的重新载入提示，再重新读取卡片。</p> : null}
          </>
        ) : null}
        {confirmReload ? <div className="card-editor__confirmation" role="status">
          <p>重新读取会替换本表单尚未保存的草稿。</p>
          <div className="room-actions">
            <button className="button button--secondary" type="button" disabled={locked} onClick={() => void reload()}>放弃草稿并读取</button>
            <button className="button button--secondary" type="button" disabled={locked} onClick={() => setConfirmReload(false)}>保留草稿</button>
          </div>
        </div> : null}
        {confirmDelete && !current ? <div className="card-editor__confirmation" role="status">
          <p>删除范围：这张卡片的文件，以及其中保存的房间收发、输出和混音规则。</p>
          <div className="room-actions">
            <button className="button" type="button" disabled={locked} onClick={() => void remove()}>删除卡片文件</button>
            <button className="button button--secondary" type="button" disabled={locked} onClick={() => setConfirmDelete(false)}>保留卡片</button>
          </div>
        </div> : null}
        <FieldError id={`${id}-error`} message={error} />
        {notice ? <p className="readout" role="status">{notice}</p> : null}
      </div>
    </details>
  )
}
