// Builds tests/fixtures/baileys for crates/client: a Baileys multi-file auth directory
// for a made-up device "alice", plus messages to it that Baileys has NOT decrypted yet.
// The importer passes only if Rust then decrypts them from the imported state.
//
// It must run on WAHA's own Baileys and libsignal, so it runs inside WAHA's image:
//
//   docker run --rm --entrypoint node -v "$PWD/out:/out" \
//     -v "$PWD/scripts/baileys-fixture.mjs:/gen.mjs:ro" devlikeapro/waha:noweb-2026.10.1 /gen.mjs
//
// Every key in the output is generated here and belongs to nobody.

import fs from 'node:fs/promises'
import { makeLibSignalRepository } from '/app/node_modules/@adiwajshing/baileys/lib/Signal/libsignal.js'
import {
	addTransactionCapability,
	Curve,
	encodeWAMessage,
	generateSignalPubKey,
	initAuthCreds,
	proto,
	useMultiFileAuthState
} from '/app/node_modules/@adiwajshing/baileys/lib/index.js'

const OUT = '/out'
const ALICE = '222222222222222:3@lid'
const BOB = '111111111111111@lid'
const CAROL = '333333333333333@lid'
const GROUP = '120363000000000001@g.us'

const logger = { trace() {}, debug() {}, info() {}, warn() {}, error() {}, child() { return logger } }
const tx = keys => addTransactionCapability(keys, logger, { maxCommitRetries: 1, delayBetweenTriesMs: 1 })

function memoryKeys() {
	const data = {}
	return {
		get: async (type, ids) => Object.fromEntries(ids.filter(id => data[type]?.[id] != null).map(id => [id, data[type][id]])),
		set: async update => {
			for (const [type, items] of Object.entries(update)) {
				data[type] ??= {}
				for (const [id, value] of Object.entries(items)) {
					if (value == null) delete data[type][id]
					else data[type][id] = value
				}
			}
		}
	}
}

await fs.rm(`${OUT}/alice`, { recursive: true, force: true })
const multi = await useMultiFileAuthState(`${OUT}/alice`)
const alice = { creds: initAuthCreds(), keys: tx(multi.state.keys) }
Object.assign(multi.state.creds, alice.creds)
alice.creds = multi.state.creds

// A phone ("account") that signed alice in, the way pairing would have.
const phone = Curve.generateKeyPair()
const details = Buffer.from(proto.ADVDeviceIdentity.encode({ rawId: 7, timestamp: 1700000000, keyIndex: 3 }).finish())
const identity = alice.creds.signedIdentityKey.public
const accountSignature = Curve.sign(phone.private, Buffer.concat([Buffer.from([6, 0]), details, identity]))
const deviceSignature = Curve.sign(
	alice.creds.signedIdentityKey.private,
	Buffer.concat([Buffer.from([6, 1]), details, identity, phone.public])
)
// WAHA's creds.json holds these four as base64 strings, not Buffers, so this does too.
alice.creds.account = {
	details: details.toString('base64'),
	accountSignatureKey: phone.public.toString('base64'),
	accountSignature: Buffer.from(accountSignature).toString('base64'),
	deviceSignature: Buffer.from(deviceSignature).toString('base64')
}
alice.creds.me = { id: '48000000000:3@s.whatsapp.net', lid: ALICE, name: 'Alice' }
alice.creds.registered = true

const preKeys = {}
for (let id = 1; id <= 5; id++) preKeys[id] = Curve.generateKeyPair()
await alice.keys.transaction(() => alice.keys.set({ 'pre-key': preKeys }), 'prekeys')
alice.creds.nextPreKeyId = 6
alice.creds.firstUnuploadedPreKeyId = 6
await multi.saveCreds()

const aliceRepo = makeLibSignalRepository(alice, logger)
const bob = { creds: initAuthCreds(), keys: tx(memoryKeys()) }
const bobRepo = makeLibSignalRepository(bob, logger)

// Bob starts the session from alice's published bundle, as any contact's phone would.
await bobRepo.injectE2ESession({
	jid: ALICE,
	session: {
		registrationId: alice.creds.registrationId,
		identityKey: generateSignalPubKey(identity),
		signedPreKey: {
			keyId: alice.creds.signedPreKey.keyId,
			publicKey: generateSignalPubKey(alice.creds.signedPreKey.keyPair.public),
			signature: alice.creds.signedPreKey.signature
		},
		preKey: { keyId: 2, publicKey: generateSignalPubKey(preKeys[2].public) }
	}
})

