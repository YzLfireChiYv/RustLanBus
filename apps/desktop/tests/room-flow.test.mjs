import assert from 'node:assert/strict'
import test from 'node:test'
import {
  backgroundWaitEntry,
  cancelChoice,
  cardFactLines,
  cardFacts,
  cardPresence,
  choiceFactLines,
  chooseEntry,
  chooseFromClick,
  confirmChoiceEntry,
  createRoomEntry,
  createSavedEntry,
  creatorBind,
  currentChoiceDraft,
  discoveredFactLines,
  entryCancellationAllowed,
  joinDiscoveredEntry,
  joinSavedEntry,
  leaveRoomSelection,
  localNameLocked,
  refreshCardsAction,
  reloadRulesAction,
  roomControlLines,
  roomFailureText,
  roomStatus,
  sessionFactLines,
  unknownJoinEntry,
  unknownRooms,
  withAddressCondition,
} from '../src/participant/roomFlow.ts'

const generation = '9007199254740993'
const creationDefaults = { contactPort: 47101, listenAddress: '0.0.0.0:47101' }
const unavailableDefaults = { contactPort: null, listenAddress: null }
const studios = [
  { name: 'Studio', instanceId: 'instance-a', candidates: ['192.0.2.10:47101'] },
  { name: 'Studio', instanceId: 'instance-b', candidates: ['192.0.2.11:47101'] },
]

test('discovered room names remain exact even when an unpadded name also exists', () => {
  const plain = joinDiscoveredEntry('Studio', '', false)
  const padded = joinDiscoveredEntry(' Studio ', '', false)
  assert.equal(plain.action.target.name, 'Studio')
  assert.equal(padded.action.target.name, ' Studio ')
  assert.notEqual(plain.action.target.name, padded.action.target.name)
  assert.equal(padded.action.target.address, null)
})

test('choice display follows authoritative phase, generation and candidate presence', () => {
  const draft = { generation, instanceId: 'instance-b' }
  const choosing = { phase: 'choosing', generation, choices: studios }
  assert.equal(currentChoiceDraft(choosing, draft), draft)
  assert.equal(currentChoiceDraft({ ...choosing, phase: 'joined' }, draft), null)
  assert.equal(currentChoiceDraft({ ...choosing, generation: '2' }, draft), null)
  assert.equal(currentChoiceDraft({ ...choosing, choices: studios.slice(0, 1) }, draft), null)
  assert.equal(currentChoiceDraft(null, draft), null)
  assert.equal(entryCancellationAllowed(true, 'joined'), false)
  assert.equal(entryCancellationAllowed(true, 'choosing'), false)
  assert.equal(entryCancellationAllowed(false, 'connecting'), true)
  assert.equal(entryCancellationAllowed(false, 'contact_unavailable'), true)
})

function facts(overrides) {
  return sessionFactLines({
    phase: 'idle',
    profilePath: null,
    savedAddress: null,
    selectedEndpoints: [],
    localService: null,
    cardPersisted: null,
    failure: null,
    discoveryFailure: null,
    ...overrides,
  })
}

test('local setup actions keep an unknown join distinct from create and skip an empty name', () => {
  const emptyUnknown = unknownJoinEntry({
    name: '  ',
    address: '192.0.2.10',
    port: '',
    password: 'sentinel-password-do-not-leak',
    strictAutoJoin: false,
  })
  const emptyCreate = createRoomEntry({
    name: '',
    address: '',
    port: '',
    password: 'sentinel-password-do-not-leak',
    strictAutoJoin: false,
    bind: '0.0.0.0:47101',
  }, creationDefaults)
  assert.equal(emptyUnknown.action, null)
  assert.equal(emptyCreate.action, null)
  assert.equal(JSON.stringify(emptyUnknown).includes('create_room'), false)
  assert.equal(JSON.stringify(emptyCreate).includes('create_room'), false)
  assert.equal(JSON.stringify(emptyUnknown).includes('join_unknown_room'), false)

  const joined = unknownJoinEntry({
    name: 'Studio',
    address: '',
    port: '',
    password: 'sentinel-password-do-not-leak',
    strictAutoJoin: false,
  })
  assert.equal(joined.action.type, 'join_unknown_room')
  assert.equal(joined.action.target.name, 'Studio')
  assert.equal(joined.action.target.address, null)
  assert.equal(joined.action.target.password, 'sentinel-password-do-not-leak')
  assert.equal(JSON.stringify(facts({ phase: 'resolving' })).includes('sentinel-password-do-not-leak'), false)

  const discovered = joinDiscoveredEntry('Studio', 'sentinel-password-do-not-leak', true)
  assert.equal(discovered.action.type, 'join_unknown_room')
  assert.equal(discovered.action.target.address, null)
  assert.equal(discovered.action.target.strict_auto_join, true)
  assert.equal(JSON.stringify(discovered.action).includes('198.51.100.10'), false)
  const pool = discoveredFactLines({
    name: 'Studio',
    instanceId: 'instance-b',
    candidates: ['198.51.100.10:5004'],
  }).join('\n')
  assert.match(pool, /运行候选 198\.51\.100\.10:5004/)
  assert.equal(pool.includes('填写地址'), false)
  assert.equal(pool.includes('sentinel-password-do-not-leak'), false)

  const badPort = unknownJoinEntry({
    name: 'Studio',
    address: '',
    port: 'nope',
    password: '',
    strictAutoJoin: false,
  })
  assert.equal(badPort.action, null)
})

