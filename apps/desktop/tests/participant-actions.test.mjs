import assert from 'node:assert/strict'
import test from 'node:test'
import {
  chooseRoomAction,
  createRoomAction,
  editMemberDirectionAction,
  pollParticipantOnce,
} from '../src/participant/bridge.ts'

test('member direction helpers preserve the selector and omit the other direction', () => {
  const send = JSON.parse(JSON.stringify(editMemberDirectionAction({ name: 'Ada' }, 'send', false)))
  assert.deepEqual(send, {
    type: 'edit_room_rule', rule: { match: { name: 'Ada' }, send: false },
  })
  const listen = JSON.parse(JSON.stringify(editMemberDirectionAction({ ip: '192.0.2.10' }, 'listen', true)))
  assert.deepEqual(listen, {
    type: 'edit_room_rule', rule: { match: { ip: '192.0.2.10' }, listen: true },
  })
})

test('room helpers preserve the exact choice token and never infer the saved address', () => {
  const generation = '9007199254740993'
  assert.equal(chooseRoomAction(generation, 'instance').generation, generation)
  const profile = {
    schema: 1, room: { name: 'Studio', address: null, password: '', strict_auto_join: false },
    rules: [],
  }
  const action = createRoomAction(profile, '0.0.0.0:47101')
  assert.equal(action.bind, '0.0.0.0:47101')
  assert.equal(action.profile.room.address, null)
})

test('the actual poll completion cannot overwrite an intervening command or an unmounted view', async () => {
  const generation = { current: 4 }
  let state = 'before'
  let complete
  const pending = pollParticipantOnce(() => new Promise((resolve) => { complete = resolve }),
    generation, () => true, (next) => { state = next })
  generation.current += 1
  state = 'command result'
  complete('old poll')
  await pending
  assert.equal(state, 'command result')
  await pollParticipantOnce(async () => 'fresh poll', generation, () => true, (next) => { state = next })
  assert.equal(state, 'fresh poll')
  await pollParticipantOnce(async () => 'late result', generation, () => false, (next) => { state = next })
  assert.equal(state, 'fresh poll')
})
