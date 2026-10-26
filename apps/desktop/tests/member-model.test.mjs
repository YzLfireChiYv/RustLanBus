import assert from 'node:assert/strict'
import test from 'node:test'
import {
  assignmentText,
  defaultRuleAction,
  defaultRuleView,
  deviceEditAction,
  deviceSwitchState,
  directionRuleSource,
  endpointIp,
  groupEditable,
  groupMembers,
  groupSwitchState,
  incomingStatus,
  matchText,
  nameEditAction,
  outgoingStatus,
  ruleFileNeedsAttention,
  selectedOutputEndpointIds,
  switchState,
} from '../src/participant/memberModel.ts'

function member(id, name, overrides = {}) {
  return {
    id, name, contact: 'connected', sendAllowed: true, receiveAllowed: false,
    sendReady: false, sendingSourceIds: [], receiveReady: false, sources: [], ...overrides,
  }
}

function route(memberId, name, address) {
  return {
    memberId, runId: '1', name, endpoint: { kind: 'ready', address },
    directoryVisible: true, send: true, listen: false,
    sendFrom: null, listenFrom: null,
    outputs: ['main'], outputsFrom: null,
    sources: [],
  }
}

function remoteSource(kind, overrides = {}) {
  return {
    id: `src-${kind}`, kind, name: kind === 'system' ? '系统声音' : '麦克风', available: false,
    gainDb: 0, pan: 0, mute: false, solo: false, peakDb: null, publication: 'active',
    publicationEpoch: '1', format: null, ...overrides,
    outputTargets: overrides.outputTargets ?? ['main'],
    availabilityReason: '',
    receiveAllowed: overrides.receiveAllowed ?? true,
    receiveReady: overrides.receiveReady ?? true,
  }
}

function localSource(kind, publish, publication) {
  return {
    id: `local-${kind}`, kind, name: kind === 'system' ? '系统声音' : '麦克风',
    available: publication === 'active', gainDb: 0, pan: 0, mute: false, solo: false, peakDb: null,
    captureState: 'ready', availabilityReason: '', publication, publish, monitor: false, endpointId: null,
    outputTargets: ['main'], monitorReady: false, monitorAvailabilityReason: '',
  }
}

test('the rule address is the endpoint IP the core compares, without port or IPv6 zone', () => {
  assert.equal(endpointIp({ kind: 'ready', address: '192.0.2.31:47000' }), '192.0.2.31')
  assert.equal(endpointIp({ kind: 'ready', address: '[2001:db8::5]:47000' }), '2001:db8::5')
  assert.equal(endpointIp({ kind: 'ready', address: '[fe80::1%12]:47000' }), 'fe80::1')
  assert.equal(endpointIp({ kind: 'local_scope_required', ip: 'fe80::2', port: 47000 }), 'fe80::2')
  assert.equal(endpointIp({ kind: 'ready', address: '[fe80::1' }), null)
})

test('same-name members form one group in first-appearance order under the current room name', () => {
  const members = [member('a', 'Music'), member('b', 'Desk'), member('c', 'Music')]
  const routes = [
    route('a', 'Music', '192.0.2.31:47000'),
    route('b', 'Desk', '192.0.2.40:47000'),
    route('c', 'Music', '192.0.2.32:47000'),
  ]
  const groups = groupMembers(members, routes)
  assert.deepEqual(groups.map((group) => group.name), ['Music', 'Desk'])
  assert.deepEqual(groups[0].devices.map((device) => device.ip), ['192.0.2.31', '192.0.2.32'])
  const spaced = groupMembers(
    [member('a', 'Music'), member('d', 'Music ')],
    [route('a', 'Music', '192.0.2.31:47000'), route('d', 'Music ', '192.0.2.33:47000')],
  )
  assert.deepEqual(spaced.map((group) => group.name), ['Music', 'Music '])
})

test('a group lacking current route facts for any device cannot be edited', () => {
  const partial = groupMembers(
    [member('a', 'Music'), member('c', 'Music')],
    [route('a', 'Music', '192.0.2.31:47000')],
  )
  assert.equal(partial.length, 1)
  assert.equal(groupEditable(partial[0], true), false)
  const complete = groupMembers([member('a', 'Music')], [route('a', 'Music', '192.0.2.31:47000')])
  assert.equal(groupEditable(complete[0], true), true)
  assert.equal(groupEditable(complete[0], false), false)
})

