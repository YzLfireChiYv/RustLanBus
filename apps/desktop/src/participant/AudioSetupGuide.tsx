import { invoke } from '@tauri-apps/api/core'
import { useRef, useState } from 'react'
import { actionErrorMessage } from './labels.ts'
import { FieldError } from './primitives.tsx'
import type { ParticipantAction, ParticipantViewState } from './types.ts'

export function AudioSetupGuide({ state, disabled, onAction }: {
  state: ParticipantViewState
  disabled: boolean
  onAction: (action: ParticipantAction) => Promise<void>
}) {
  const system = state.localSources.find((source) => source.kind === 'system')
  const [error, setError] = useState<string | null>(null)
  const [openingReference, setOpeningReference] = useState(false)
  const referenceBusy = useRef(false)
  const endpointName = (id: string | null | undefined) =>
    state.endpoints.find((endpoint) => endpoint.id === id)?.name ?? id ?? '尚未选择'
  const mainOutputId = state.mainOutputId ?? state.outputEndpointId
  const outputReady = mainOutputId !== null && state.outputDevices.some((output) =>
    output.phase === 'running' && output.actualEndpointId === mainOutputId)
  const configured = Boolean(system?.endpointId && mainOutputId)
  const virtualCandidates = state.endpoints.filter((endpoint) => endpoint.direction === 'render'
    && /fxsound|vb[- ]?audio|vb[- ]?cable|cable input|voicemeeter|virtual|虚拟/i.test(endpoint.name))
  const sourceStatus = !system?.endpointId ? '尚未选择采集设备'
    : system.captureState === 'idle' ? '按需待机，接听或本机监听时启用'
    : system.captureState === 'disabled' ? '已关闭，可在来源卡片开启发布或监听'
    : system.available ? '来源可用' : system.availabilityReason || '等待来源就绪'

  async function openReference(key: string) {
    if (referenceBusy.current) return
    referenceBusy.current = true
    setOpeningReference(true)
    setError(null)
    try { await invoke('participant_open_reference', { key }) } catch (cause) {
      setError(actionErrorMessage(cause))
    } finally {
      referenceBusy.current = false
      setOpeningReference(false)
    }
  }

  function showOutput() {
    document.getElementById('master-heading')?.closest('details')?.setAttribute('open', '')
  }

  return (
    <details open={!configured}>
      <summary>声音接入指南</summary>
      <section className="panel" aria-labelledby="audio-guide-heading">
        <h2 className="panel__title" id="audio-guide-heading">先接入声音，再选择收听成员</h2>
        <div className="panel__body">
          {state.snapshotStatus === 'stale' ? <p className="field-error" role="status">下面是最后读取的声音配置，正在等待控制联系恢复。</p> : null}
          <ol>
            <li>选择一个独立的虚拟<strong>播放</strong>设备。已安装且适合这条路由的设备可以继续使用；没有时展开下方 FxSound 可选安装路线。</li>
            <li>退出会另行播放该来源的配套处理／转发程序，并关闭其自启。之后再到 Windows 声音设置，把需要共享的应用或系统默认输出设为虚拟播放设备。</li>
            <li>先展开“输出”，在“主输出设备”明确选择物理耳机或音箱；再到“本机声音和设备”，将系统声音的“采集设备”选为虚拟播放设备。Windows 默认已经是虚拟设备时，主输出需要明确选物理设备。</li>
            <li>系统声音的“发布”默认开启，麦克风默认关闭。需要在本机听到自己的原声时，单独开启该来源的“本机监听”。</li>
            <li>创建或加入同一房间，在耳机电脑点击要听的成员“接听”。它会接听该成员当前及以后开启的来源，展开后可逐路关闭。</li>
          </ol>
          <p className="field-hint">本机原声静音依靠“应用送虚拟端点、外部转发关闭”这条路由。本机监听独立于房间；没有接收者也没有监听时按需待机，联系和保活继续。</p>
          {virtualCandidates.length > 0 ? <details>
            <summary>按名称找到的可能虚拟播放设备（{virtualCandidates.length}）</summary>
            <p className="field-hint">名称识别用于定位，不会替你选设备。请确认它是独立的虚拟播放端点。</p>
            <ul>{virtualCandidates.map((endpoint) => <li key={endpoint.id}>
              {endpoint.name} · {!endpoint.available ? '目前不可用' : !endpoint.supported ? '当前格式不支持' : endpoint.id === mainOutputId ? '先将主输出改为独立物理设备' : '可在采集设备中选择'}
            </li>)}</ul>
          </details> : <p className="field-hint">没有按名称识别到虚拟播放设备。其它名称的已装设备仍可手动选择；尚无设备时可使用下面的官方安装入口。</p>}

          <details>
            <summary>尚无虚拟设备：FxSound 可选安装路线</summary>
            <ol>
              <li>查看 FxSound 官方项目和下载页面，选择适合这台 Windows 架构的官方安装器，按其向导安装。它提供 FxSound Audio Enhancer 虚拟设备；RustLanBus 不代为下载或安装。</li>
              <li>在 Windows“设置 → 应用 → 启动”或任务管理器的启动应用列表关闭 FxSound 自启，并从 FxSound 的菜单／托盘明确退出应用。</li>
              <li><strong>退出之后</strong>，再把 Windows 系统默认或指定应用的输出设置为 FxSound 虚拟播放端点。FxSound 退出会恢复默认播放设备，先前设置可能因此被改回。</li>
              <li>让 FxSound 的处理／转发应用保持退出，然后刷新本软件的设备列表，按上面的顺序选择物理主输出和虚拟采集来源。</li>
            </ol>
            <div className="room-actions">
              <button className="button button--secondary" type="button" disabled={openingReference} onClick={() => void openReference('fxsound_project')}>FxSound 官方项目</button>
              <button className="button button--secondary" type="button" disabled={openingReference} onClick={() => void openReference('fxsound_download')}>FxSound 官方下载页面</button>
            </div>
            <p className="field-hint">FxSound 是可选候选。已装的其它合适虚拟播放设备可以继续使用，可用性和实际格式会在来源详情显示。</p>
          </details>

          <details>
            <summary>Windows 应用输出与全部系统声音</summary>
            <p>只共享部分应用：让该应用开始播放，在“设置 → 系统 → 声音 → 音量合成器”的应用列表展开它，将输出设备改为虚拟播放端点。Windows 10 的相关入口可能称为“应用音量和设备首选项”。应用自身另有设备选项时，也要选相同端点。</p>
            <p>共享跟随默认输出的声音：在 Windows“声音”的输出设备中选择虚拟播放端点。已有单独输出设置的应用需要另行确认；RustLanBus 的主输出仍明确选物理耳机／音箱。</p>
            <p className="field-hint">要共享的应用和虚拟播放设备保持非静音。物理设备上的原声安静依靠路由分离，本机音量由所选输出和本机监听控制。</p>
            <p>关闭会把所选来源另行播放到物理设备的外部监听链路，包括虚拟线录音端或麦克风属性中已有的“侦听此设备”。驱动所需组件按其说明保留；本机原声监听由本软件单独控制。</p>
            <div className="room-actions">
              <button className="button button--secondary" type="button" disabled={openingReference} onClick={() => void openReference('windows_volume_mixer')}>打开 Windows 音量合成器</button>
              <button className="button button--secondary" type="button" disabled={openingReference} onClick={() => void openReference('windows_sound_settings')}>打开 Windows 声音设置</button>
              <button className="button button--secondary" type="button" disabled={openingReference} onClick={() => void openReference('windows_audio_guide')}>微软应用音频说明</button>
            </div>
          </details>
          <p className="field-hint">实际采样率、有效精度和声道布局从所选设备读取，在来源详情显示；某个虚拟设备的范围不成为程序的全局 48 kHz／24-bit 限制。多声道保留原布局传输，在各输出按支持的微软矩阵适配。</p>
          <p className="readout">系统声来源：{endpointName(system?.endpointId)} · {sourceStatus}</p>
          <p className="readout">主输出：{endpointName(mainOutputId)} · {outputReady ? '已就绪' : '等待输出就绪'}</p>
          <div className="room-actions">
            <a className="button button--secondary" href="#master-heading" onClick={showOutput}>先选择物理主输出</a>
            <a className="button button--secondary" href="#local-sources-heading" onClick={() => {
              document.getElementById('local-audio-settings')?.setAttribute('open', '')
            }}>配置声音</a>
            <button className="button button--secondary" type="button" disabled={disabled}
              onClick={() => { setError(null); void onAction({ type: 'refresh_devices' }).catch((cause: unknown) => setError(actionErrorMessage(cause))) }}>刷新设备</button>
          </div>
          <FieldError id="audio-guide-error" message={error} />
          <p className="field-hint">外部资料由系统浏览器打开，声音设置在 Windows 自己的窗口打开。</p>
        </div>
      </section>
    </details>
  )
}
