import assert from 'node:assert/strict'
import { readFileSync } from 'node:fs'
import { registerHooks } from 'node:module'
import test from 'node:test'
import ts from 'typescript'
import { createElement } from 'react'
import { renderToStaticMarkup } from 'react-dom/server'

// Render the real components without a browser, Tauri process or audio device.
registerHooks({
  load(url, context, nextLoad) {
    if (url.endsWith('.tsx')) {
      return {
        format: 'module', shortCircuit: true,
        source: ts.transpileModule(readFileSync(new URL(url), 'utf8'), {
          compilerOptions: { jsx: ts.JsxEmit.ReactJSX, module: ts.ModuleKind.ESNext },
        }).outputText,
      }
    }
    if (url.endsWith('.css')) return { format: 'module', shortCircuit: true, source: '' }
    return nextLoad(url, context)
  },
})

const { GroupPanel } = await import('../src/participant/GroupPanel.tsx')
const { SetupPanel } = await import('../src/participant/SetupPanel.tsx')
const { MemberList } = await import('../src/participant/MemberList.tsx')
const { MemberDetail } = await import('../src/participant/MemberDetail.tsx')
const { LocalSources } = await import('../src/participant/LocalSources.tsx')
const { MasterStrip } = await import('../src/participant/MasterStrip.tsx')
const { groupMembers } = await import('../src/participant/memberModel.ts')
const { unavailableParticipantState } = await import('../src/participant/state.ts')

test('device selection prevents feedback through every selected output including a disconnected explicit route', () => {
  const state = {
    ...unavailableParticipantState,
    localSources: [{
      id: 'system', kind: 'system', name: '系统声音', available: false,
      gainDb: 0, pan: 0, mute: false, solo: false, peakDb: null,
      captureState: 'missing', availabilityReason: '', publication: 'unavailable',
      publish: true, monitor: false, monitorReady: false, monitorAvailabilityReason: '',
      endpointId: 'system-source', outputTargets: ['main'],
    }],
    endpoints: ['system-source', 'remote-output'].map((id) => ({
      id, name: id, direction: 'render', supported: true, available: true, reason: null,
    })),
    room: { ...unavailableParticipantState.room, routes: [{
      outputs: [], sources: [{ outputs: [{ endpoint: 'remote-output' }] }],
    }] },
  }
  const props = { state, disabled: false, onAction: async () => {} }
  const local = renderToStaticMarkup(createElement(LocalSources, props))
  assert.match(local, /<option\b(?=[^>]*\bdisabled="")(?=[^>]*\bvalue="remote-output")[^>]*>/u)
  const main = renderToStaticMarkup(createElement(MasterStrip, props))
  assert.match(main, /<option\b(?=[^>]*\bdisabled="")(?=[^>]*\bvalue="system-source")[^>]*>/u)
})

function roomState(joined, phase) {
  return {
    ...unavailableParticipantState, localName: 'Local',
    group: { name: 'Studio', joined, contact: joined ? 'connected' : 'contacting' },
    room: {
      cards: [], routes: [], globalRulesPath: 'global.json', autoEnter: joined, error: null,
      creationDefaults: { contactPort: 47101, listenAddress: '0.0.0.0:47101' },
      session: {
        phase, generation: '1', choices: [], discoveredRooms: [], profilePath: null,
        savedAddress: null, selectedEndpoints: [], localService: null,
        cardPersisted: true, failure: null, failureKind: null, discoveryFailure: null,
        globalRules: null, rules: [], playbackRules: [], rulesUnsaved: false,
        rulesSaveError: null, pendingTargetChange: false,
        ruleFiles: {
          room: { path: null, state: 'unloaded' },
          global: { path: 'global.json', state: 'saved' },
        },
      },
    },
  }
}

test('an established room exposes Leave without a misleading entry Cancel', () => {
  const html = renderToStaticMarkup(createElement(GroupPanel, {
    state: roomState(true, 'joined'), disabled: false, onAction: async () => {},
  }))
  assert.match(html, /离开房间/u)
  assert.doesNotMatch(html, /<button\b[^>]*>\s*取消\s*<\/button>/u)
})

