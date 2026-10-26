import { useState, type FormEvent } from 'react'
import './participant.css'
import type { SetupInput } from './bridge.ts'
import { errorText } from './bridge.ts'
import type { FilePurpose } from './ManagementPanel.tsx'

export function SetupPanel({
  busy,
  configPath,
  message,
  onCreate,
  onPickFile,
}: {
  busy: boolean
  configPath: string
  message: string | null
  onCreate: (input: SetupInput) => Promise<void>
  onPickFile?: (kind: FilePurpose) => Promise<string | null>
}) {
  const [name, setName] = useState('')
  const [bind, setBind] = useState('0.0.0.0:0')
  const [existingPath, setExistingPath] = useState('')
  const [newPath, setNewPath] = useState('')
  const [localError, setLocalError] = useState<string | null>(null)
  const ephemeral = bind.trim().endsWith(':0')

  async function submit(event: FormEvent) {
    event.preventDefault()
    if (busy) return
    setLocalError(null)
    try {
      await onCreate({
        name,
        bind,
        existingPath,
        newPath,
      })
    } catch (error) {
      setLocalError(errorText(error))
    }
  }

  return (
    <form className="setup-panel" onSubmit={(event) => void submit(event)}>
      <h1>建立本机参与</h1>
      <p>
        可以创建本机配置，或打开已有配置继续使用。新配置在主界面进入房间，本机监听默认关闭；已有配置会恢复其中保存的设置。
      </p>
      <p className="field-hint">本步保存本机身份；声音来源和输出随后在应用中配置。程序按管理员权限运行。</p>
      {configPath ? (
        <p>
          默认配置文件：<code>{configPath}</code>
        </p>
      ) : null}
      {message ? (
        <p className="app-message" role="status">
          {message}
        </p>
      ) : null}
      {localError ? (
        <p className="app-message" role="alert">
          {localError}
        </p>
      ) : null}
      <label className="field">
        <span className="field__label">显示名称</span>
        <input
          className="text-input"
          value={name}
          maxLength={80}
          autoComplete="nickname"
          disabled={busy || existingPath.trim() !== ''}
          onChange={(event) => setName(event.target.value)}
        />
      </label>
      <label className="field">
        <span className="field__label">绑定地址</span>
        <input
          className="text-input"
          value={bind}
          spellCheck={false}
          disabled={busy || existingPath.trim() !== ''}
          onChange={(event) => setBind(event.target.value)}
        />
      </label>
      {ephemeral ? (
        <p>
          端口写 0 时，系统会分配实际端口；加入房间后，联络服务会介绍当前地址和端口。
        </p>
      ) : null}
      <details>
        <summary>高级设置：打开现有配置</summary>
        <p>将直接使用所选文件，后续设置会写回该文件，已保存的自动进入房间设置也会恢复。</p>
        <label className="field">
          <span className="field__label">现有配置文件</span>
          <input
            className="text-input"
            value={existingPath}
            spellCheck={false}
            disabled={busy}
            onChange={(event) => setExistingPath(event.target.value)}
          />
        </label>
        {onPickFile ? <button className="button button--secondary" type="button" disabled={busy} onClick={() => {
          setLocalError(null)
          void onPickFile('existing_config').then(path => { if (path) { setExistingPath(path); setNewPath('') } }).catch(cause => setLocalError(errorText(cause)))
        }}>选择已有配置文件</button> : null}
        <p className="field-hint">已有文件损坏时，可以在下面恢复设置包，或另建配置；原文件会保留。</p>
        {newPath ? <p className="readout">新配置位置：{newPath}</p> : null}
        {onPickFile ? <button className="button button--secondary" type="button" disabled={busy} onClick={() => {
          setLocalError(null)
          void onPickFile('new_config').then(path => { if (path) { setNewPath(path); setExistingPath('') } }).catch(cause => setLocalError(errorText(cause)))
        }}>另选位置创建新配置</button> : null}
      </details>
      <button
        className="button button--primary"
        type="submit"
        disabled={busy || (name.trim() === '' && existingPath.trim() === '')}
      >
        {existingPath.trim() ? '打开并启动' : '创建并启动'}
      </button>
    </form>
  )
}
