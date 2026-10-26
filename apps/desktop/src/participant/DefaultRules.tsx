import { useState } from 'react'
import { actionErrorMessage } from './labels.ts'
import { defaultRuleAction, defaultRuleView, type RuleDirection } from './memberModel.ts'
import { FieldError, ToggleField } from './primitives.tsx'
import type { ParticipantAction, RoomSessionView } from './types.ts'

export function DefaultRules({
  session,
  globalRulesPath,
  disabled,
  onAction,
}: {
  session: RoomSessionView | null
  globalRulesPath: string
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const [open, setOpen] = useState(false)
  const [message, setMessage] = useState<string | null>(null)
  const rules = session?.globalRules ?? null
  const view = defaultRuleView(rules)
  function commit(direction: RuleDirection, enabled: boolean) {
    if (disabled || rules === null) return
    const action = defaultRuleAction(rules, direction, enabled)
    if (action === null) return
    setMessage(null)
    void onAction(action).catch((error: unknown) => {
      setMessage(actionErrorMessage(error))
    })
  }

  const summary =
    view === null
      ? rules === null
        ? '全局规则尚未载入'
        : '全局规则文件使用了自定义顺序'
      : `新成员：${view.send ? '发送' : '不发送'} · ${view.listen ? '自动收听' : '不自动收听'}`
  const locked = disabled || view === null

  return (
    <>
      <span className="default-rules__summary">{summary}</span>
      <button
        type="button"
        className="button button--quiet"
        aria-expanded={open}
        aria-controls="default-rules-panel"
        onClick={() => setOpen(!open)}
      >
        {open ? '收起默认规则' : '默认规则'}
      </button>
      {open ? (
        <div className="default-rules__panel" id="default-rules-panel">
          <p className="field-hint">
            新成员默认对应全局规则第 1 条。全局规则在每个房间先生效；在房间里切换过的开关保存在本房间规则里，随后生效。
          </p>
          <div className="default-rules__switches">
            <ToggleField
              id="default-send"
              label="发送给对方"
              checked={view?.send ?? false}
              disabled={locked}
              onCommit={(next) => commit('send', next)}
            />
            <ToggleField
              id="default-listen"
              label="收听"
              checked={view?.listen ?? false}
              disabled={locked}
              onCommit={(next) => commit('listen', next)}
            />
          </div>
          {view === null && rules !== null ? (
            <p className="field-hint">全局规则第 1 条没有针对全部成员和全部来源，请直接编辑全局规则文件。</p>
          ) : null}
          {view !== null && view.additional > 0 ? (
            <p className="field-hint">
              另有 {view.additional} 条指定成员的全局规则，请在全局规则文件中编辑；保存文件后点「重新读取规则文件」生效。
            </p>
          ) : null}
          {globalRulesPath ? <p className="readout default-rules__path">{globalRulesPath}</p> : null}
          <FieldError id="default-rules-error" message={message} />
        </div>
      ) : null}
    </>
  )
}