test('a pending entry still provides cancellation', () => {
  const html = renderToStaticMarkup(createElement(GroupPanel, {
    state: roomState(false, 'connecting'), disabled: false, onAction: async () => {},
  }))
  assert.match(html, /<button\b[^>]*>\s*取消\s*<\/button>/u)
})

function savedCard(path, name, overrides = {}) {
  return {
    path, name, enteredAddress: null, port: null, strictAutoJoin: false,
    hasPassword: false, ruleCount: 0, error: null, ...overrides,
  }
}

function panelText(state) {
  return renderToStaticMarkup(createElement(GroupPanel, { state, disabled: false, onAction: async () => {} }))
    .replace(/<!--.*?-->/g, '')
}

test('a joined room shows its summary and Leave while technical facts stay in connection details', () => {
  const state = roomState(true, 'joined')
  state.room.session = {
    ...state.room.session, roomName: 'Studio', profilePath: 'C:/rooms/studio.json',
    localService: '0.0.0.0:47101', instanceId: 'instance-a',
  }
  state.room.cards = [savedCard('C:/rooms/studio.json', 'Studio')]
  const text = panelText(state)
  assert.match(text, /房间「Studio」/u)
  assert.match(text, /本机提供这个房间的联络服务，端口 47101/u)
  assert.match(text, /下次启动时会自动进入这个房间/u)
  assert.match(text, /当前房间/u)
  assert.match(text, /切换到其他房间/u)
  const details = text.indexOf('连接详情')
  assert.ok(details > 0)
  for (const fact of ['实例 instance-a', '代次 1', '全局规则文件 global.json']) {
    assert.ok(text.indexOf(fact) > details, fact)
  }
  assert.doesNotMatch(text, /自动加入此卡片/u)
})

test('saved cards offer join and create, show presence, and saved names leave the unknown pool', () => {
  const state = roomState(false, 'idle')
  state.room.cards = [savedCard('C:/rooms/studio.json', 'Studio', { hasPassword: true })]
  state.room.session = {
    ...state.room.session,
    discoveredRooms: [
      { name: 'Studio', instanceId: 'found-a', candidates: ['192.0.2.10:47101'] },
      { name: 'Lab', instanceId: 'found-c', candidates: ['192.0.2.12:47101'] },
    ],
  }
  const text = panelText(state)
  const pool = text.indexOf('局域网中的其他房间')
  const joinForm = text.indexOf('按名称加入其他房间')
  const saved = text.slice(0, pool)
  const unknown = text.slice(pool, joinForm)
  assert.match(saved, />加入<\/button>/u)
  assert.match(saved, />在本机创建<\/button>/u)
  assert.match(saved, /局域网中可见/u)
  assert.match(saved, /未填写地址，按名称查找 · 有密码/u)
  assert.doesNotMatch(text, /自动加入此卡片/u)
  assert.match(unknown, /Lab/)
  assert.match(unknown, /192\.0\.2\.12:47101/)
  assert.doesNotMatch(unknown, /Studio/)
})

test('choosing lists candidate addresses with one-click entry and keeps instance tokens folded', () => {
  const state = roomState(false, 'choosing')
  state.room.session = {
    ...state.room.session,
    roomName: 'Studio',
    choices: [
      { name: 'Studio', instanceId: 'instance-a', candidates: ['192.0.2.10:47101'] },
      { name: 'Studio', instanceId: 'instance-b', candidates: ['192.0.2.11:47101'] },
    ],
  }
  const text = panelText(state)
  const details = text.indexOf('连接详情')
  assert.equal((text.match(/>进入这个<\/button>/gu) ?? []).length, 2)
  assert.ok(text.indexOf('192.0.2.11:47101') < details)
  assert.ok(text.indexOf('instance-b') > details)
  assert.match(text, /<button\b[^>]*>\s*取消\s*<\/button>/u)
  assert.doesNotMatch(text, /确认选择|已选实例/u)
  assert.doesNotMatch(text, /已保存的房间/u)
})

