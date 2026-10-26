import { invoke } from '@tauri-apps/api/core'
import { useCallback, useEffect, useRef, useState } from 'react'
import { unavailableParticipantState } from './state.ts'
import type { AppInfo, BundlePreview, FilePurpose, ImportMode, ImportOptions, SettingsTransition, UnsavedChange } from './ManagementPanel.tsx'
import type {
  CoreMixPatch,
  CoreGlobalRules,
  JoinModeName,
  ParticipantAction,
  ParticipantViewState,
  RoomProfileInput,
  RoomRuleInput,
  RoomTargetInput,
  RuleMatchInput,
  SourceKind,
} from './types.ts'

export interface BootReport {
  phase: 'setup' | 'running'
  configPath: string
  state: ParticipantViewState
  message: string | null
}

export interface SetupInput {
  name: string
  bind: string
  existingPath: string
  newPath?: string
}

/** A poll started at `startedGeneration` may replace state only while that generation is still current. */
export function pollMayApply(startedGeneration: number, currentGeneration: number): boolean {
  return startedGeneration === currentGeneration
}

/** Capture before the read; an intervening command or unmount retires it. */
export async function pollParticipantOnce<T>(
  read: () => Promise<T>, generation: { readonly current: number },
  active: () => boolean, apply: (value: T) => void,
): Promise<void> {
  const started = generation.current
  const value = await read()
  if (active() && pollMayApply(started, generation.current)) apply(value)
}

export interface CommandResult {
  applied: boolean
  message: string
  persistenceError: string | null
  state: ParticipantViewState
  management?: unknown
}

// Room commands are queued; their effect reaches the snapshot shortly after
// the command itself returns.
const FOLLOW_UP_POLLS_MS = [150, 600] as const

