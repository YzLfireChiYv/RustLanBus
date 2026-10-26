import type { ReactNode } from 'react'

export type SourceKind = 'system' | 'microphone'

export type NetworkPolicy = { mode: 'auto' } | { mode: 'interface'; name: string }
export interface NetworkInterfaceView {
  name: string
  display_name: string
  up: boolean
  loopback: boolean
  addresses: readonly string[]
  index: number | null
  ipv4_index?: number | null
  ipv6_index?: number | null
}
export interface NetworkStateView {
  policy: NetworkPolicy
  interfaces: readonly NetworkInterfaceView[]
  selection: { bind: string; interface: string | null; interface_index: number | null } | null
  selections?: readonly { bind: string; interface: string | null; interface_index: number | null }[]
  error: string | null
}

/** Persistent routing choice. A named endpoint never follows the main output. */
export type OutputTarget = 'main' | { endpoint: string }

export type CaptureState =
  | 'disabled'
  | 'idle'
  | 'starting'
  | 'ready'
  | 'missing'
  | 'unsupported'
  | 'error'

export type ContactState =
  | 'offline'
  | 'contacting'
  | 'connected'
  | 'reconnecting'

export type EngineState = 'unavailable' | 'starting' | 'running' | 'error'

export type EndpointDirection = 'capture' | 'render'

export type GroupContact = 'offline' | 'contacting' | 'connected'

export interface SourceView {
  id: string
  kind: SourceKind
  name: string
  available: boolean
  gainDb: number
  pan: number
  mute: boolean
  solo: boolean
  peakDb: number | null
}

export interface LocalSourceView extends SourceView {
  format?: PcmFormatView | null
  captureState: CaptureState
  availabilityReason: string
  publication: PublicationState
  publish: boolean
  monitor: boolean
  monitorReady: boolean
  monitorAvailabilityReason: string
  endpointId: string | null
  outputTargets: readonly OutputTarget[]
}

export type PublicationState = 'disabled' | 'unavailable' | 'active'

export interface PcmFormatView {
  sampleRateHz: number
  channels: number
  layout: 'mono' | 'stereo_left_right' | 'speakers'
  channelMask?: number
  encoding: 'f32_le' | 'signed32_le' | 'f64_le'
  sampleBytes: number
  validBits: number
}

export interface RemoteSourceView extends SourceView {
  publication: PublicationState
  publicationEpoch: string
  receiveAllowed: boolean
  receiveReady: boolean
  format: PcmFormatView | null
  availabilityReason: string
  outputTargets: readonly OutputTarget[]
}

export interface MemberView {
  id: string
  name: string
  contact: ContactState
  sendAllowed: boolean
  receiveAllowed: boolean
  sendReady: boolean
  sendingSourceIds: readonly string[]
  receiveReady: boolean
  sources: readonly RemoteSourceView[]
}

export interface EndpointView {
  id: string
  name: string
  direction: EndpointDirection
  available: boolean
  supported: boolean
  reason: string | null
}

export interface GroupView {
  name: string
  joined: boolean
  contact: GroupContact
}

export type RoomPhase =
  | 'idle'
  | 'resolving'
  | 'choosing'
  | 'connecting'
  | 'joined'
  | 'contact_unavailable'
  | 'leaving'
  | 'stopped'
  | 'failed'

export type JoinModeName = 'manual' | 'automatic'

/** Core rule selector. Absent `ip` or `name` is omitted, not JSON null. */
export type RuleMatchInput = 'all' | { ip?: string; name?: string }

/** Core room rule. Absent `send` or `listen` means that direction is unchanged. */
export interface RoomRuleInput {
  match: RuleMatchInput
  source_kind?: SourceKind
  send?: boolean
  listen?: boolean
  outputs?: readonly OutputTarget[]
}

/** Nested playback JSON keeps the core field names. */
export interface CoreMixIntent {
  gain_db: number
  pan: number
  mute: boolean
  solo: boolean
}

/** Only present fields change; the core rejects an empty patch. */
export type CoreMixPatch = Partial<CoreMixIntent>

export interface CorePlaybackRule {
  condition: RuleMatchInput
  source_kind: SourceKind
  playback: CoreMixPatch
}

export interface CoreGlobalRules {
  schema: number
  rules: readonly RoomRuleInput[]
}

export interface RoomTargetInput {
  name: string
  address: string | null
  port?: number
  password: string
  strict_auto_join: boolean
}

export interface RoomProfileInput {
  schema: number
  room: RoomTargetInput
  rules: readonly RoomRuleInput[]
  playback?: readonly CorePlaybackRule[]
}