test('a failed entry keeps its reason visible, offers closing the notice and shows entry choices again', () => {
  const state = roomState(false, 'failed')
  state.room.session = {
    ...state.room.session, roomName: 'Studio',
    failure: 'No discovered room matches the name, and the card has no address to probe.',
  }
  const text = panelText(state)
  assert.match(text, /没能进入房间「Studio」/u)
  assert.ok(text.indexOf('No discovered room matches the name') < text.indexOf('连接详情'))
  assert.match(text, /<button\b[^>]*>\s*关闭提示\s*<\/button>/u)
  assert.doesNotMatch(text, /<button\b[^>]*>\s*取消\s*<\/button>/u)
  assert.doesNotMatch(text, /在后台继续等待/u)
  assert.match(text, /已保存的房间/u)
})

test('typed failures use the Chinese explanation and keep the original detail under connection diagnostics', () => {
  const state = roomState(false, 'failed')
  const raw = 'The supplied room password was rejected.'
  state.room.session = {
    ...state.room.session, roomName: 'Studio', failureKind: 'password_rejected', failure: raw,
  }
  state.room.error = raw
  const text = panelText(state)
  const details = text.indexOf('连接详情')
  assert.ok(text.indexOf('房间密码不正确') < details)
  assert.ok(text.indexOf(raw) > details)
  assert.doesNotMatch(text, /在后台继续等待/u)
})

test('creation placeholders use the core listener family and explain an unavailable default', () => {
  const state = roomState(false, 'idle')
  state.room.creationDefaults = { contactPort: 47222, listenAddress: '[::1]:47222' }
  const ready = panelText(state)
  assert.match(ready, /placeholder="\[::1\]:47222"/u)
  assert.match(ready, /留空时使用 47222/u)
  assert.doesNotMatch(ready, /0\.0\.0\.0:47101|监听本机所有网络接口/u)
  state.room.creationDefaults = { contactPort: null, listenAddress: null }
  const unavailable = panelText(state)
  assert.match(unavailable, /等待网络就绪，或填写监听地址/u)
  assert.doesNotMatch(unavailable, /0\.0\.0\.0:47101|留空时使用 47101/u)
})

test('an unwritten card warns before room exit and shares the member-area retry', () => {
  const state = roomState(true, 'joined')
  state.room.session = { ...state.room.session, roomName: 'Studio', cardPersisted: false }
  state.room.session.ruleFiles.room = { path: 'cards/studio.json', state: 'write_failed', detail: 'The room card could not be saved.' }
  const text = panelText(state)
  assert.match(text, /这个房间的卡片还没有保存成功/u)
  assert.doesNotMatch(text, /<button\b[^>]*>\s*再次尝试保存卡片\s*<\/button>/u)
  const warning = text.indexOf('离开或切换房间会放弃本房间未保存的修改')
  assert.ok(warning >= 0 && warning < text.indexOf('离开房间</button>'))
  assert.match(text, /未保存的全局修改会保留/u)
  assert.equal(((text + memberText(state)).match(/<button\b[^>]*>\s*重新读取规则文件\s*<\/button>/gu) ?? []).length, 1)
  state.room.session.ruleFiles.room = { path: 'cards/studio.json', state: 'saved' }
  state.room.session.ruleFiles.global = { path: 'global.json', state: 'write_failed', detail: 'global error' }
  assert.doesNotMatch(panelText(state), /离开或切换房间会放弃/u)
})

test('setup explains writable existing-file use and current directory support', () => {
  const html = renderToStaticMarkup(createElement(SetupPanel, {
    busy: false, configPath: 'participant.json', message: null, onCreate: async () => {},
  }))
  assert.match(html, /打开现有配置/u)
  assert.match(html, /后续设置会写回该文件/u)
  assert.match(html, /自动进入房间设置也会恢复/u)
  assert.doesNotMatch(html, /导入现有配置|在目录功能完成前/u)
})

