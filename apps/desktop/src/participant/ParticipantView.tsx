import { useCallback, useState } from 'react'
import './participant.css'
import { GroupPanel } from './GroupPanel.tsx'
import { engineLabel } from './labels.ts'
import { LocalSources } from './LocalSources.tsx'
import { MasterStrip } from './MasterStrip.tsx'
import { MemberList } from './MemberList.tsx'
import { StatusDot } from './primitives.tsx'
import { VirtualReturnPanel } from './VirtualReturnPanel.tsx'
import { WindowsSettings } from './WindowsSettings.tsx'
import { AudioSetupGuide } from './AudioSetupGuide.tsx'
import type {
  EngineState,
  ParticipantAction,
  ParticipantViewProps,
} from './types.ts'

const engineTone: Record<
  EngineState,
  'positive' | 'warning' | 'danger' | 'info' | 'neutral'
> = {
  unavailable: 'neutral',
  starting: 'info',
  running: 'positive',
  error: 'danger',
}

export function ParticipantView({
  state,
  onAction,
  busy,
  onRequest,
  extraSettings,
}: ParticipantViewProps) {
  const [pending, setPending] = useState(0)
  const controlsLocked = busy || pending > 0 || state.engine !== 'running'

  const submit = useCallback(
    async (action: ParticipantAction) => {
      setPending((count) => count + 1)
      try {
        await onAction(action)
      } finally {
        setPending((count) => count - 1)
      }
    },
    [onAction],
  )

  return (
    <div className="app-shell">
      <a className="skip-link" href="#participant-main">
        跳到控制
      </a>
      <header className="app-header">
        <div className="brand">
          <span aria-hidden="true" className="brand-mark">
            <i />
            <i />
            <i />
            <i />
            <i />
          </span>
          <span>
            <h1>RustLanBus</h1>
            <small>本机参与</small>
          </span>
        </div>
        <p className={`engine-pill engine-pill--${state.engine}`}>
          <StatusDot
            pulse={state.engine === 'starting'}
            tone={engineTone[state.engine]}
          />
          <span>{engineLabel[state.engine]}</span>
        </p>
      </header>
      {state.message ? (
        <p className="app-message" role="status">
          {state.message}
        </p>
      ) : null}
      <main
        aria-busy={busy || pending > 0}
        className="participant-grid member-first-grid"
        id="participant-main"
      >
        <GroupPanel disabled={controlsLocked} managementDisabled={busy || pending > 0} onAction={submit} onRequest={onRequest} state={state} />
        <MemberList disabled={controlsLocked} onAction={submit} state={state} />
        <section className="secondary-settings" aria-label="音频设置">
          {extraSettings}
          <AudioSetupGuide disabled={controlsLocked} onAction={submit} state={state} />
          <details id="local-audio-settings">
            <summary>本机声音和设备</summary>
            <LocalSources disabled={controlsLocked} onAction={submit} state={state} />
          </details>
          <details>
            <summary>输出</summary>
            <MasterStrip disabled={controlsLocked} onAction={submit} state={state} />
          </details>
          <details>
            <summary>本机回传检测</summary>
            <VirtualReturnPanel disabled={controlsLocked} onAction={submit} state={state} />
          </details>
          <details>
            <summary>Windows 启动设置</summary>
            <WindowsSettings disabled={controlsLocked} onAction={submit} state={state} />
          </details>
        </section>
      </main>
    </div>
  )
}
