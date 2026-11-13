// Builds crates/client/tests/fixtures/secret_edit.json: an edit sealed the way current phones
// seal one (whatsmeow `msgsecret.go`: HKDF-SHA256 over the original's secret, AES-256-GCM, no
// associated data), made here with Node's own crypto and WAHA's protobuf definitions, so the
// Rust side is checked against an implementation it shares no code with.
//
//   docker run --rm --entrypoint node -v "$PWD/out:/out" \
//     -v "$PWD/scripts/secret-edit-fixture.mjs:/gen.mjs:ro" devlikeapro/waha:noweb-2026.10.1 /gen.mjs

import fs from 'node:fs/promises'
import { createCipheriv, hkdfSync, randomBytes } from 'node:crypto'
import { proto } from '/app/node_modules/@adiwajshing/baileys/WAProto/index.js'

const b64 = (b) => Buffer.from(b).toString('base64')
const secret = randomBytes(32)
const originalId = '3EB0C0FFEE1234567890'
// Made-up accounts: the original was sent from the phone number form, the edit from the LID.
const originalSender = '15550001111@s.whatsapp.net'
const editor = '111111111111111@lid'

// The original, as our own phone's message reaches a linked device: inside deviceSentMessage,
// with the secret on the OUTER envelope.
const original = proto.Message.encode({
	deviceSentMessage: { destinationJid: originalSender, message: { conversation: 'teh original' } },
	messageContextInfo: { messageSecret: secret }
}).finish()

const sealed = proto.Message.encode({
	protocolMessage: {
		key: { remoteJid: originalSender, fromMe: true, id: originalId },
		type: proto.Message.ProtocolMessage.Type.MESSAGE_EDIT,
		editedMessage: { conversation: 'the original, fixed' }
	}
}).finish()
const info = Buffer.concat([originalId, originalSender, editor, 'Message Edit'].map((s) => Buffer.from(s)))
const key = Buffer.from(hkdfSync('sha256', secret, Buffer.alloc(0), info, 32))
const iv = randomBytes(12)
const cipher = createCipheriv('aes-256-gcm', key, iv)
const payload = Buffer.concat([cipher.update(sealed), cipher.final(), cipher.getAuthTag()])

const edit = proto.Message.encode({
	secretEncryptedMessage: {
		targetMessageKey: { remoteJid: originalSender, fromMe: true, id: originalId },
		encPayload: payload,
		encIv: iv,
		secretEncType: proto.Message.SecretEncryptedMessage.SecretEncType.MESSAGE_EDIT
	},
	messageContextInfo: { messageSecret: randomBytes(32) }
}).finish()

await fs.writeFile('/out/secret_edit.json', JSON.stringify({
	secret: b64(secret), originalId, originalSender, editor,
	original: b64(original), edit: b64(edit), iv: b64(iv), payload: b64(payload),
	body: 'the original, fixed'
}, null, 1))
console.log('secret edit fixture written')