function remoteMember(id, name, overrides = {}) {
  return {
    id, name, contact: 'connected', sendAllowed: true, receiveAllowed: false,
    sendReady: false, sendingSourceIds: [], receiveReady: false, sources: [], ...overrides,
  }
}

function routeFor(member, address) {
  return {
    memberId: member.id, runId: '1', name: member.name, directoryVisible: true,
    send: member.sendAllowed, listen: member.receiveAllowed,
    sendFrom: null, listenFrom: null,
    sources: [],
    endpoint: { kind: 'ready', address },
  }
}

function memberText(state) {
  return renderToStaticMarkup(createElement(MemberList, { state, disabled: false, onAction: async () => {} }))
    .replace(/<!--.*?-->/g, '')
}

test('same-name members share one row whose switches show the core values for the whole name', () => {
  const state = roomState(true, 'joined')
  state.members = [
    remoteMember('a', 'Music', { receiveAllowed: true, receiveReady: true }),
    remoteMember('b', 'Music'),
  ]
  state.room.routes = [routeFor(state.members[0], '192.0.2.31:47000'), routeFor(state.members[1], '192.0.2.32:47000')]
  const text = memberText(state)
  assert.equal((text.match(/<li class="member-group"/g) ?? []).length, 1)
  assert.match(text, /同名 2 台/u)
  assert.match(text, /这 2 台一起改变/u)
  assert.match(text, /192\.0\.2\.31/)
  assert.match(text, /192\.0\.2\.32/)
  assert.equal((text.match(/role="checkbox" aria-checked="mixed"/g) ?? []).length, 1)
  assert.equal((text.match(/role="checkbox" aria-checked="true"/g) ?? []).length, 1)
  assert.equal((text.match(/role="switch"/g) ?? []).length, 0)
  assert.match(text, /按地址分别设置/u)
})

test('a unique name shows its address and one status per direction without scope text', () => {
  const state = roomState(true, 'joined')
  state.members = [remoteMember('a', 'Desk', { sendReady: true, sendingSourceIds: ['local-system'] })]
  state.room.routes = [routeFor(state.members[0], '192.0.2.40:47000')]
  state.localSources = [{
    id: 'local-system', kind: 'system', name: '系统声音', available: true, gainDb: 0, pan: 0,
    mute: false, solo: false, peakDb: null, captureState: 'ready', availabilityReason: '',
    publication: 'active', publish: true, monitor: false, endpointId: null,
  }]
  const text = memberText(state)
  assert.doesNotMatch(text, /同名|一起改变|按地址分别设置/u)
  assert.match(text, /192\.0\.2\.40/)
  assert.match(text, /未收听/u)
  assert.match(text, /已向对方开放：系统声音/u)
  assert.equal((text.match(/role="checkbox" aria-checked="false"/g) ?? []).length, 1)
})