test('switch state shows a partly-on group as mixed and an empty group as off', () => {
  assert.equal(switchState([true, true]), 'on')
  assert.equal(switchState([false, false]), 'off')
  assert.equal(switchState([true, false]), 'mixed')
  assert.equal(switchState([]), 'off')
})

test('group edits use the name only, device edits add the address, and both omit the other direction', () => {
  const group = JSON.parse(JSON.stringify(nameEditAction('Music', 'listen', true)))
  assert.deepEqual(group, { type: 'edit_room_rule', rule: { match: { name: 'Music' }, listen: true } })
  const device = JSON.parse(JSON.stringify(deviceEditAction('Music', '192.0.2.32', 'send', false)))
  assert.deepEqual(device, {
    type: 'edit_room_rule', rule: { match: { ip: '192.0.2.32', name: 'Music' }, send: false },
  })
  assert.equal('send' in group.rule, false)
  assert.equal('listen' in device.rule, false)
})

test('member switches aggregate projected source choices without evaluating rules', () => {
  const current = route('a', 'Music', '192.0.2.31:47000')
  current.sources = [
    { kind: 'system', send: true, listen: true, sendFrom: null, listenFrom: null },
    { kind: 'microphone', send: false, listen: false, sendFrom: null, listenFrom: null },
  ]
  const group = groupMembers([member('a', 'Music')], [current])[0]
  assert.equal(deviceSwitchState(group.devices[0], 'listen'), 'mixed')
  assert.equal(groupSwitchState(group, 'send'), 'mixed')
  const off = { ...current, listen: true, sources: current.sources.map((source) => ({ ...source, listen: false })) }
  assert.equal(groupSwitchState(groupMembers([member('a', 'Music')], [off])[0], 'listen'), 'off')
  const action = nameEditAction('Music', 'listen', true)
  assert.equal('source_kind' in action.rule, false)
})

test('incoming status separates the local choice, the remote choice and actual playback', () => {
  assert.equal(incomingStatus(member('a', 'M')).text, '未收听')
  assert.equal(
    incomingStatus(member('a', 'M', { receiveAllowed: true, contact: 'reconnecting' })).text,
    '等待接通',
  )
  assert.equal(
    incomingStatus(member('a', 'M', { receiveAllowed: true, receiveReady: false })).text,
    '对方暂未向本机发送',
  )
  const unpublished = member('a', 'M', {
    receiveAllowed: true, receiveReady: true, sources: [remoteSource('system', { publication: 'disabled' })],
  })
  assert.equal(incomingStatus(unpublished).text, '对方没有发布声音')
  assert.equal(incomingStatus({ ...unpublished, sources: [remoteSource('system', { publication: 'unavailable' })] }).text,
    '对方已发布声音，来源暂不可用')
  const notPlaying = member('a', 'M', {
    receiveAllowed: true, receiveReady: true, sources: [remoteSource('system')],
  })
  assert.equal(incomingStatus(notPlaying).tone, 'warning')
  assert.match(incomingStatus(notPlaying).text, /尚未接入本机播放/)
  const noOutput = { ...notPlaying, sources: [remoteSource('system', { outputTargets: [] })] }
  assert.equal(incomingStatus(noOutput).text, '已接收声音，尚未选择输出设备')
  assert.equal(noOutput.sources[0].receiveReady, true)
  const playing = member('a', 'M', {
    receiveAllowed: true,
    receiveReady: true,
    sources: [remoteSource('system', { available: true }), remoteSource('microphone', { publication: 'disabled' })],
  })
  assert.deepEqual(incomingStatus(playing), { tone: 'positive', text: '正在收听：系统声音' })
  assert.equal(incomingStatus({ ...playing, receiveAllowed: false }).text, '未收听')
})