test('two same-name choices submit the clicked instance and the current generation string', () => {
  const draft = chooseFromClick(generation, studios, 'instance-b')
  assert.equal(draft.instanceId, 'instance-b')
  assert.equal(draft.generation, generation)
  assert.equal(typeof draft.generation, 'string')
  const entry = confirmChoiceEntry('choosing', generation, studios, draft)
  assert.equal(entry.action.type, 'choose_room')
  assert.equal(entry.action.instanceId, 'instance-b')
  assert.notEqual(entry.action.instanceId, 'instance-a')
  assert.equal(entry.action.generation, '9007199254740993')
  assert.equal(typeof entry.action.generation, 'string')
  assert.match(JSON.stringify(entry.action), /"generation":"9007199254740993"/)
  assert.equal(JSON.stringify(entry.action).includes('9007199254740992'), false)
  const shown = choiceFactLines(studios[1]).join('\n')
  assert.match(shown, /待选房间 Studio/)
  assert.match(shown, /实例 instance-b/)
  assert.equal(chooseFromClick(generation, studios, 'instance-a').instanceId, 'instance-a')
})

test('an expired choice does not submit choose_room or another room', () => {
  const clicked = chooseFromClick(generation, studios, 'instance-b')
  const wrongGeneration = confirmChoiceEntry('choosing', '5', studios, {
    generation: '4',
    instanceId: 'instance-a',
  })
  const missing = confirmChoiceEntry('choosing', generation, studios, {
    generation,
    instanceId: 'gone',
  })
  const failedPhase = confirmChoiceEntry('failed', generation, studios, clicked)
  const resolvingPhase = confirmChoiceEntry('resolving', generation, studios, clicked)
  for (const entry of [wrongGeneration, missing, failedPhase, resolvingPhase]) {
    assert.equal(entry.action, null)
    assert.equal(JSON.stringify(entry).includes('choose_room'), false)
    assert.equal(JSON.stringify(entry).includes('instance-a'), false)
    assert.equal(JSON.stringify(entry).includes('instance-b'), false)
  }
  assert.equal(chooseFromClick(generation, studios, 'gone'), null)
})

test('cancel clears the local choice and does not join or choose', () => {
  const cancelled = cancelChoice()
  assert.equal(cancelled.action.type, 'cancel_room')
  assert.equal(cancelled.draft, null)
  assert.equal(cancelled.action.type === 'join_unknown_room', false)
  assert.equal(cancelled.action.type === 'choose_room', false)
  assert.equal(cancelled.action.type === 'create_room', false)
  assert.equal(cancelled.action.type === 'join_saved_room', false)
  const left = leaveRoomSelection()
  assert.equal(left.action.type, 'leave_room')
  assert.equal(left.draft, null)
})

