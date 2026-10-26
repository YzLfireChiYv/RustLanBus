import { useRef, useState } from 'react'
import type { BootReport } from './bridge.ts'
import type { ManagementRequest } from './CardEditor.tsx'
import { actionErrorMessage } from './labels.ts'
import { FieldError } from './primitives.tsx'
import type { ParticipantViewState } from './types.ts'

export interface AppInfo {
  version: string
  buildId: string
  engineBuildId: string
  configPath: string
  executablePath: string
}

export type ImportMode = 'restore' | 'copy_to_new_device'
export type FilePurpose = 'settings_import' | 'settings_export' | 'existing_config' | 'new_config'

export interface BundlePreview {
  source_name: string
  source_revision: string
  mode: ImportMode
  cards: readonly {
    file_name: string
    room_name: string | null
    has_password: boolean
    rule_count: number
    unreadable: boolean
  }[]
  selected_card: string | null
  contains_passwords: boolean
  destination: string | null
  destination_revision: string | null
  warnings: readonly string[]
  removed_fixed_outputs: readonly { scope: string; endpoint_id: string }[]
}

export interface ImportOptions {
  mode: ImportMode
  new_name: string | null
  expected_destination_revision: string | null
  expected_source_revision: string | null
}

export interface ImportResult {
  config_path: string
  recovery_directory: string | null
  mode: ImportMode
  requires_audio_setup: boolean
  installed_revision: string
}

export interface SettingsTransition {
  boot: BootReport
  imported: ImportResult | null
  error: string | null
}

export interface UnsavedChange {
  kind: string
  path: string
  detail: string | null
}

export interface ManagementApi {
  request: ManagementRequest
  pickFile: (kind: FilePurpose) => Promise<string | null>
  preview: (input: string, destination: string | null, mode: ImportMode) => Promise<BundlePreview>
  importSettings: (input: string, destination: string, options: ImportOptions, discardUnsaved: boolean) => Promise<SettingsTransition>
  switchConfig: (path: string, discardUnsaved: boolean) => Promise<unknown>
  pendingChanges: () => Promise<UnsavedChange[]>
}

interface Approval {
  kind: 'import' | 'switch'
  signature: string
  label: string
  changes: readonly UnsavedChange[]
  unknown: boolean
  perform: (discard: boolean) => Promise<void>
}

function changeLabel(kind: string) {
  if (kind === 'configuration') return '本机配置'
  if (kind === 'global') return '全局规则'
  if (kind === 'room') return '房间卡片／规则'
  return '保存状态未确认'
}

function bootReport(value: unknown): BootReport | null {
  if (value !== null && typeof value === 'object' && 'phase' in value
    && (value.phase === 'running' || value.phase === 'setup') && 'configPath' in value
    && typeof value.configPath === 'string') return value as BootReport
  return null
}

function exported(value: unknown): { path: string; preview: BundlePreview } {
  if (value === null || typeof value !== 'object' || !('kind' in value) || value.kind !== 'exported'
    || !('path' in value) || typeof value.path !== 'string' || !('preview' in value)
    || value.preview === null || typeof value.preview !== 'object') {
    throw new Error('共同控制层没有返回导出确认。')
  }
  const preview = value.preview as BundlePreview
  if (!Array.isArray(preview.cards) || !Array.isArray(preview.warnings)) {
    throw new Error('设置包导出结果格式不兼容，请确认程序版本。')
  }
  return { path: value.path, preview }
}

export interface ManagementPanelProps {
  busy: boolean
  state: ParticipantViewState
  info: AppInfo | null
  api: ManagementApi
}

