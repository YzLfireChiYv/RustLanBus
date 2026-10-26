import { useState } from 'react'
import { reloadRoomRulesAction } from './bridge.ts'
import { actionErrorMessage } from './labels.ts'
import { assignmentText, matchText, ruleFileNeedsAttention } from './memberModel.ts'
import { FieldError } from './primitives.tsx'
import type { ParticipantAction, RoomSessionView } from './types.ts'

export function RoomRules({
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
  if (session === null || !joined) return null
  const rules = session.rules
  const unsaved = ruleFileNeedsAttention(session.ruleFiles.room)
  const reloadShownInNotice = unsaved || ruleFileNeedsAttention(session.ruleFiles.global)

  function reload() {
    if (disabled) return
    setMessage(null)
    void onAction(reloadRoomRulesAction()).catch((error: unknown) => {
      setMessage(actionErrorMessage(error))
    })
  }

  return (
    <details className="details room-rules">
      <summary>
        本房间规则 {rules.length} 条{unsaved ? '，尚未写入文件' : ''}
      </summary>
      <div className="details__body">
        {rules.length === 0 ? (
          <p className="field-hint">本房间还没有规则。在成员上切换过的开关会按顺序保存在这里。</p>
        ) : (
          <ol className="rule-list">
            {rules.map((rule, index) => (
              <li key={`${index}:${JSON.stringify(rule)}`}>
                {matchText(rule.match)}：{assignmentText(rule)}
              </li>
            ))}
          </ol>
        )}
        <p className="field-hint">
          全局规则先生效，本房间规则随后生效；同一方向以排在后面的规则为准。两个规则文件都可以直接编辑，保存后点「重新读取规则文件」。
        </p>
        {session.profilePath ? <p className="readout room-rules__path">{session.profilePath}</p> : null}
        {reloadShownInNotice ? null : (
          <div className="room-actions">
            <button className="button button--secondary" type="button" disabled={disabled} onClick={reload}>
              重新读取规则文件
            </button>
          </div>
        )}
        <FieldError id="room-rules-error" message={message} />
      </div>
    </details>
  )
}