test('card text keeps an unpersisted card, a blank address, and a card error', () => {
  const card = cardFactLines({
    path: 'C:/rooms/studio.json',
    name: 'Studio',
    enteredAddress: null,
    port: null,
    hasPassword: true,
    error: '档案损坏',
  }).join('\n')
  assert.match(card, /填写地址 未填写/)
  assert.match(card, /档案损坏/)
  assert.match(card, /有密码 是/)
  assert.equal(card.includes('0.0.0.0'), false)
  assert.equal(card.includes('已写入'), false)
  assert.equal(card.includes('sentinel-password-do-not-leak'), false)

  const session = facts({
    phase: 'joined',
    profilePath: 'C:/rooms/studio.json',
    savedAddress: null,
    selectedEndpoints: ['198.51.100.10:5004'],
    localService: '0.0.0.0:47101',
    cardPersisted: false,
  }).join('\n')
  assert.match(session, /尚未写入/)
  assert.equal(session.includes('已写入'), false)
  assert.equal(session.includes('sentinel-password-do-not-leak'), false)
  const saved = session.split('\n').find((line) => line.startsWith('已保存地址'))
  const selected = session.split('\n').find((line) => line.startsWith('选中端点'))
  const listen = session.split('\n').find((line) => line.startsWith('创建者监听'))
  assert.equal(saved, '已保存地址 未填写')
  assert.equal(selected, '选中端点 198.51.100.10:5004')
  assert.equal(listen, '创建者监听 0.0.0.0:47101')
  assert.equal(saved.includes('0.0.0.0'), false)
  assert.equal(saved.includes('198.51.100.10'), false)
})

test('idle archive text stays separate from the global rules path', () => {
  const idle = facts({ phase: 'idle', profilePath: 'C:/rooms/studio.json' }).join('\n')
  const global = roomControlLines({
    error: '卡片目录不可用',
    globalRulesPath: 'C:/rooms/global.json',
    autoEnter: true,
  }).join('\n')
  assert.match(idle, /房间阶段 空闲/)
  assert.match(idle, /当前档案 C:\/rooms\/studio\.json/)
  assert.equal(idle.includes('全局规则'), false)
  assert.match(global, /全局规则文件 C:\/rooms\/global\.json/)
  assert.match(global, /下次启动自动进入 是/)
  assert.match(global, /房间控制错误 卡片目录不可用/)
  assert.equal(global.includes('studio.json'), false)
})

test('resolving, choosing, and failed follow the snapshot phase', () => {
  const labels = [
    ['resolving', '正在解析'],
    ['choosing', '需要选择'],
    ['failed', '未能完成'],
  ]
  for (const [phase, label] of labels) {
    const text = facts({
      phase,
      failure: phase === 'failed' ? '连接失败' : null,
    }).join('\n')
    assert.match(text, new RegExp(`房间阶段 ${label}`))
    assert.equal(text.includes('目录已加入'), false)
  }
  assert.match(facts({ phase: 'failed', failure: '连接失败' }).join('\n'), /房间失败说明 连接失败/)
})

test('saved cards join or create explicitly, and refresh does not require membership', () => {
  const manual = joinSavedEntry('C:/rooms/studio.json', 'manual')
  assert.equal(manual.action.type, 'join_saved_room')
  assert.equal(manual.action.profilePath, 'C:/rooms/studio.json')
  assert.equal(manual.action.mode, 'manual')
  const automatic = joinSavedEntry('C:/rooms/studio.json', 'automatic')
  assert.equal(automatic.action.type, 'join_saved_room')
  assert.equal(automatic.action.mode, 'automatic')
  assert.equal(createSavedEntry('C:/rooms/studio.json', '   ', 47111, creationDefaults).action.bind, '0.0.0.0:47111')
  const hosted = createSavedEntry('C:/rooms/studio.json', ' 192.0.2.5:47101 ', 47111, creationDefaults)
  assert.equal(hosted.action.type, 'create_saved_room')
  assert.equal(hosted.action.bind, '192.0.2.5:47101')
  assert.equal(hosted.action.profilePath, 'C:/rooms/studio.json')
  const created = createRoomEntry({
    name: 'Studio',
    address: '',
    port: '47101',
    password: 'sentinel-password-do-not-leak',
    strictAutoJoin: false,
    bind: '0.0.0.0:47101',
  }, creationDefaults)
  assert.equal(created.action.type, 'create_room')
  assert.equal(created.action.bind, '0.0.0.0:47101')
  assert.equal(created.action.profile.room.address, null)
  assert.equal(created.action.profile.room.port, 47101)
  assert.equal(created.action.profile.room.password, 'sentinel-password-do-not-leak')
  assert.equal(refreshCardsAction().type, 'refresh_rooms')
  assert.equal(reloadRulesAction().type, 'reload_room_rules')
  assert.equal(localNameLocked(false, null), false)
  assert.equal(localNameLocked(false, 'failed'), false)
  assert.equal(localNameLocked(false, 'idle'), false)
  assert.equal(localNameLocked(true, 'idle'), true)
  assert.equal(localNameLocked(false, 'joined'), true)
  assert.equal(localNameLocked(false, 'choosing'), true)
})