export function ManagementPanel({ busy, state, info, api }: ManagementPanelProps) {
  const [pending, setPending] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [input, setInput] = useState('')
  const [destinationDraft, setDestination] = useState<string | null>(null)
  const [mode, setMode] = useState<ImportMode>('restore')
  const [newName, setNewName] = useState('')
  const [previewed, setPreviewed] = useState<{ value: BundlePreview; signature: string } | null>(null)
  const [replaceConfirmed, setReplaceConfirmed] = useState(false)
  const [approval, setApproval] = useState<Approval | null>(null)
  const [switchPath, setSwitchPath] = useState('')
  const [lastExport, setLastExport] = useState<{ path: string; preview: BundlePreview } | null>(null)
  const [lastTransition, setLastTransition] = useState<SettingsTransition | null>(null)
  const inFlight = useRef(false)
  const locked = busy || pending
  const canExport = state.engine === 'running' || state.engine === 'error'
  const destination = destinationDraft ?? info?.configPath ?? ''
  const signature = JSON.stringify([input.trim(), destination.trim(), mode, newName, info?.configPath ?? ''])
  const switchSignature = JSON.stringify([switchPath.trim(), info?.configPath ?? ''])
  const preview = previewed?.signature === signature ? previewed.value : null
  const hasDestination = destination.trim() !== ''
  const requiresReplacement = preview?.destination_revision != null

  async function work(operation: () => Promise<void>) {
    if (busy || inFlight.current) return
    inFlight.current = true
    setPending(true)
    setError(null)
    setNotice(null)
    try { await operation() } catch (cause) { setError(actionErrorMessage(cause)) } finally {
      inFlight.current = false
      setPending(false)
    }
  }

  function invalidate() {
    setPreviewed(null)
    setReplaceConfirmed(false)
    setApproval(null)
    setLastTransition(null)
    setNotice(null)
  }

  async function pick(kind: FilePurpose, apply: (path: string) => void) {
    await work(async () => {
      const path = await api.pickFile(kind)
      if (path !== null) apply(path)
    })
  }

  async function readPreview() {
    await work(async () => {
      setApproval(null)
      setReplaceConfirmed(false)
      const value = await api.preview(input.trim(), destination.trim() || null, mode)
      const name = newName.trim() === '' ? value.source_name : newName
      if (name !== newName) setNewName(name)
      setPreviewed({ value, signature: JSON.stringify([input.trim(), destination.trim(), mode, name, info?.configPath ?? '']) })
    })
  }

  async function prepare(kind: Approval['kind'], label: string, actionSignature: string, perform: Approval['perform']) {
    try {
      const changes = await api.pendingChanges()
      if (changes.length > 0) {
        setApproval({ kind, label, signature: actionSignature, changes, perform, unknown: false })
        return
      }
    } catch (cause) {
      setApproval({ kind, label, signature: actionSignature, perform, unknown: true, changes: [{
        kind: 'unknown', path: info?.configPath ?? '当前配置', detail: actionErrorMessage(cause),
      }] })
      return
    }
    await perform(false)
  }

  async function continueApproved() {
    if (approval === null) return
    const expected = approval.kind === 'import' ? signature : switchSignature
    if (approval.signature !== expected) {
      setApproval(null)
      setError('设置或当前配置已经改变，请重新预览并确认。')
      return
    }
    await work(async () => {
      if (!approval.unknown) {
        let latest: UnsavedChange[]
        try { latest = await api.pendingChanges() } catch (cause) {
          setApproval({ ...approval, unknown: true, changes: [{ kind: 'unknown', path: info?.configPath ?? '当前配置', detail: actionErrorMessage(cause) }] })
          return
        }
        const acknowledged = new Set(approval.changes.map((item) => `${item.kind}:${item.path}`))
        if (latest.some((item) => !acknowledged.has(`${item.kind}:${item.path}`))) {
          setApproval({ ...approval, changes: latest })
          setNotice('未保存内容发生变化，请确认最新列表。')
          return
        }
      }
      setApproval(null)
      await approval.perform(true)
    })
  }

  async function applyImport(discard: boolean) {
    if (preview === null || !hasDestination) throw new Error('请先完成导入预览。')
    const options: ImportOptions = {
      mode,
      new_name: mode === 'copy_to_new_device' ? newName : null,
      expected_source_revision: preview.source_revision,
      expected_destination_revision: preview.destination_revision,
    }
    try {
      const result = await api.importSettings(input.trim(), destination.trim(), options, discard)
      setLastTransition(result)
      if (result.error !== null) setError(result.error)
      else setNotice(result.imported?.requires_audio_setup ? '设置已复制并启动。请重新确认本机名称、音源、主输出和各路输出；房间不会自动进入。' : '设置已恢复，界面显示的是实际启动后的配置。')
    } finally {
      setPreviewed(null)
      setReplaceConfirmed(false)
    }
  }

  async function applySwitch(discard: boolean) {
    const result = bootReport(await api.switchConfig(switchPath.trim(), discard))
    if (result !== null && (result.phase !== 'running' || result.configPath !== switchPath.trim())) {
      setError(result.message ?? '所选配置未能启动；界面已恢复为实际状态。')
    } else {
      setNotice(result?.message ?? '配置切换操作已结束，请查看当前配置和运行状态。')
    }
  }

  return (
    <details className="management-panel">
      <summary>设置与数据</summary>
      <section className="panel" aria-labelledby="management-heading" aria-busy={pending}>
        <h2 className="panel__title" id="management-heading">设置与数据</h2>
        <div className="panel__body management-panel__body">
          <p className="field-hint">配置保存在用户数据位置。配置切换或导入会停止当前配置的本机音频和房间联系，然后启动所选配置。</p>
          {info ? <div>
            <p className="readout">版本：{info.version}</p>
            <p className="readout">桌面构建：{info.buildId}</p>
            <p className="readout">引擎构建：{info.engineBuildId}</p>
            <p className="readout">当前配置：{info.configPath || '尚未选择'}</p>
            <p className="readout">当前程序：{info.executablePath}</p>
          </div> : <p className="field-hint">版本和当前文件位置尚未读取。</p>}

          <h3 className="panel__title">导出当前设置</h3>
          <p className="field-hint">导出当前生效设置，包括尚未写回原文件的修改、全局规则及全部房间卡片。设置包会包含其中保存的房间密码。导出使用单个可读 JSON 文件。</p>
          <div className="room-actions">
            <button className="button button--secondary" type="button" disabled={locked || !canExport} onClick={() => void work(async () => {
              setLastExport(null)
              const path = await api.pickFile('settings_export')
              if (path === null) return
              const result = await api.request({ type: 'export_settings', path })
              const value = exported(result.management)
              setLastExport(value)
              setNotice('当前生效设置已导出；原配置文件未被写回。')
            })}>导出到新 JSON 文件…</button>
          </div>
          {!canExport ? <p className="field-hint">当前没有运行中的配置，内存设置暂不能导出。可使用下方入口打开或恢复配置。</p> : null}
          {lastExport ? <div role="status">
            <p className="readout">导出文件：{lastExport.path}</p>
            <p className="field-hint">包含 {lastExport.preview.cards.length} 张卡片{lastExport.preview.contains_passwords ? '，以及房间密码或原始卡片资料' : ''}。</p>
            {lastExport.preview.warnings.length > 0 ? <details>
              <summary>导出提示详情</summary>
              <ul>{lastExport.preview.warnings.map((warning, index) => <li key={`${index}-${warning}`}>{warning}</li>)}</ul>
            </details> : null}
          </div> : null}

          <details>
            <summary>从设置包恢复或复制</summary>
            <div className="management-panel__body">
              <label className="field" htmlFor="management-input">
                <span className="field__label">设置包 JSON</span>
                <input id="management-input" className="text-input" spellCheck={false} value={input} disabled={locked}
                  onChange={(event) => { setInput(event.target.value); setNewName(''); invalidate() }} />
              </label>
              <div className="room-actions">
                <button className="button button--secondary" type="button" disabled={locked}
                  onClick={() => void pick('settings_import', (path) => { setInput(path); setNewName(''); invalidate() })}>选择设置包…</button>
              </div>
              <label className="field" htmlFor="management-mode">
                <span className="field__label">导入用途</span>
                <select id="management-mode" className="text-input" value={mode} disabled={locked}
                  onChange={(event) => { setMode(event.target.value as ImportMode); invalidate() }}>
                  <option value="restore">恢复这台电脑的完整设置</option>
                  <option value="copy_to_new_device">复制设置到新设备</option>
                </select>
              </label>
              <p className="field-hint">本机恢复保留设备身份及已保存的设备选择；复制到新设备会创建新身份、清理本机绑定，并等待重新配置。</p>
              {mode === 'copy_to_new_device' ? <label className="field" htmlFor="management-new-name">
                <span className="field__label">新设备显示名称</span>
                <input id="management-new-name" className="text-input" maxLength={80} value={newName} disabled={locked}
                  onChange={(event) => { setNewName(event.target.value); invalidate() }} />
                <span className="field-hint">第一次预览会填入设置包中的名称，请按这台设备的用途确认。</span>
              </label> : null}
              <label className="field" htmlFor="management-destination">
                <span className="field__label">目标本机配置文件</span>
                <input id="management-destination" className="text-input" value={destination} spellCheck={false} disabled={locked}
                  onChange={(event) => { setDestination(event.target.value); invalidate() }} />
              </label>
              <div className="room-actions">
                <button className="button button--secondary" type="button" disabled={locked}
                  onClick={() => void pick('new_config', (path) => { setDestination(path); invalidate() })}>选择新位置…</button>
                <button className="button button--secondary" type="button" disabled={locked}
                  onClick={() => void pick('existing_config', (path) => { setDestination(path); invalidate() })}>另选已有配置…</button>
                <button className="button button--secondary" type="button" disabled={locked || !input.trim()}
                  onClick={() => void readPreview()}>预览导入内容</button>
              </div>
              {preview ? <section className="management-preview" aria-labelledby="management-preview-heading">
                <h4 id="management-preview-heading">导入预览</h4>
                <p className="readout">来源设备：{preview.source_name} · {preview.cards.length} 张房间卡片</p>
                <p className="readout">目标：{preview.destination ?? '尚未选择，请选择后重新预览'}</p>
                <p className="field-hint">{preview.destination === null ? '请选择目标配置位置，再重新预览。' : requiresReplacement ? '目标已有应用资料。确认导入会替换这套本机配置和房间资料，旧资料保留在恢复目录。' : '目标没有现有应用资料，可以新建。'}</p>
                {preview.contains_passwords ? <p className="field-hint">包内含房间密码或原始卡片资料。</p> : null}
                {preview.cards.some((card) => card.unreadable) ? <p className="field-error">包内有无法读取的卡片。其原始内容会保留，修复前不能用于加入房间。</p> : null}
                {preview.cards.length > 0 ? <details>
                  <summary>房间卡片列表</summary>
                  <ul className="management-scroll">{preview.cards.map((card) => <li key={card.file_name}>
                    {card.room_name ?? card.file_name} · {card.unreadable ? '原始内容待修复' : `${card.rule_count} 条规则`}{card.has_password ? ' · 有密码' : ''}
                  </li>)}</ul>
                </details> : null}
                {preview.removed_fixed_outputs.length > 0 ? <div>
                  <p className="field-hint">新设备复制将移除 {preview.removed_fixed_outputs.length} 处固定输出绑定。跟随主输出的符号路由保留；原先仅固定输出的路由会变为不播放，等待重新配置。</p>
                  <details><summary>待重新配置的固定输出</summary>
                    <ul className="management-scroll">{preview.removed_fixed_outputs.map((item, index) => <li key={`${index}-${item.scope}-${item.endpoint_id}`}>{item.scope}：{item.endpoint_id}</li>)}</ul>
                  </details>
                </div> : null}
                {preview.warnings.length > 0 ? <details><summary>核心预览提示详情</summary>
                  <ul>{preview.warnings.map((warning, index) => <li key={`${index}-${warning}`}>{warning}</li>)}</ul>
                </details> : null}
                {requiresReplacement ? <label className="checkbox-field">
                  <input type="checkbox" checked={replaceConfirmed} disabled={locked} onChange={(event) => setReplaceConfirmed(event.target.checked)} />
                  <span>我已查看内容，确认替换目标位置这套配置和房间资料</span>
                </label> : null}
                <div className="room-actions">
                  <button className="button button--primary" type="button"
                    disabled={locked || approval !== null || !hasDestination || (mode === 'copy_to_new_device' && !newName.trim()) || (requiresReplacement && !replaceConfirmed)}
                    onClick={() => void work(() => prepare('import', '停止当前配置并导入', signature, applyImport))}>停止当前配置并导入</button>
                </div>
              </section> : null}
              {previewed !== null && preview === null ? <p className="field-hint">字段或当前配置已改变，请重新预览。</p> : null}
            </div>
          </details>

          <details>
            <summary>切换已有本机配置</summary>
            <div className="management-panel__body">
              <label className="field" htmlFor="management-switch">
                <span className="field__label">已有本机配置文件</span>
                <input id="management-switch" className="text-input" value={switchPath} spellCheck={false} disabled={locked}
                  onChange={(event) => { setSwitchPath(event.target.value); setApproval(null); setNotice(null) }} />
              </label>
              <div className="room-actions">
                <button className="button button--secondary" type="button" disabled={locked}
                  onClick={() => void pick('existing_config', (path) => { setSwitchPath(path); setApproval(null); setNotice(null) })}>选择已有配置…</button>
                <button className="button button--secondary" type="button" disabled={locked || approval !== null || !switchPath.trim()}
                  onClick={() => void work(() => prepare('switch', '停止当前配置并切换', switchSignature, applySwitch))}>停止当前配置并切换</button>
              </div>
            </div>
          </details>

          {approval ? <section className="management-approval" role="status" aria-labelledby="management-unsaved-heading">
            <h3 className="panel__title" id="management-unsaved-heading">{approval.unknown ? '保存状态无法确认' : '当前仍有未保存内容'}</h3>
            <p className="field-hint">继续会放弃当前配置尚未保存的内存修改。可以返回处理，或先导出当前设置。</p>
            <ul>{approval.changes.map((change, index) => <li key={`${index}-${change.kind}-${change.path}`}>
              {changeLabel(change.kind)}：{change.path}{change.detail ? <p className="field-hint">{change.detail}</p> : null}
            </li>)}</ul>
            <div className="room-actions">
              <button className="button" type="button" disabled={locked} onClick={() => void continueApproved()}>放弃未保存并继续：{approval.label}</button>
              <button className="button button--secondary" type="button" disabled={locked} onClick={() => setApproval(null)}>返回处理</button>
            </div>
          </section> : null}
          {lastTransition?.imported ? <div role="status">
            <p className="readout">导入目标：{lastTransition.imported.config_path}</p>
            {lastTransition.imported.recovery_directory ? <p className="readout">导入恢复目录：{lastTransition.imported.recovery_directory}</p> : null}
            {!lastTransition.error && lastTransition.imported.requires_audio_setup ? <p className="field-hint">请在“本机声音和设备”及“输出”中重新选择这台设备的音频绑定。</p> : null}
          </div> : null}
          <FieldError id="management-error" message={error} />
          {notice ? <p className="readout" role="status">{notice}</p> : null}
        </div>
      </section>
    </details>
  )
}
