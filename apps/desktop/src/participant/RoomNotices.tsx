import { useState } from 'react'
import { reloadRoomRulesAction } from './bridge.ts'
import { actionErrorMessage } from './labels.ts'
import { ruleFileNeedsAttention } from './memberModel.ts'
import { FieldError } from './primitives.tsx'
import type { ParticipantAction, RoomSessionView } from './types.ts'

export function RoomNotices({
  session,
  joined,
  disabled,
  onAction,
}: {
  session: RoomSessionView | null
  joined: boolean
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const [message, setMessage] = useState<string | null>(null)
  if (session === null) return null
  const contactLost = joined && session.phase === 'contact_unavailable'
  const files = [
    { kind: 'room', label: '本房间规则', status: session.ruleFiles.room },
    { kind: 'global', label: '全局规则', status: session.ruleFiles.global },
  ].filter((file) => ruleFileNeedsAttention(file.status))
  const unsaved = files.length > 0
  if (!contactLost && !unsaved && !session.pendingTargetChange && message === null) return null
  const hasExistingFileChanges = files.some((file) => file.kind !== 'room' || session.profilePath !== null)

  function reload() {
    if (disabled) return
    setMessage(null)
    void onAction(reloadRoomRulesAction()).catch((error: unknown) => {
      setMessage(actionErrorMessage(error))
    })
  }

  return (
    <div className="room-notices">
      {contactLost ? (
        <div className="room-notice room-notice--warning" role="status">
          <strong>联络服务暂时不可用</strong>
          <p>已经建立的声音继续。新加入的设备暂时看不到，程序正在后台重新联系。</p>
        </div>
      ) : null}
      {unsaved ? (
        <div className="room-notice room-notice--warning" role="alert">
          {files.map(({ kind, label, status }) => {
            const cardPending = kind === 'room' && session.profilePath === null
            const reason = status.state === 'external_change'
              ? '文件已在软件外修改，原文件未被覆盖。当前修改仍然生效。'
              : status.state === 'write_failed'
                ? '写入失败，当前修改仍然生效。'
                : '当前修改已经生效，尚未写入文件。'
            return (
              <div key={kind}>
                <strong>{cardPending ? '房间卡片还没有保存成功' : `${label}尚未写入`}</strong>
                <p>{cardPending
                  ? '本房间的规则暂时只在内存中生效。处理好文件位置的问题后，重新读取会再次尝试保存。'
                  : reason}</p>
                {cardPending && status.state !== 'pending' ? <p>{reason}</p> : null}
                {status.path ? <p className="readout">{status.path}</p> : null}
                {'detail' in status && status.detail ? (
                  <details className="details">
                    <summary>{label}详细信息</summary>
                    <p className="readout">{status.detail}</p>
                  </details>
                ) : null}
              </div>
            )
          })}
          {hasExistingFileChanges ? (
            <p>重新读取会改用已有文件中的规则，并放弃对应的未写入修改。</p>
          ) : null}
          <div className="room-actions">
            <button className="button button--secondary" type="button" disabled={disabled} onClick={reload}>
              重新读取规则文件
            </button>
          </div>
        </div>
      ) : null}
      {session.pendingTargetChange ? (
        <div className="room-notice room-notice--info" role="status">
          <p>房间文件里的房间名称、地址或密码已经修改，下次进入房间时生效。</p>
        </div>
      ) : null}
      <FieldError id="room-notice-error" message={message} />
    </div>
  )
}