test('creating listens on the room port unless an advanced listen address is given', () => {
  assert.equal(creatorBind('', null, creationDefaults), '0.0.0.0:47101')
  assert.equal(creatorBind('   ', 0, creationDefaults), '0.0.0.0:47101')
  assert.equal(creatorBind('', 47111, creationDefaults), '0.0.0.0:47111')
  assert.equal(creatorBind(' 192.0.2.5:5000 ', 47111, creationDefaults), '192.0.2.5:5000')
  const created = createRoomEntry({
    name: 'Studio', address: '', port: '47111', password: '', strictAutoJoin: false, bind: '',
  }, creationDefaults)
  assert.equal(created.action.bind, '0.0.0.0:47111')
  assert.equal(created.action.profile.room.address, null)
  assert.equal(created.action.profile.room.port, 47111)
  const badPort = createRoomEntry({
    name: 'Studio', address: '', port: '70000', password: '', strictAutoJoin: false, bind: '',
  }, creationDefaults)
  assert.equal(badPort.action, null)
})

test('creation keeps the supplied IPv6 or loopback scope, and refuses an unavailable default', () => {
  const ipv6 = { contactPort: 47222, listenAddress: '[::]:47222' }
  assert.equal(creatorBind('', null, ipv6), '[::]:47222')
  assert.equal(creatorBind('', 47111, ipv6), '[::]:47111')
  assert.equal(creatorBind('', 0, ipv6), '[::]:47222')
  assert.equal(creatorBind('', null, { contactPort: 47222, listenAddress: '[::1]:47222' }), '[::1]:47222')
  assert.equal(creatorBind('', 47111, { contactPort: 47222, listenAddress: '127.0.0.1:47222' }), '127.0.0.1:47111')
  assert.equal(creatorBind('', null, unavailableDefaults), null)
  const draft = { name: 'Studio', address: '', port: '', password: '', strictAutoJoin: false, bind: '' }
  for (const refused of [
    createRoomEntry(draft, unavailableDefaults),
    createSavedEntry('C:/rooms/studio.json', '', 47111, unavailableDefaults),
  ]) {
    assert.equal(refused.action, null)
    assert.match(refused.message, /无法确定本机的监听地址/u)
  }
  assert.equal(createRoomEntry(draft, ipv6).action.bind, '[::]:47222')
  assert.equal(createRoomEntry({ ...draft, bind: '[::1]:5000' }, unavailableDefaults).action.bind, '[::1]:5000')
})

test('the unknown pool drops exactly saved names and the current room; cards show presence', () => {
  const found = [
    { name: 'Studio', instanceId: 'a', candidates: ['192.0.2.10:47101'] },
    { name: ' Studio ', instanceId: 'b', candidates: ['192.0.2.11:47101'] },
    { name: 'Lab', instanceId: 'c', candidates: ['192.0.2.12:47101'] },
  ]
  const cards = [{ name: 'Studio' }, { name: null }]
  assert.deepEqual(unknownRooms(found, cards, 'c').map((room) => room.instanceId), ['b'])
  assert.deepEqual(unknownRooms(found, cards, null).map((room) => room.instanceId), ['b', 'c'])
  assert.deepEqual(cardPresence({ name: 'Studio' }, found).map((room) => room.instanceId), ['a'])
  assert.deepEqual(cardPresence({ name: null }, found), [])
  assert.deepEqual(
    cardFacts({ enteredAddress: null, port: null, hasPassword: false, strictAutoJoin: false }),
    ['未填写地址，按名称查找', '无密码'],
  )
  assert.deepEqual(
    cardFacts({ enteredAddress: '192.0.2.10', port: 47111, hasPassword: true, strictAutoJoin: true }),
    ['地址 192.0.2.10', '端口 47111', '有密码', '自动进入时要求地址一致'],
  )
})

