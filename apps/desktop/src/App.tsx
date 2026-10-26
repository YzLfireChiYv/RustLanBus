import './App.css'
import './participant/participant.css'
import { SetupPanel } from './participant/SetupPanel.tsx'
import { ParticipantView } from './participant/ParticipantView.tsx'
import { useParticipantBridge } from './participant/bridge.ts'
import { ManagementPanel } from './participant/ManagementPanel.tsx'
import { NetworkSettings } from './participant/NetworkSettings.tsx'

export default function App() {
  const bridge = useParticipantBridge()

  const management = {
    request: bridge.request,
    pickFile: bridge.pickFile,
    preview: bridge.previewSettings,
    importSettings: bridge.importSettings,
    switchConfig: bridge.switchConfig,
    pendingChanges: bridge.pendingChanges,
  }

  return (
    <>
      <div className="shell-tools">
        <button className="button button--secondary" type="button" disabled={bridge.busy || bridge.phase !== 'running'} onClick={() => void bridge.showDetails()}>
          诊断摘要
        </button>
        <button className="button button--secondary" type="button" disabled={bridge.busy} onClick={() => void bridge.quit()}>
          退出
        </button>
      </div>
      {bridge.phase !== 'running' ? (
      <SetupPanel
        busy={bridge.busy || bridge.phase === 'loading'}
        configPath={bridge.configPath}
        message={bridge.phase === 'loading' ? bridge.state.message : bridge.setupMessage ?? bridge.state.message}
        onCreate={bridge.createConfig}
        onPickFile={bridge.pickFile}
      />
      ) : <ParticipantView busy={bridge.busy} onAction={bridge.command} onRequest={bridge.request} state={bridge.state}
        extraSettings={<NetworkSettings state={bridge.state} busy={bridge.busy} onAction={bridge.command} />} />}
      <div className="app-shell">
        <ManagementPanel busy={bridge.busy || bridge.phase === 'loading'} state={bridge.state} info={bridge.info} api={management} />
      </div>
      {bridge.details !== null ? (
        <section className="details-box">
          <label className="field" htmlFor="participant-details">
            <span className="field__label">当前联系和保存状态</span>
            <textarea id="participant-details" readOnly rows={12} value={bridge.details} />
          </label>
          <button className="button button--secondary" type="button" onClick={bridge.hideDetails}>
            关闭摘要
          </button>
        </section>
      ) : null}
    </>
  )
}