export type ProjectedEndpointView =
  | { kind: 'ready'; address: string }
  | { kind: 'local_scope_required'; ip: string; port: number }

export interface DiscoveredRoomView {
  name: string
  instanceId: string
  candidates: readonly string[]
}

export interface RoomMemberView {
  memberId: string
  runId: string
  displayName: string
  endpoint: ProjectedEndpointView
  directoryVisible: boolean
}

export interface AssignmentOriginView {
  list: 'global' | 'room'
  index: number
}

export interface SourceDirectionsView {
  kind: SourceKind
  send: boolean
  listen: boolean
  sendFrom: AssignmentOriginView | null
  listenFrom: AssignmentOriginView | null
  outputs: readonly OutputTarget[]
  outputsFrom: AssignmentOriginView | null
}

export interface PeerRouteView {
  memberId: string
  runId: string
  name: string
  endpoint: ProjectedEndpointView
  directoryVisible: boolean
  send: boolean
  listen: boolean
  sendFrom: AssignmentOriginView | null
  listenFrom: AssignmentOriginView | null
  outputs: readonly OutputTarget[]
  outputsFrom: AssignmentOriginView | null
  sources: readonly SourceDirectionsView[]
}

export type RuleFileStatusView = { path: string | null } & (
  | { state: 'unloaded' | 'saved' | 'pending' }
  | { state: 'external_change' | 'write_failed'; detail: string }
)

export interface RoomCardView {
  path: string
  name: string | null
  enteredAddress: string | null
  port: number | null
  strictAutoJoin: boolean
  hasPassword: boolean
  ruleCount: number
  error: string | null
}

export interface LocalAdvertisementView {
  instanceId: string
  displayName: string
  port: number
}

export interface VirtualReturnMemberView {
  memberId: string
  runId: string
  displayName: string
}

/** Decimal text keeps original u64 correspondence exact across the bridge. */
export interface ReturnMappingView {
  sourceId: string
  epoch: string
  upstreamMemberId: string
  upstreamRunId: string
  upstreamSourceId: string
  upstreamEpoch: string
  inputFirstFrame: string
  outputFirstFrame: string
}

export interface VirtualReturnView {
  member: VirtualReturnMemberView | null
  mappings: readonly ReturnMappingView[]
  error: string | null
}

export interface RoomSessionView {
  generation: string
  phase: RoomPhase
  roomName: string | null
  instanceId: string | null
  localService: string | null
  profilePath: string | null
  discoveredRooms: readonly DiscoveredRoomView[]
  choices: readonly DiscoveredRoomView[]
  selectedEndpoints: readonly string[]
  members: readonly RoomMemberView[]
  globalRules: CoreGlobalRules | null
  rules: readonly RoomRuleInput[]
  playbackRules: readonly CorePlaybackRule[]
  rulesUnsaved: boolean
  rulesSaveError: string | null
  ruleFiles: { room: RuleFileStatusView; global: RuleFileStatusView }
  savedAddress: string | null
  cardPersisted: boolean | null
  pendingTargetChange: boolean
  failure: string | null
  failureKind: 'unavailable' | 'name_mismatch' | 'password_rejected' | 'protocol_mismatch' | 'contact_rejected' | 'configuration' | 'listener_failed' | 'discovery_failed' | 'address_required' | 'selection_expired' | null
  outputOverflow: boolean
  outputLossRevision: string
  discoveryFailure: string | null
  localAdvertisement: LocalAdvertisementView | null
  virtualReturn?: VirtualReturnView | null
  operationNotice?: string | null
}

export interface RoomControlView {
  session: RoomSessionView | null
  cards: readonly RoomCardView[]
  routes: readonly PeerRouteView[]
  globalRulesPath: string
  autoEnter: boolean
  error: string | null
  creationDefaults: { contactPort: number | null; listenAddress: string | null }
}

export interface ParticipantViewState {
  network?: NetworkStateView
  requestedBind?: string
  /** Every currently owned concrete UDP source address, including both families. */
  localMediaAddresses?: readonly string[]
  /** Client-side read health. Absence means an ordinary core snapshot. */
  snapshotStatus?: 'fresh' | 'stale'
  lastSnapshotAt?: string | null
  controlError?: string | null
  engine: EngineState
  message: string | null
  localName: string
  group: GroupView
  localSources: readonly LocalSourceView[]
  members: readonly MemberView[]
  endpoints: readonly EndpointView[]
  outputEndpointId: string | null
  mainOutputId: string | null
  outputDevices: readonly OutputDeviceView[]
  masterGainDb: number
  limiterEnabled: boolean
  outputPeakDb: number | null
  overloaded: boolean
  autoReconnect: boolean
  loginStartup: LoginStartupView
  lanFirewall: LanFirewallView
  room: RoomControlView
  returnLoop?: ReturnLoopView | null
}