test('output guards include exact disconnected destinations and resolved main without inventing endpoints', () => {
  const base = {
    outputEndpointId: 'main-selected', mainOutputId: 'main-resolved',
    localSources: [{ outputTargets: ['main', { endpoint: 'local-monitor' }] }],
    room: { routes: [{ outputs: ['main'], sources: [
      { outputs: [{ endpoint: 'remote-one' }, { endpoint: 'local-monitor' }] },
      { outputs: [] },
    ] }] },
  }
  assert.deepEqual(selectedOutputEndpointIds(base), [
    'main-selected', 'main-resolved', 'local-monitor', 'remote-one',
  ])
  assert.deepEqual(selectedOutputEndpointIds({
    outputEndpointId: null, mainOutputId: null,
    localSources: [{ outputTargets: ['main'] }], room: { routes: [] },
  }), [])
  assert.equal(assignmentText({ match: 'all', outputs: [] }), '不送往输出设备')
  assert.equal(assignmentText({ match: 'all', outputs: ['main', { endpoint: 'virtual-cable' }] }),
    '输出：跟随主输出、virtual-cable')
})

test('outgoing status confirms the remote listener and names what this computer publishes', () => {
  const sources = [localSource('system', true, 'active'), localSource('microphone', false, 'disabled')]
  assert.equal(outgoingStatus(member('a', 'M', { sendAllowed: false, sendReady: true }), sources).text, '不向对方发送')
  assert.equal(outgoingStatus(member('a', 'M'), sources).text, '发送尚未就绪，等待对方接收或确认')
  assert.equal(outgoingStatus(member('a', 'M', { contact: 'contacting', sendReady: true }), sources).text, '等待接通')
  assert.deepEqual(outgoingStatus(member('a', 'M', { sendReady: true, sendingSourceIds: ['local-system'] }), sources), {
    tone: 'positive', text: '已向对方开放：系统声音',
  })
  assert.equal(
    outgoingStatus(member('a', 'M', { sendReady: true }), [localSource('system', true, 'unavailable')]).text,
    '本机声音尚未就绪',
  )
  assert.equal(
    outgoingStatus(member('a', 'M', { sendReady: true }), [localSource('system', false, 'disabled')]).text,
    '本机没有发布声音',
  )
})

test('outgoing labels include only source IDs confirmed ready by the peer', () => {
  const sources = [localSource('system', true, 'active'), localSource('microphone', true, 'active')]
  const current = route('a', 'M', '192.0.2.31:47000')
  current.sources = [
    { kind: 'system', send: true, listen: false, sendFrom: null, listenFrom: null },
    { kind: 'microphone', send: true, listen: false, sendFrom: null, listenFrom: null },
  ]
  const peer = member('a', 'M', { sendReady: true, sendingSourceIds: ['local-system'] })
  assert.deepEqual(outgoingStatus(peer, sources, current), {
    tone: 'positive', text: '已向对方开放：系统声音',
  })
  assert.equal(outgoingStatus({ ...peer, sendingSourceIds: [] }, sources, current).text,
    '发送尚未就绪，等待对方接收或确认')
})

test('incoming labels use source admission even when the wildcard member choice differs', () => {
  const current = route('a', 'M', '192.0.2.31:47000')
  current.sources = [
    { kind: 'system', send: true, listen: true, sendFrom: null, listenFrom: null },
    { kind: 'microphone', send: true, listen: false, sendFrom: null, listenFrom: null },
  ]
  const peer = member('a', 'M', {
    receiveAllowed: false,
    sources: [
      remoteSource('system', { available: true }),
      remoteSource('microphone', { available: true, receiveAllowed: false, receiveReady: false }),
    ],
  })
  assert.deepEqual(incomingStatus(peer, current), { tone: 'positive', text: '正在收听：系统声音' })
  const waiting = { ...peer, sources: peer.sources.map((source) => ({ ...source, receiveReady: false })) }
  assert.equal(incomingStatus(waiting, current).text, '对方暂未向本机发送')
})

test('new-member defaults send only a direction patch to the authoritative core', () => {
  const rules = {
    schema: 1,
    rules: [
      { match: 'all', send: true, listen: false },
      { match: { ip: '192.0.2.31' }, listen: true },
    ],
  }
  assert.deepEqual(defaultRuleView(rules), { send: true, listen: false, additional: 1 })
  const on = defaultRuleAction(rules, 'listen', true)
  assert.deepEqual(on, { type: 'set_global_defaults', listen: true })
  const send = defaultRuleAction(rules, 'send', false)
  assert.deepEqual(send, { type: 'set_global_defaults', send: false })
  assert.equal(rules.rules[0].listen, false)
})

