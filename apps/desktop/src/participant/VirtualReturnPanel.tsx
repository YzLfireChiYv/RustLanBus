import { useState } from 'react'
import { startVirtualReturnAction, stopVirtualReturnAction } from './bridge.ts'
import { actionErrorMessage } from './labels.ts'
import { FieldError } from './primitives.tsx'
import type { ParticipantAction, ParticipantViewState } from './types.ts'

export function VirtualReturnPanel({ state, disabled, onAction }: {
  state: ParticipantViewState
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const [error, setError] = useState<string | null>(null)
  const session = state.room.session
  const returned = session?.virtualReturn
  const active = returned?.member != null
  const contactAvailable = session?.phase === 'joined'
  const timing = state.returnLoop

  async function operate() {
    if (disabled || !contactAvailable) return
    setError(null)
    try {
      await onAction(active ? stopVirtualReturnAction() : startVirtualReturnAction())
    } catch (failure: unknown) {
      setError(actionErrorMessage(failure))
    }
  }

  return (
    <div className="room-notices">
      <p>通过房间服务所在的设备，将本机原声返回本机。支持回传的 OpenWrt 服务会在成员列表中增加一个对应的回传设备。</p>
      {!active ? <p>开始后会允许向该回传名称发送并接听，并保存到本房间规则。麦克风仍由你手动开启，播放沿用已有输出选择。</p> : (
        <p role="status">回传设备：{returned.member!.displayName} · 已登记 {returned.mappings.length} 路来源。停止后保留已保存的收发规则。</p>
      )}
      {active && timing ? (
        <div role="status">
          <p>已对应收到 {timing.returnedPackets} 个音频包。</p>
          <p>音频包往返：{timing.packetRttMs == null ? '等待音频' : `${timing.packetRttMs.toFixed(2)} ms`}
            {timing.minimumPacketRttMs == null ? '' : ` · 最低 ${timing.minimumPacketRttMs.toFixed(2)} ms`}</p>
          <p>采集首帧到返回：{timing.captureToReceiveMs == null ? '暂无可用的采集时间' : `${timing.captureToReceiveMs.toFixed(2)} ms`}</p>
          <p>以上记录到本机收到回传为止，播放进度可在输出设备状态中查看。</p>
        </div>
      ) : null}
      <div className="room-actions">
        <button type="button" className="button button--secondary"
          disabled={disabled || !contactAvailable} onClick={() => { void operate() }}>
          {active ? '停止回传' : '开始回传检测'}
        </button>
      </div>
      {!contactAvailable ? <p>进入房间且联络服务可用后，可以开始或停止回传。</p> : null}
      {returned?.error ? (
        <div role="alert"><p>回传操作未成功。请确认房间服务已启用虚拟回传。</p>
          <details><summary>错误详情</summary><p className="readout">{returned.error}</p></details>
        </div>
      ) : null}
      <FieldError id="virtual-return-error" message={error} />
    </div>
  )
}
