// Builds crates/client/tests/fixtures/stanza.json: what WAHA's own Baileys sends back for what
// it receives (acks, receipts), how it reads a message stanza, and its pre-key upload.
//
// Everything is Baileys' own code: buildAckStanza, decodeMessageNode and getNextPreKeysNode are
// exported; sendReceipt is not, so its source is cut out of messages-send.js and run as is.
//
//   docker run --rm --entrypoint node -v "$PWD/out:/out" \
//     -v "$PWD/scripts/baileys-stanza-fixture.mjs:/gen.mjs:ro" devlikeapro/waha:noweb-2026.10.1 /gen.mjs

import fs from 'node:fs/promises'
import { createRequire } from 'node:module'
import { Boom } from '/app/node_modules/@hapi/boom/lib/index.js'
import { buildAckStanza } from '/app/node_modules/@adiwajshing/baileys/lib/Utils/stanza-ack.js'
import { decodeMessageNode } from '/app/node_modules/@adiwajshing/baileys/lib/Utils/decode-wa-message.js'
import { getNextPreKeysNode } from '/app/node_modules/@adiwajshing/baileys/lib/Utils/signal.js'
import { Curve, signedKeyPair } from '/app/node_modules/@adiwajshing/baileys/lib/Utils/crypto.js'
import { unixTimestampSeconds } from '/app/node_modules/@adiwajshing/baileys/lib/Utils/generics.js'
import { decodeBinaryNode, encodeBinaryNode, isLidUser, isPnUser } from '/app/node_modules/@adiwajshing/baileys/lib/WABinary/index.js'
import { cleanMessage } from '/app/node_modules/@adiwajshing/baileys/lib/Utils/process-message.js'

// WAHA's own id helpers, for the ids the archive holds.
const require = createRequire('/app/dist/')
await require('/app/dist/vendor/esm.js').loadESMModules()
const { buildMessageId } = require('/app/dist/core/engines/noweb/utils.js')
const { toCusFormat } = require('/app/dist/core/utils/jids.js')

const b64 = b => Buffer.from(b).toString('base64')
const meId = '48000000000:3@s.whatsapp.net'
const meLid = '222222222222222:3@lid'
const enc = { tag: 'enc', attrs: { v: '2', type: 'msg' }, content: Buffer.from([1, 2, 3]) }

// Stanzas as the server sends them, from string attributes the way Baileys encodes them.
const incoming = {
	'pn contact device': { tag: 'message', attrs: { from: '48111111111:2@s.whatsapp.net', id: 'A1', t: '1700000000', type: 'text', notify: 'Bob' }, content: [enc] },
	'pn contact, explicit device 0': { tag: 'message', attrs: { from: '48111111111:0@s.whatsapp.net', id: 'A0', t: '1699999999', type: 'text' }, content: [enc] },
	'lid contact': { tag: 'message', attrs: { from: '111222333444555@lid', id: 'A2', t: '1700000001', type: 'text' }, content: [enc] },
	'group from lid member': { tag: 'message', attrs: { from: '120363000000000001@g.us', participant: '111222333444555:4@lid', id: 'A3', t: '1700000002', type: 'text', addressing_mode: 'lid' }, content: [enc] },
	'own phone to pn chat': { tag: 'message', attrs: { from: '48000000000@s.whatsapp.net', recipient: '48111111111@s.whatsapp.net', id: 'A4', t: '1700000003', type: 'text' }, content: [enc] },
	'own phone to lid chat': { tag: 'message', attrs: { from: '222222222222222@lid', recipient: '111222333444555@lid', id: 'A5', t: '1700000004', type: 'text' }, content: [enc] },
	'own device peer message': { tag: 'message', attrs: { from: '48000000000@s.whatsapp.net', id: 'A6', t: '1700000005', type: 'text', category: 'peer' }, content: [enc] },
	'status from contact': { tag: 'message', attrs: { from: 'status@broadcast', participant: '48111111111@s.whatsapp.net', id: 'A7', t: '1700000006', type: 'media' }, content: [enc] },
	'own group message': { tag: 'message', attrs: { from: '120363000000000001@g.us', participant: '222222222222222:3@lid', id: 'A8', t: '1700000007', type: 'text' }, content: [enc] },
	'newsletter': { tag: 'message', attrs: { from: '120363000000000002@newsletter', id: 'A9', t: '1700000008', type: 'text', server_id: '77' }, content: [] },
	'read receipt': { tag: 'receipt', attrs: { from: '48111111111:2@s.whatsapp.net', id: 'R1', type: 'read', t: '1700000009' } },
	'group delivery receipt': { tag: 'receipt', attrs: { from: '120363000000000001@g.us', participant: '111222333444555:4@lid', id: 'R2', t: '1700000010' } },
	'encrypt notification': { tag: 'notification', attrs: { from: 's.whatsapp.net', id: 'N1', type: 'encrypt', t: '1700000011' } },
	'call offer': { tag: 'call', attrs: { from: '48111111111:2@s.whatsapp.net', id: 'C1', t: '1700000012' } }
}