test('room status follows the phase and whether this computer has joined', () => {
  const status = (phase, joined) => roomStatus({ phase, roomName: 'Studio', failure: null, failureKind: null }, joined)
  assert.equal(roomStatus(null, false).stage, 'outside')
  assert.equal(status('idle', false).stage, 'outside')
  assert.deepEqual([status('resolving', false).stage, status('resolving', false).title], ['entering', '正在查找房间「Studio」'])
  assert.equal(status('connecting', false).title, '正在连接房间「Studio」')
  assert.equal(status('choosing', true).stage, 'choosing')
  assert.equal(status('joined', true).stage, 'inside')
  assert.equal(status('joined', false).stage, 'entering')
  assert.match(status('contact_unavailable', true).detail, /已经建立的声音继续/u)
  assert.equal(status('contact_unavailable', true).stage, 'inside')
  assert.equal(status('contact_unavailable', false).stage, 'entering')
  assert.match(status('contact_unavailable', false).detail, /后台继续尝试/u)
  assert.doesNotMatch(status('contact_unavailable', false).detail, /已经建立的声音/u)
  assert.deepEqual([status('failed', false).stage, status('failed', false).title], ['failed', '没能进入房间「Studio」'])
  assert.equal(roomStatus({ phase: 'resolving', roomName: null, failure: null, failureKind: null }, false).title, '正在查找房间')
})

test('a candidate is chosen in one click with exact tokens, and a stale click submits nothing', () => {
  const choosing = { phase: 'choosing', generation, choices: studios }
  const entry = chooseEntry(choosing, 'instance-b')
  assert.equal(entry.action.type, 'choose_room')
  assert.equal(entry.action.instanceId, 'instance-b')
  assert.equal(entry.action.generation, '9007199254740993')
  for (const stale of [
    chooseEntry({ ...choosing, phase: 'connecting' }, 'instance-b'),
    chooseEntry(choosing, 'gone'),
  ]) {
    assert.equal(stale.action, null)
    assert.equal(JSON.stringify(stale).includes('choose_room'), false)
  }
})

test('background waiting is offered only after a failed manual join of the same saved card', () => {
  const card = 'C:/rooms/studio.json'
  const wait = backgroundWaitEntry('failed', card, card, 'unavailable')
  assert.equal(wait.action.type, 'join_saved_room')
  assert.equal(wait.action.mode, 'automatic')
  assert.equal(wait.action.profilePath, card)
  assert.equal(backgroundWaitEntry('failed', null, card, 'unavailable'), null)
  assert.equal(backgroundWaitEntry('failed', card, 'C:/rooms/other.json', 'unavailable'), null)
  assert.equal(backgroundWaitEntry('failed', card, null, 'unavailable'), null)
  assert.equal(backgroundWaitEntry('entering', card, card, 'unavailable'), null)
  for (const kind of [null, 'password_rejected', 'contact_rejected', 'configuration', 'listener_failed', 'discovery_failed', 'address_required', 'selection_expired']) {
    assert.equal(backgroundWaitEntry('failed', card, card, kind), null, kind)
  }
})

test('typed failures have Chinese explanations while unknown failures retain their original detail', () => {
  for (const kind of ['unavailable', 'password_rejected', 'contact_rejected', 'configuration', 'listener_failed', 'discovery_failed', 'address_required', 'selection_expired']) {
    const explanation = roomFailureText(kind, 'Raw failure detail')
    assert.match(explanation, /[\u4e00-\u9fff]/u)
    assert.doesNotMatch(explanation, /Raw failure/)
    const status = roomStatus({ phase: 'failed', roomName: 'Studio', failureKind: kind, failure: 'Raw failure detail' }, false)
    assert.equal(status.detail, explanation)
  }
  assert.equal(roomFailureText(null, 'Raw failure detail'), 'Raw failure detail')
  assert.equal(roomFailureText(null, null), null)
})

test('clearing the address also clears the strict address condition', () => {
  const draft = { name: 'Studio', address: '192.0.2.10', port: '', password: '', strictAutoJoin: true }
  assert.equal(withAddressCondition(draft), draft)
  assert.equal(withAddressCondition({ ...draft, address: '  ' }).strictAutoJoin, false)
  const plain = { ...draft, address: '', strictAutoJoin: false }
  assert.equal(withAddressCondition(plain), plain)
})
