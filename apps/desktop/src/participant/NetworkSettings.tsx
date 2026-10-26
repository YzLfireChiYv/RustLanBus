import { useState } from 'react'
import { actionErrorMessage } from './labels.ts'
import { FieldError } from './primitives.tsx'
import type { NetworkPolicy, ParticipantAction, ParticipantViewState } from './types.ts'

export function NetworkSettings({ state, busy, onAction }: {
  state: ParticipantViewState
  busy: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const network = state.network
  const current = network?.policy ?? { mode: 'auto' as const }
  const [draft, setDraft] = useState<NetworkPolicy | null>(null)
  const [bindDraft, setBind] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [pending, setPending] = useState(false)
  const selected = draft ?? current
  const requestedBind = bindDraft ?? state.requestedBind ?? '0.0.0.0:0'
  const interfaces = network?.interfaces ?? []
  const locked = busy || pending

  async function apply() {
    setError(null)
    if (selected.mode === 'interface' && !selected.name) {
      setError('请选择要严格使用的网卡。')
      return
    }
    if (!requestedBind.trim()) {
      setError('请填写完整的本机绑定地址；自动端口可填写 0.0.0.0:0。')
      return
    }
    setPending(true)
    try {
      await onAction({ type: 'set_network', policy: selected, bind: requestedBind.trim() })
      setDraft(null)
      setBind(null)
    } catch (cause) {
      setError(actionErrorMessage(cause))
    } finally { setPending(false) }
  }

  return <details open={Boolean(network?.error)}>
    <summary>网络与联系</summary>
    <section className="panel" aria-labelledby="network-heading">
      <h2 id="network-heading" className="panel__title">本机网络</h2>
      <div className="panel__body">
        <p className="field-hint">默认按可达路径联系成员，支持系统可用的 IPv4／IPv6。网络失败时，本机设置和监听仍可使用。</p>
        <label className="field">
          <span className="field__label">工作范围</span>
          <select className="select-input" disabled={locked} value={selected.mode}
            onChange={event => setDraft(event.target.value === 'auto' ? { mode: 'auto' } : { mode: 'interface', name: current.mode === 'interface' ? current.name : '' })}>
            <option value="auto">自动选择可达网络</option>
            <option value="interface">严格使用指定网卡</option>
          </select>
        </label>
        {selected.mode === 'interface' && <label className="field">
          <span className="field__label">指定网卡</span>
          <select className="select-input" value={selected.name} disabled={locked}
            onChange={event => setDraft({ mode: 'interface', name: event.target.value })}>
            <option value="">请选择</option>
            {selected.name && !interfaces.some(i => i.name === selected.name)
              ? <option value={selected.name}>等待恢复：{selected.name}</option> : null}
            {interfaces.map(item => <option key={item.name} value={item.name}>
              {item.display_name} · {item.up ? '可用' : '未连接'} · {item.addresses.join('、')}
            </option>)}
          </select>
          <span className="field-hint">该网卡不可用时会等待恢复，不转到其它网卡；网卡内的可用地址可以更新。</span>
        </label>}
        <details>
          <summary>高级绑定</summary>
          <label className="field">
            <span className="field__label">本机媒体绑定</span>
            <input className="text-input" value={requestedBind} spellCheck={false} disabled={locked}
              onChange={event => setBind(event.target.value)} />
          </label>
          <p className="field-hint">0 表示由系统分配端口。默认通配绑定使用可用的两种地址族；指定具体本机 IP 会限制为该地址。房间联络端口独立设置。</p>
          <button type="button" className="button button--secondary" disabled={locked} onClick={() => setBind('0.0.0.0:0')}>使用自动地址和端口</button>
        </details>
        <button className="button button--secondary" type="button" disabled={locked} onClick={() => void apply()}>应用网络设置</button>
        <p className="field-hint">更改工作网络会恢复本机对等联系；保持有效的音源、监听与输出选择。</p>
        <FieldError id="network-operation-error" message={error} />
        {network?.error ? <p role="status" className="field-error">网络当前状态：{network.error}</p> : null}
        {state.localMediaAddresses === undefined ? <p className="field-hint">正在获取本机媒体地址。</p>
          : state.localMediaAddresses.length > 0 ? <details>
            <summary>{state.snapshotStatus === 'stale' ? '上次获取的媒体地址' : '实际媒体地址'}</summary>
            {state.localMediaAddresses.map(address => <p className="readout" key={address}>{address}</p>)}
          </details> : <p className="field-hint">{state.snapshotStatus === 'stale'
            ? '状态暂未更新，上次获取时尚无媒体绑定。'
            : '媒体网络等待绑定；控制和数据设置仍可调整。'}</p>}
      </div>
    </section>
  </details>
}