const acks = []
const decoded = []
for (const [name, sent] of Object.entries(incoming)) {
	// What Baileys' handlers see is the stanza decoded off the wire, not the strings it was
	// built from: decoding drops an explicit `:0` device, for one.
	const wire = encodeBinaryNode(sent)
	const node = await decodeBinaryNode(wire)
	for (const error of [undefined, 500]) {
		acks.push({ name, error: error ?? null, input: b64(wire), output: b64(encodeBinaryNode(buildAckStanza(node, error, meId))) })
	}
	if (node.tag === 'message') {
		try {
			const { fullMessage, author } = decodeMessageNode(node, meId, meLid)
			const k = fullMessage.key
			const entry = { name, input: b64(wire), chat: k.remoteJid, fromMe: k.fromMe, participant: k.participant ?? null, author, category: fullMessage.category ?? null }
			// What WAHA hands the ingester: the cleaned key, its message id, and the author the
			// ingester's `author_of` picks (WAHA's `participant` in a group, else its `from`).
			cleanMessage(fullMessage, meId, meLid)
			const isGroup = Boolean(fullMessage.key.participant)
			entry.wahaMessageId = buildMessageId(fullMessage.key)
			entry.wahaAuthor = isGroup ? toCusFormat(fullMessage.key.participant) : (k.remoteJid.endsWith('@g.us') ? null : toCusFormat(fullMessage.key.remoteJid))
			decoded.push(entry)
		} catch (e) {
			decoded.push({ name, input: b64(wire), error: e.message })
		}
	}
}

// What an edit or reaction points at: Baileys' cleanMessage (with its normaliseKey for
// reactions) over the outer stanza, then WAHA's buildMessageId over the resulting target key.
const bob = '111222333444555@lid'
const group = '120363000000000001@g.us'
const targets = []
for (const [name, attrs, kind, key] of [
	['bob reacts in a DM to my message', { from: bob }, 'reaction', { remoteJid: meLid.replace(':3', ''), fromMe: false, id: 'T1' }],
	['bob reacts in a DM to his own message', { from: bob }, 'reaction', { remoteJid: meLid.replace(':3', ''), fromMe: true, id: 'T2' }],
	['carol reacts in a group to bob', { from: group, participant: '333444555666777@lid' }, 'reaction', { remoteJid: group, fromMe: false, id: 'T3', participant: bob }],
	['carol reacts in a group to me', { from: group, participant: '333444555666777@lid' }, 'reaction', { remoteJid: group, fromMe: false, id: 'T4', participant: '222222222222222@lid' }],
	['my phone reacts in a group to bob', { from: group, participant: '222222222222222@lid' }, 'reaction', { remoteJid: group, fromMe: false, id: 'T5', participant: bob }],
	['bob edits his message in a group', { from: group, participant: bob }, 'edit', { remoteJid: group, fromMe: true, id: 'T6' }]
]) {
	const sent = { tag: 'message', attrs: { ...attrs, id: 'OUTER', t: '1700000100', type: 'text' }, content: [enc] }
	const wire = encodeBinaryNode(sent)
	const { fullMessage } = decodeMessageNode(await decodeBinaryNode(wire), meId, meLid)
	fullMessage.message = kind === 'reaction' ? { reactionMessage: { key: { ...key }, text: 'x' } } : { protocolMessage: { key: { ...key }, type: 14 } }
	cleanMessage(fullMessage, meId, meLid)
	const expected = kind === 'reaction'
		? buildMessageId(fullMessage.message.reactionMessage.key)
		: buildMessageId({ ...fullMessage.key, id: key.id })
	targets.push({ name, kind, input: b64(wire), key, expected })
}