export interface LoginStartupView {
  requested: boolean
  registered: boolean | null
  taskName: string | null
  executable: string | null
  lastError: string | null
}

export interface LanFirewallView {
  registered: boolean | null
  program: string | null
  lastError: string | null
}

/** Actual audio packet observations; no playback or one-way latency estimate. */
export interface ReturnLoopView {
  memberId: string
  runId: string
  returnedPackets: string
  packetRttMs: number | null
  minimumPacketRttMs: number | null
  captureToReceiveMs: number | null
  minimumCaptureToReceiveMs: number | null
}

/** Renderer observation for one source at one output. Counters remain decimal text. */
export interface SourcePlayoutView {
  memberId: string | null
  sourceId: string
  phase: 'awaiting_output' | 'unsupported' | 'muted' | 'priming' | 'awaiting_current' | 'running'
  lastArrivalAgeMs: number | null
  closedBefore: string
  highWater: string
  submittedFrames: string
  droppedBlocks: string
  effectiveGain: number
  segmentRestarts?: string
  lastError?: string | null
}

export interface SourceOutputErrorView {
  memberId: string | null
  sourceId: string
  detail: string
}

/** Native owner state per concrete output device. */
export interface OutputDeviceView {
  endpointId: string
  phase: string
  actualEndpointId: string | null
  generation: string
  rateHz: number | null
  channels: number | null
  lastError: string | null
  peakDb?: number | null
  overloaded?: boolean
  sources?: readonly SourcePlayoutView[]
  sourceErrors?: readonly SourceOutputErrorView[]
}

export type ParticipantAction =
  | { type: 'set_network'; policy: NetworkPolicy; bind: string | null }
  | { type: 'read_room_card'; path: string }
  | { type: 'save_room_card'; path: string; target: RoomTargetInput; expectedRevision: string }
  | { type: 'delete_room_card'; path: string; expectedRevision: string }
  | { type: 'export_settings'; path: string }
  | { type: 'set_name'; name: string }
  | { type: 'join_group'; name: string }
  | { type: 'leave_group' }
  | {
      type: 'set_peer_intent'
      memberId: string
      send: boolean
      receive: boolean
    }
  | {
      type: 'set_local_source'
      sourceId: string
      publish: boolean
      monitor: boolean
    }
  | { type: 'select_source_endpoint'; sourceId: string; endpointId: string }
  | { type: 'set_local_outputs'; sourceId: string; targets: readonly OutputTarget[] }
  | { type: 'select_output'; endpointId: string }
  | {
      type: 'set_source_mix'
      memberId: string | null
      sourceId: string
      gainDb: number
      pan: number
      mute: boolean
      solo: boolean
    }
  | { type: 'set_master'; gainDb: number; limiterEnabled: boolean }
  | { type: 'set_auto_reconnect'; enabled: boolean }
  | { type: 'set_login_startup'; enabled: boolean }
  | { type: 'refresh_login_startup' }
  | { type: 'allow_lan_firewall' }
  | { type: 'refresh_lan_firewall' }
  | { type: 'remove_own_lan_firewall' }
  | { type: 'refresh_devices' }
  | { type: 'create_room'; profile: RoomProfileInput; bind: string }
  | { type: 'create_saved_room'; profilePath: string; bind: string }
  | { type: 'join_saved_room'; profilePath: string; mode: JoinModeName }
  | { type: 'join_unknown_room'; target: RoomTargetInput }
  | { type: 'choose_room'; generation: string; instanceId: string }
  | { type: 'leave_room' }
  | { type: 'cancel_room' }
  | { type: 'reload_room_rules' }
  | { type: 'start_virtual_return' }
  | { type: 'stop_virtual_return' }
  | { type: 'refresh_rooms' }
  | { type: 'edit_room_rule'; rule: RoomRuleInput }
  | { type: 'replace_global_rules'; rules: CoreGlobalRules }
  | { type: 'set_global_defaults'; send?: boolean; listen?: boolean }
  | {
      type: 'set_room_source_mix'
      condition: RuleMatchInput
      sourceKind: SourceKind
      gainDb?: number
      pan?: number
      mute?: boolean
      solo?: boolean
    }

export interface ParticipantViewProps {
  state: ParticipantViewState
  onAction: (action: ParticipantAction) => Promise<void>
  busy: boolean
  onRequest?: (action: ParticipantAction) => Promise<{ management?: unknown }>
  extraSettings?: ReactNode
}
