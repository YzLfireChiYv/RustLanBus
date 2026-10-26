import type {
  CaptureState,
  ContactState,
  EngineState,
  GroupContact,
  PcmFormatView,
  SourceKind,
  SourcePlayoutView,
} from './types.ts'

export function formatPcmFormat(format: PcmFormatView): string {
  const layout = format.channels === 1 ? '单声道'
    : format.channels === 2 ? '双声道'
      : format.channelMask === 0x3f ? '5.1（背环绕）'
        : format.channelMask === 0x60f ? '5.1（侧环绕）'
          : format.channelMask === 0x63f ? '7.1 环绕'
            : format.channelMask === 0xff ? '7.1（宽前置）' : `${format.channels} 声道`
  const encoding = { f32_le: '32 位浮点', signed32_le: '32 位整数', f64_le: '64 位浮点' }[format.encoding]
  return `${format.sampleRateHz / 1000} kHz · ${layout} · ${encoding}，有效 ${format.validBits} 位`
}

export const engineLabel: Record<EngineState, string> = {
  unavailable: '尚未连接',
  starting: '正在启动',
  running: '运行中',
  error: '出现问题',
}

export const contactLabel: Record<ContactState, string> = {
  offline: '离线',
  contacting: '正在联系',
  connected: '已接通',
  reconnecting: '正在重连',
}

export const groupContactLabel: Record<GroupContact, string> = {
  offline: '离线',
  contacting: '正在联系',
  connected: '已接通',
}

export const captureLabel: Record<CaptureState, string> = {
  disabled: '未启用',
  idle: '等待接听或本机监听，联系保持',
  starting: '正在准备',
  ready: '可用',
  missing: '找不到设备',
  unsupported: '当前设备不支持',
  error: '采集出错',
}

export const sourceKindLabel: Record<SourceKind, string> = {
  system: '系统声音',
  microphone: '麦克风',
}

export function outputPhaseLabel(phase: string): string {
  switch (phase) {
    case 'running': return '设备运行中'
    case 'idle': return '空闲'
    case 'starting': return '正在准备'
    case 'awaiting-device': return '等待所选设备'
    case 'disabled': return '未启用'
    case 'stopped': return '已停止'
    case 'unavailable': return '暂不可用'
    case 'error': return '运行出错'
    default: return phase || '尚无运行状态'
  }
}

export function sourcePlayoutLabel(phase: SourcePlayoutView['phase']): string {
  switch (phase) {
    case 'awaiting_output': return '等待输出设备'
    case 'unsupported': return '此输出不支持该来源'
    case 'muted': return '本机混音已静音'
    case 'priming': return '正在准备当前声音'
    case 'awaiting_current': return '等待当前音频'
    case 'running': return '本机输出已接入'
  }
}

export function arrivalAgeText(ageMs: number | null): string {
  if (ageMs === null) return '尚无音频到达记录'
  if (!Number.isFinite(ageMs) || ageMs < 0) return '音频到达时间未知'
  return ageMs < 1000 ? `距最近音频 ${Math.round(ageMs)} ms` : `距最近音频 ${(ageMs / 1000).toFixed(1)} 秒`
}

export function formatGainDb(value: number): string {
  const rounded = Math.round(value * 10) / 10
  const body = rounded.toFixed(1)
  return `${rounded > 0 ? '+' : ''}${body} dB`
}

export function formatPan(value: number): string {
  if (value <= -0.05) return `左 ${Math.abs(value).toFixed(2)}`
  if (value >= 0.05) return `右 ${value.toFixed(2)}`
  return '居中'
}

export function controlDomId(
  prefix: string,
  id: string,
  ...additionalIds: string[]
): string {
  // Fixed-width UTF-16 units preserve opaque IDs; separators preserve tuple parts.
  const segments = [id, ...additionalIds].map((value) =>
    value
      .split('')
      .map((unit) => unit.charCodeAt(0).toString(16).padStart(4, '0'))
      .join(''),
  )
  return `${prefix}-${segments.join('-')}`
}

export function actionErrorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) {
    return error.message.trim()
  }
  if (typeof error === 'string' && error.trim()) return error.trim()
  return '这次操作没有完成。'
}