// sendReceipt, cut from Baileys' source and run with a sendNode that records the node.
const source = await fs.readFile('/app/node_modules/@adiwajshing/baileys/lib/Socket/messages-send.js', 'utf8')
const start = source.indexOf('const sendReceipt = async')
const end = source.indexOf('/** Correctly bulk send receipts', start)
const body = source.slice(start, end).trim()
let sent
const logger = { debug() {} }
const sendNode = async n => { sent = n }
const sendReceipt = new Function('Boom', 'unixTimestampSeconds', 'isPnUser', 'isLidUser', 'logger', 'sendNode', `${body}\nreturn sendReceipt`)(
	Boom, unixTimestampSeconds, isPnUser, isLidUser, logger, sendNode)
const receipts = []
for (const [name, args] of Object.entries({
	'delivery, inactive': ['48111111111@s.whatsapp.net', undefined, ['A1'], 'inactive'],
	'group delivery': ['120363000000000001@g.us', '111222333444555:4@lid', ['A3'], 'inactive'],
	'sender, pn chat, no participant': ['48111111111@s.whatsapp.net', undefined, ['A4'], 'sender'],
	'sender, lid chat': ['111222333444555@lid', '222222222222222@lid', ['A5'], 'sender'],
	'peer': ['48000000000@s.whatsapp.net', undefined, ['A6'], 'peer_msg'],
	'several ids': ['48111111111@s.whatsapp.net', undefined, ['B1', 'B2', 'B3'], 'inactive'],
	'history sync': ['48000000000@s.whatsapp.net', undefined, ['B4'], 'hist_sync']
})) {
	await sendReceipt(...args)
	receipts.push({ name, chat: args[0], participant: args[1] ?? null, ids: args[2], type: args[3], output: b64(encodeBinaryNode(sent)) })
}

// The pre-key upload, from a key store that already holds every key it asks for.
const identity = Curve.generateKeyPair()
const signedPreKey = signedKeyPair(identity, 7)
const store = {}
for (let id = 10; id < 20; id++) store[id] = Curve.generateKeyPair()
const keys = {
	get: async (type, ids) => Object.fromEntries(ids.filter(id => store[id]).map(id => [id, store[id]])),
	set: async () => {}
}
const creds = { registrationId: 4321, signedIdentityKey: identity, signedPreKey, nextPreKeyId: 20, firstUnuploadedPreKeyId: 10 }
const { update, node: uploadNode } = await getNextPreKeysNode({ creds, keys }, 5)

await fs.writeFile(
	'/out/stanza.json',
	JSON.stringify(
		{
			meId,
			meLid,
			acks,
			decoded,
			receipts,
			targets,
			preKeyUpload: {
				registrationId: creds.registrationId,
				identityPublic: b64(identity.public),
				signedPreKey: { id: 7, public: b64(signedPreKey.keyPair.public), private: b64(signedPreKey.keyPair.private), signature: b64(signedPreKey.signature) },
				preKeys: Object.fromEntries(Object.entries(store).map(([id, kp]) => [id, { public: b64(kp.public), private: b64(kp.private) }])),
				nextPreKeyId: creds.nextPreKeyId,
				firstUnuploadedPreKeyId: creds.firstUnuploadedPreKeyId,
				count: 5,
				output: b64(encodeBinaryNode(uploadNode)),
				update
			}
		},
		null,
		1
	)
)
console.log(`${acks.length} acks, ${decoded.length} decoded, ${receipts.length} receipts, 1 upload`)