test('member source details preserve differing fields and require current joined room facts', () => {
  const source = {
    id: 'source-system', kind: 'system', name: '系统声音', available: true,
    publication: 'active', publicationEpoch: '1', receiveAllowed: true, receiveReady: true, format: null,
    gainDb: 0, pan: 0, mute: false, solo: false, peakDb: null, outputTargets: ['main'],
  }
  const a = remoteMember('a', 'Music', { sources: [{ ...source }] })
  const b = remoteMember('b', 'Music', {
    sources: [{ ...source, id: 'source-b', gainDb: -6, pan: 0.5, mute: true, solo: true }],
  })
  const routes = [routeFor(a, '192.0.2.31:47000'), routeFor(b, '192.0.2.32:47000')]
  const render = (members, currentRoutes, joined = true) => renderToStaticMarkup(createElement(MemberDetail, {
    id: 'member-detail', group: groupMembers(members, currentRoutes)[0],
    joined, disabled: false, onAction: async () => { throw new Error('SSR must not issue an action') },
  })).replace(/<!--.*?-->/g, '')

  const differing = render([a, b], routes)
  assert.match(differing, /192\.0\.2\.31：/u)
  assert.match(differing, /192\.0\.2\.32：/u)
  assert.match(differing, /音量 0\.0 dB，声像 居中，\s*静音关闭，独奏关闭/u)
  assert.match(differing, /音量 -6\.0 dB，声像 右 0\.50，\s*静音开启，独奏开启/u)
  assert.match(differing, /修改某一项会统一该项，其余设置各自保留/u)
  assert.match(differing, /按名称与声音来源类型保存/u)
  assert.equal((differing.match(/type="range"/g) ?? []).length, 2)
  assert.equal((differing.match(/各设备不同，调整后统一/g) ?? []).length, 4)
  assert.equal((differing.match(/disabled=""/g) ?? []).length, 0)

  // Each field can be unified while the untouched fields retain their own values.
  for (const patch of [{ gainDb: -3 }, { pan: -0.5 }, { mute: true }, { solo: true }]) {
    const changed = { ...b, sources: [{ ...source, id: 'source-b', ...patch }] }
    assert.match(render([a, changed], routes), /修改某一项会统一该项，其余设置各自保留/u)
  }

  const enabled = render([a], [routes[0]])
  assert.equal((enabled.match(/disabled=""/g) ?? []).length, 0)
  for (const locked of [render([a], [routes[0]], false), render([a], [])]) {
    const mixControls = locked.match(/<(?:input|button)\b(?=[^>]*id="mix-(?:gain|pan|mute|solo)-)[^>]*>/g) ?? []
    assert.equal(mixControls.length, 4)
    assert.ok(mixControls.every((control) => control.includes('disabled=""')))
  }
})

test('missing current room facts disable edits without guessing a saved scope', () => {
  const state = roomState(true, 'joined')
  state.members = [remoteMember('a', 'Music', { sendReady: true })]
  const html = renderToStaticMarkup(createElement(MemberList, { state, disabled: false, onAction: async () => {} }))
  assert.match(html, /当前房间成员资料尚未就绪/u)
  assert.equal((html.match(/<button(?=[^>]*role="checkbox")(?=[^>]*disabled="")[^>]*>/g) ?? []).length, 2)
})

test('typed room publication failure keeps edits active and offers one reload with the original detail', () => {
  const state = roomState(true, 'joined')
  state.room.session = {
    ...state.room.session, profilePath: 'C:/rooms/a.json', rulesUnsaved: true,
    rulesSaveError: 'The room profile changed outside the application. It was left unchanged.',
    ruleFiles: {
      room: {
        path: 'C:/rooms/a.json', state: 'external_change',
        detail: 'The room profile changed outside the application. It was left unchanged.',
      },
      global: { path: 'global.json', state: 'saved' },
    },
  }
  const text = memberText(state)
  assert.match(text, /本房间规则尚未写入/u)
  assert.match(text, /文件已在软件外修改，原文件未被覆盖。当前修改仍然生效/u)
  assert.match(text, /重新读取会改用已有文件中的规则，并放弃对应的未写入修改/u)
  assert.match(text, /C:\/rooms\/a\.json/u)
  assert.match(text, /changed outside the application/)
  assert.equal((text.match(/>重新读取规则文件<\/button>/gu) ?? []).length, 1)
  state.room.session = {
    ...state.room.session, profilePath: null,
    ruleFiles: {
      room: { path: 'C:/rooms/new.json', state: 'pending' },
      global: { path: 'global.json', state: 'saved' },
    },
  }
  const pendingCard = memberText(state)
  assert.match(pendingCard, /房间卡片还没有保存成功/u)
  assert.match(pendingCard, /重新读取会再次尝试保存/u)
  assert.doesNotMatch(pendingCard, /放弃对应的未写入修改/u)
})

test('room and global file failures remain independent and do not mark saved room rules as unwritten', () => {
  const state = roomState(true, 'joined')
  state.room.session = {
    ...state.room.session, profilePath: 'C:/rooms/a.json', rulesUnsaved: true,
    ruleFiles: {
      room: { path: 'C:/rooms/a.json', state: 'saved' },
      global: { path: 'C:/global.json', state: 'write_failed', detail: 'Access was denied.' },
    },
  }
  const globalOnly = memberText(state)
  assert.match(globalOnly, /全局规则尚未写入/u)
  assert.match(globalOnly, /写入失败，当前修改仍然生效/u)
  assert.match(globalOnly, /C:\/global\.json/u)
  assert.match(globalOnly, /Access was denied\./u)
  assert.doesNotMatch(globalOnly, /本房间规则 0 条，尚未写入文件/u)
  assert.equal((globalOnly.match(/>重新读取规则文件<\/button>/gu) ?? []).length, 1)

  state.room.session.ruleFiles.room = {
    path: 'C:/rooms/a.json', state: 'external_change', detail: 'An external editor replaced the room file.',
  }
  const both = memberText(state)
  assert.match(both, /本房间规则尚未写入/u)
  assert.match(both, /全局规则尚未写入/u)
  assert.match(both, /An external editor replaced the room file\./u)
  assert.equal((both.match(/>重新读取规则文件<\/button>/gu) ?? []).length, 1)

  state.room.session.ruleFiles = {
    room: { path: null, state: 'unloaded' },
    global: { path: 'C:/global.json', state: 'unloaded' },
  }
  assert.doesNotMatch(memberText(state), /role="alert"|尚未写入/u)
})

test('each device shows its own confirmed directional source without inferring missing origins', () => {
  const state = roomState(true, 'joined')
  state.members = [remoteMember('a', 'Music'), remoteMember('b', 'Music')]
  state.room.session.globalRules = {
    schema: 1, rules: [{ match: 'all', send: true }, { match: { name: 'Music' }, send: false }],
  }
  state.room.session.rules = [{ match: { name: 'Music', ip: '192.0.2.31' }, listen: true }]
  state.room.routes = [
    {
      ...routeFor(state.members[0], '192.0.2.31:47000'),
      sendFrom: { list: 'global', index: 1 }, listenFrom: { list: 'room', index: 0 },
    },
    routeFor(state.members[1], '192.0.2.32:47000'),
  ]
  const text = memberText(state)
  assert.equal((text.match(/发送：全局规则第 2 条/gu) ?? []).length, 1)
  assert.equal((text.match(/收听：本房间规则第 1 条，按地址单独设置/gu) ?? []).length, 1)
  assert.ok(text.indexOf('发送：全局规则第 2 条') < text.indexOf('192.0.2.32'))
})

test('contact loss after joining says established audio continues and keeps the member rows', () => {
  const state = roomState(true, 'contact_unavailable')
  state.members = [remoteMember('a', 'Desk', { receiveAllowed: true, receiveReady: true })]
  state.room.routes = [{ ...routeFor(state.members[0], '192.0.2.40:47000'), directoryVisible: false }]
  const text = memberText(state)
  assert.match(text, /联络服务暂时不可用/u)
  assert.match(text, /已经建立的声音继续/u)
  assert.match(text, /<li class="member-group"/)
  const before = roomState(false, 'contact_unavailable')
  assert.doesNotMatch(memberText(before), /已经建立的声音继续/u)
})

test('room rules are listed in file order with exact conditions and only assigned directions', () => {
  const state = roomState(true, 'joined')
  state.room.session = {
    ...state.room.session,
    rules: [
      { match: { name: 'Music' }, listen: true },
      { match: { ip: '192.0.2.32', name: 'Music' }, send: false },
    ],
  }
  const text = memberText(state)
  const first = text.indexOf('名称「Music」：收听')
  const second = text.indexOf('名称「Music」且地址 192.0.2.32：不发送给对方')
  assert.ok(first >= 0 && second > first)
  assert.match(text, /本房间规则 2 条/u)
})