export function useParticipantBridge() {
  const [phase, setPhase] = useState<'loading' | 'setup' | 'running'>('loading')
  const [state, setState] = useState<ParticipantViewState>({
    ...unavailableParticipantState,
    message: '正在读取本机配置。',
  })
  const [configPath, setConfigPath] = useState('')
  const [info, setInfo] = useState<AppInfo | null>(null)
  const [setupMessage, setSetupMessage] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [details, setDetails] = useState<string | null>(null)
  const [controlError, setControlError] = useState<string | null>(null)
  const [operationError, setOperationError] = useState<string | null>(null)
  const [lastSnapshotAt, setLastSnapshotAt] = useState<string | null>(null)
  const generation = useRef(0)
  const pollInFlight = useRef(false)
  const busyRef = useRef(false)
  const followUp = useRef<(() => void) | null>(null)

  const applySnapshot = useCallback((snapshot: ParticipantViewState) => {
    setState(snapshot)
    setLastSnapshotAt(new Date().toISOString())
    setControlError(null)
  }, [])

  const applyBoot = useCallback((report: BootReport) => {
    setConfigPath(report.configPath)
    setSetupMessage(report.message)
    applySnapshot(report.state)
    setOperationError(null)
    setPhase(report.phase === 'running' ? 'running' : 'setup')
    void invoke<AppInfo>('participant_app_info').then(setInfo)
      .catch((error: unknown) => setOperationError(`读取程序信息失败。${errorText(error)}`))
  }, [applySnapshot])

  useEffect(() => {
    let cancelled = false
    void invoke<BootReport>('participant_bootstrap')
      .then((report) => {
        if (!cancelled) applyBoot(report)
      })
      .catch((error: unknown) => {
        if (cancelled) return
        setPhase('setup')
        setSetupMessage(errorText(error))
      })
    return () => {
      cancelled = true
    }
  }, [applyBoot])

  useEffect(() => {
    if (phase !== 'running') return
    let cancelled = false
    const poll = () => {
      if (cancelled || busyRef.current || pollInFlight.current) return
      pollInFlight.current = true
      const startedGeneration = generation.current
      void pollParticipantOnce(
        () => invoke<ParticipantViewState>('participant_poll'), generation,
        () => !cancelled, applySnapshot,
      )
        .catch((error: unknown) => {
          if (!cancelled && pollMayApply(startedGeneration, generation.current)) {
            setControlError(`无法读取共同控制层，显示的状态已过期。程序会继续尝试恢复。${errorText(error)}`)
          }
        })
        .finally(() => {
          pollInFlight.current = false
        })
    }
    const timer = window.setInterval(poll, 1000)
    followUp.current = () => {
      for (const delay of FOLLOW_UP_POLLS_MS) window.setTimeout(poll, delay)
    }
    return () => {
      cancelled = true
      followUp.current = null
      window.clearInterval(timer)
    }
  }, [phase, applySnapshot])

  const request = useCallback(async (action: ParticipantAction) => {
    if (busyRef.current) throw new Error('正在应用上一项操作，请稍候。')
    generation.current += 1
    busyRef.current = true
    setBusy(true)
    setOperationError(null)
    try {
      const result = await invoke<CommandResult>('participant_command', { action })
      applySnapshot(result.state)
      if (!result.applied) {
        throw new Error(result.message)
      }
      return result
    } catch (error: unknown) {
      setOperationError(errorText(error))
      throw error
    } finally {
      busyRef.current = false
      setBusy(false)
      followUp.current?.()
    }
  }, [applySnapshot])

  const command = useCallback(async (action: ParticipantAction): Promise<void> => {
    await request(action)
  }, [request])

  const createConfig = useCallback(
    async (input: SetupInput) => {
      if (busyRef.current) throw new Error('本机配置正在启动，请稍候。')
      generation.current += 1
      busyRef.current = true
      setBusy(true)
      try {
        const report = await invoke<BootReport>('participant_setup', {
          request: {
            name: input.name,
            bind: input.bind,
            existingPath: input.existingPath,
            newPath: input.newPath ?? null,
          },
        })
        applyBoot(report)
      } finally {
        busyRef.current = false
        setBusy(false)
      }
    },
    [applyBoot],
  )

  const showDetails = useCallback(async () => {
    setOperationError(null)
    try {
      const text = await invoke<string>('participant_details')
      setDetails(text)
    } catch (error: unknown) {
      setOperationError(`读取诊断摘要失败。${errorText(error)}`)
    }
  }, [])

  const cold = useCallback(async <T>(work: () => Promise<T>): Promise<T> => {
    if (busyRef.current) throw new Error('当前操作尚未结束，请稍候。')
    generation.current += 1
    busyRef.current = true
    setBusy(true)
    setOperationError(null)
    try { return await work() }
    catch (error: unknown) { setOperationError(errorText(error)); throw error }
    finally { busyRef.current = false; setBusy(false); followUp.current?.() }
  }, [])

  const pickFile = useCallback((kind: FilePurpose) => cold(() => invoke<string | null>('participant_pick_file', { kind })), [cold])
  const previewSettings = useCallback((input: string, destination: string | null, mode: ImportMode) => cold(() =>
    invoke<BundlePreview>('participant_settings_preview', { input, destination, mode })), [cold])
  const pendingChanges = useCallback(() => cold(() => invoke<UnsavedChange[]>('participant_pending_settings')), [cold])
  const importSettings = useCallback((input: string, destination: string, options: ImportOptions, discardUnsaved: boolean) => cold(async () => {
    const report = await invoke<SettingsTransition>('participant_import_settings', { input, destination, options, discardUnsaved })
    applyBoot(report.boot)
    return report
  }), [cold, applyBoot])
  const switchConfig = useCallback((path: string, discardUnsaved: boolean) => cold(async () => {
    const report = await invoke<BootReport>('participant_switch_config', { path, discardUnsaved })
    applyBoot(report)
    return report
  }), [cold, applyBoot])

  const quit = useCallback(async () => {
    if (busyRef.current) {
      setOperationError('当前操作尚未结束，请稍后再退出。')
      return
    }
    generation.current += 1
    busyRef.current = true
    setBusy(true)
    setOperationError(null)
    try {
      await invoke('participant_quit')
    } catch (error: unknown) {
      setOperationError(`退出请求未完成，程序仍在运行。${errorText(error)}`)
    } finally {
      busyRef.current = false
      setBusy(false)
      followUp.current?.()
    }
  }, [])

  const visibleState: ParticipantViewState = {
    ...state,
    engine: controlError === null ? state.engine : 'error',
    snapshotStatus: controlError === null ? 'fresh' : 'stale',
    lastSnapshotAt,
    controlError,
    message: [controlError, operationError, state.message].filter(Boolean).join(' ') || null,
  }

  return {
    phase,
    state: visibleState,
    configPath,
    info: info ? { ...info, configPath } : null,
    setupMessage,
    busy,
    details,
    command,
    request,
    pickFile,
    previewSettings,
    pendingChanges,
    importSettings,
    switchConfig,
    createConfig,
    showDetails,
    hideDetails: () => setDetails(null),
    quit,
  }
}

