import { useState } from 'react'
import { actionErrorMessage } from './labels.ts'
import { FieldError, ToggleField } from './primitives.tsx'
import type { ParticipantAction, ParticipantViewState } from './types.ts'

export function WindowsSettings({ state, disabled, onAction }: {
  state: ParticipantViewState
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const [error, setError] = useState<string | null>(null)
  const [firewallError, setFirewallError] = useState<string | null>(null)
  const startup = state.loginStartup
  const firewall = state.lanFirewall
  const firewallDetail = firewallError ?? firewall.lastError
  async function commit(action: ParticipantAction, scope: 'startup' | 'firewall' = 'startup') {
    const setFailure = scope === 'firewall' ? setFirewallError : setError
    setFailure(null)
    try { await onAction(action) } catch (cause) { setFailure(actionErrorMessage(cause)) }
  }
  return (
    <section className="panel" aria-labelledby="windows-settings-heading">
      <h2 className="panel__title" id="windows-settings-heading">Windows 设置</h2>
      <div className="panel__body">
        <ToggleField
          id="windows-login-startup"
          label="登录 Windows 后自动启动"
          checked={startup.requested}
          disabled={disabled}
          describedBy="windows-login-startup-error"
          onCommit={(enabled) => void commit({ type: 'set_login_startup', enabled })}
        />
        <p className="field-hint">默认关闭。开启后以管理员权限启动，并沿用当前配置和已保存的入房选择。</p>
        <p className="readout" role="status">
          {startup.registered === null ? '尚未确认 Windows 自启登记状态。'
            : startup.registered ? 'Windows 已登记登录自启。' : 'Windows 当前没有有效的登录自启登记。'}
        </p>
        <FieldError id="windows-login-startup-error" message={error ?? startup.lastError} />
        <button className="button button--secondary" type="button" disabled={disabled}
          onClick={() => void commit({ type: 'refresh_login_startup' })}>刷新登记状态</button>
        {(startup.requested || startup.lastError) && <button className="button button--secondary" type="button" disabled={disabled}
          onClick={() => void commit({ type: 'set_login_startup', enabled: true })}>用当前程序重新登记</button>}
        {(startup.registered !== false || startup.lastError) && <button className="button button--secondary" type="button" disabled={disabled}
          onClick={() => void commit({ type: 'set_login_startup', enabled: false })}>撤销此配置的自启登记</button>}
        <h3 className="panel__title">局域网访问</h3>
        <p className="field-hint">由你点击后，为当前程序添加本地子网入站 TCP／UDP 规则，覆盖动态音频与房间端口。沿用现有出站策略，连接仍取决于其它防火墙及网络设置。</p>
        <p className="readout" role="status">
          {firewall.registered === null ? '尚未确认此程序的局域网规则。'
            : firewall.registered ? '此程序的两条局域网入站规则已登记。' : '此程序尚无完整、匹配的局域网入站规则。'}
        </p>
        <FieldError id="windows-firewall-error" message={firewallDetail ? '局域网规则操作未能确认，请查看详情。' : null} />
        <div className="room-actions">
          <button className="button" type="button" disabled={disabled}
            onClick={() => void commit({ type: 'allow_lan_firewall' }, 'firewall')}>允许本程序的局域网连接</button>
          <button className="button button--secondary" type="button" disabled={disabled}
            onClick={() => void commit({ type: 'refresh_lan_firewall' }, 'firewall')}>刷新规则状态</button>
          <button className="button button--secondary" type="button" disabled={disabled}
            onClick={() => void commit({ type: 'remove_own_lan_firewall' }, 'firewall')}>撤销此程序的规则</button>
        </div>
        {(firewall.program || firewallDetail) && <details>
          <summary>局域网规则详情</summary>
          {firewall.program && <p className="readout">当前程序：{firewall.program}</p>}
          {firewallDetail && <p className="readout">{firewallDetail}</p>}
        </details>}
      </div>
    </section>
  )
}