test('defaults stay read-only when global rule 1 does not match everyone', () => {
  const custom = { schema: 1, rules: [{ match: { name: 'Desk' }, listen: true }, { match: 'all', send: true }] }
  assert.equal(defaultRuleView(custom), null)
  assert.equal(defaultRuleAction(custom, 'send', false), null)
  assert.equal(defaultRuleView(null), null)
  assert.equal(defaultRuleView({ schema: 1, rules: [] }), null)
  const sourceOnly = { schema: 1, rules: [{ match: 'all', source_kind: 'microphone', listen: true }] }
  assert.equal(defaultRuleView(sourceOnly), null)
  assert.equal(defaultRuleAction(sourceOnly, 'listen', false), null)
})

test('a default edit leaves the complementary field and entire rule list out of the payload', () => {
  const partial = { schema: 1, rules: [{ match: 'all', listen: false }] }
  const action = defaultRuleAction(partial, 'send', false)
  assert.equal(JSON.stringify(action), '{"type":"set_global_defaults","send":false}')
})

test('rule text names the exact condition and only the assigned directions', () => {
  assert.equal(matchText('all'), '全部成员')
  assert.equal(matchText({ name: 'Music' }), '名称「Music」')
  assert.equal(matchText({ ip: '192.0.2.32', name: 'Music' }), '名称「Music」且地址 192.0.2.32')
  assert.equal(matchText({ ip: '192.0.2.32' }), '地址 192.0.2.32')
  assert.equal(assignmentText({ match: 'all', listen: true }), '收听')
  assert.equal(assignmentText({ match: 'all', send: false, listen: false }), '不发送给对方，不收听')
  assert.equal(assignmentText({ match: 'all', source_kind: 'microphone', listen: false }), '麦克风：不收听')
})

test('direction source cites only the core-provided assignment and leaves ordinary defaults quiet', () => {
  const session = {
    globalRules: { schema: 1, rules: [
      { match: 'all', send: true, listen: false },
      { match: { name: 'Music' }, send: false },
    ] },
    rules: [
      { match: { name: 'Music' }, listen: true },
      { match: { name: 'Music', ip: '192.0.2.32' }, listen: false },
    ],
  }
  const current = route('a', 'Music', '192.0.2.32:47000')
  assert.equal(directionRuleSource(current, 'send', session), null)
  assert.equal(directionRuleSource({ ...current, sendFrom: { list: 'global', index: 0 } }, 'send', session), null)
  assert.equal(directionRuleSource({ ...current, listenFrom: { list: 'room', index: 0 } }, 'listen', session), null)
  assert.equal(
    directionRuleSource({ ...current, sendFrom: { list: 'global', index: 1 } }, 'send', session),
    '全局规则第 2 条（名称「Music」）',
  )
  assert.equal(
    directionRuleSource({ ...current, listenFrom: { list: 'room', index: 1 } }, 'listen', session),
    '本房间规则第 2 条，按地址单独设置（名称「Music」且地址 192.0.2.32）',
  )
  // The same facts could match several rules. The UI never chooses a winner.
  assert.equal(directionRuleSource(current, 'listen', session), null)
  for (const index of [-1, 1.5, 12, Number.NaN]) {
    assert.equal(directionRuleSource({ ...current, sendFrom: { list: 'global', index } }, 'send', session), null)
  }
  assert.equal(directionRuleSource({ ...current, listenFrom: { list: 'global', index: 1 } }, 'listen', session), null)
  assert.equal(directionRuleSource(null, 'send', session), null)
  assert.equal(directionRuleSource(current, 'send', null), null)
})

test('only typed pending or failed file publication states require attention', () => {
  for (const state of ['pending', 'external_change', 'write_failed']) {
    assert.equal(ruleFileNeedsAttention({ path: 'rules.json', state }), true)
  }
  for (const state of ['saved', 'unloaded']) {
    assert.equal(ruleFileNeedsAttention({ path: 'rules.json', state }), false)
  }
})