export function createRoomAction(profile: RoomProfileInput, bind: string): ParticipantAction {
  return { type: 'create_room', profile, bind }
}

export function createSavedRoomAction(profilePath: string, bind: string): ParticipantAction {
  return { type: 'create_saved_room', profilePath, bind }
}

export function joinSavedRoomAction(profilePath: string, mode: JoinModeName): ParticipantAction {
  return { type: 'join_saved_room', profilePath, mode }
}

export function joinUnknownRoomAction(target: RoomTargetInput): ParticipantAction {
  return { type: 'join_unknown_room', target }
}

export function chooseRoomAction(generation: string, instanceId: string): ParticipantAction {
  return { type: 'choose_room', generation, instanceId }
}

export function leaveRoomAction(): ParticipantAction {
  return { type: 'leave_room' }
}

export function cancelRoomAction(): ParticipantAction {
  return { type: 'cancel_room' }
}

export function reloadRoomRulesAction(): ParticipantAction {
  return { type: 'reload_room_rules' }
}

export function startVirtualReturnAction(): ParticipantAction {
  return { type: 'start_virtual_return' }
}

export function stopVirtualReturnAction(): ParticipantAction {
  return { type: 'stop_virtual_return' }
}

export function refreshRoomsAction(): ParticipantAction {
  return { type: 'refresh_rooms' }
}

export function setLoginStartupAction(enabled: boolean): ParticipantAction {
  return { type: 'set_login_startup', enabled }
}

export function refreshLoginStartupAction(): ParticipantAction {
  return { type: 'refresh_login_startup' }
}

export function allowLanFirewallAction(): ParticipantAction {
  return { type: 'allow_lan_firewall' }
}

export function refreshLanFirewallAction(): ParticipantAction {
  return { type: 'refresh_lan_firewall' }
}

export function removeOwnLanFirewallAction(): ParticipantAction {
  return { type: 'remove_own_lan_firewall' }
}

export function editRoomRuleAction(rule: RoomRuleInput): ParticipantAction {
  return { type: 'edit_room_rule', rule }
}

/** One member-row direction. The other direction is omitted, not set false. */
export function editMemberDirectionAction(
  match: RuleMatchInput,
  direction: 'send' | 'listen',
  enabled: boolean,
): ParticipantAction {
  if (direction === 'send') {
    return { type: 'edit_room_rule', rule: { match, send: enabled } }
  }
  return { type: 'edit_room_rule', rule: { match, listen: enabled } }
}

export function replaceGlobalRulesAction(rules: CoreGlobalRules): ParticipantAction {
  return { type: 'replace_global_rules', rules }
}

export function setRoomSourceMixAction(
  condition: RuleMatchInput,
  sourceKind: SourceKind,
  patch: CoreMixPatch,
): ParticipantAction {
  return {
    type: 'set_room_source_mix',
    condition,
    sourceKind,
    ...(patch.gain_db === undefined ? {} : { gainDb: patch.gain_db }),
    ...(patch.pan === undefined ? {} : { pan: patch.pan }),
    ...(patch.mute === undefined ? {} : { mute: patch.mute }),
    ...(patch.solo === undefined ? {} : { solo: patch.solo }),
  }
}

export function errorText(error: unknown): string {
  if (error instanceof Error && error.message.trim()) return error.message.trim()
  if (typeof error === 'string' && error.trim()) return error.trim()
  return '这次操作没有完成。'
}