// Each message is what a phone sends: a `Message` protobuf, randomly padded.
const wire = name => encodeWAMessage(proto.Message.fromObject({ conversation: name }))
const messages = []
let carolFirst
const sent = {}
const send = async name => {
	sent[name] = await bobRepo.encryptMessage({ jid: ALICE, data: wire(name) })
}
const read = name => aliceRepo.decryptMessage({ jid: BOB, ...sent[name] })
const record = (name, expect) =>
	messages.push({
		kind: 'pairwise',
		name,
		id: `P-${name}`,
		sender: BOB,
		type: sent[name].type,
		ciphertext: sent[name].ciphertext.toString('base64'),
		expect
	})

for (const name of ['m1', 'm2', 'm3', 'm4', 'm5', 'm6']) await send(name)
for (const name of ['m1', 'm2', 'm5']) await read(name)

// Alice answers once, so Bob ratchets and m7/m8 arrive on a chain alice has not seen.
const reply = await aliceRepo.encryptMessage({ jid: BOB, data: wire('reply') })
await bobRepo.decryptMessage({ jid: ALICE, ...reply })
for (const name of ['m7', 'm8']) await send(name)

// The order Rust decrypts in: a new chain first, then the old chain's skipped and next keys.
record('m7', 'plaintext')
record('m3', 'plaintext')
record('m6', 'plaintext')
record('m4', 'plaintext')
record('m8', 'plaintext')
record('m2', 'duplicate')

// Carol's first messages reach alice only after the import: Rust, not Baileys, has to start
// that session from the X3DH handshake, using pre-key 3.
const carol = { creds: initAuthCreds(), keys: tx(memoryKeys()) }
const carolRepo = makeLibSignalRepository(carol, logger)
await carolRepo.injectE2ESession({
	jid: ALICE,
	session: {
		registrationId: alice.creds.registrationId,
		identityKey: generateSignalPubKey(identity),
		signedPreKey: {
			keyId: alice.creds.signedPreKey.keyId,
			publicKey: generateSignalPubKey(alice.creds.signedPreKey.keyPair.public),
			signature: alice.creds.signedPreKey.signature
		},
		preKey: { keyId: 3, publicKey: generateSignalPubKey(preKeys[3].public) }
	}
})
for (const name of ['c1', 'c2']) {
	const sent = await carolRepo.encryptMessage({ jid: ALICE, data: wire(name) })
	messages.push({ kind: 'pairwise', name, id: `P-${name}`, sender: CAROL, type: sent.type, ciphertext: sent.ciphertext.toString('base64'), expect: 'plaintext' })
	if (name === 'c1') carolFirst = { ...sent }
}
messages.push({ kind: 'pairwise', name: 'c1', id: 'P-c1', sender: CAROL, type: carolFirst.type, ciphertext: carolFirst.ciphertext.toString('base64'), expect: 'duplicate' })

const groupSent = {}
for (const name of ['g1', 'g2', 'g3', 'g4', 'g5']) {
	groupSent[name] = await bobRepo.encryptGroupMessage({ group: GROUP, meId: BOB, data: wire(name) })
}
await aliceRepo.processSenderKeyDistributionMessage({
	authorJid: BOB,
	item: { groupId: GROUP, axolotlSenderKeyDistributionMessage: groupSent.g1.senderKeyDistributionMessage }
})
for (const name of ['g1', 'g3']) {
	await aliceRepo.decryptGroupMessage({ group: GROUP, authorJid: BOB, msg: groupSent[name].ciphertext })
}
for (const [name, expect] of [['g2', 'plaintext'], ['g4', 'plaintext'], ['g5', 'plaintext'], ['g1', 'duplicate']]) {
	messages.push({
		kind: 'group',
		name,
		id: `G-${name}`,
		sender: BOB,
		group: GROUP,
		ciphertext: Buffer.from(groupSent[name].ciphertext).toString('base64'),
		expect
	})
}

await fs.writeFile(`${OUT}/messages.json`, JSON.stringify(messages, null, 2) + '\n')
console.log(`alice: ${(await fs.readdir(`${OUT}/alice`)).length} files, ${messages.length} messages`)
