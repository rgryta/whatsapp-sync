// Builds crates/client/tests/fixtures/content.json: message protobufs and the archive columns
// WAHA and the Python ingester make of them.
//
// `body`, `hasMedia` and the reply id come from WAHA's own code (`extractBody`,
// `extractMediaContent`, `extractReplyTo`), run on the message as Baileys decodes it. `kind`
// and the media columns follow the ingester's `to_message`, which takes the first key of WAHA's
// `_data.message` that names a kind: that is the decoded object's key order, read here the same
// way.
//
//   docker run --rm --entrypoint node -v "$PWD/out:/out" \
//     -v "$PWD/scripts/baileys-content-fixture.mjs:/gen.mjs:ro" devlikeapro/waha:noweb-2026.10.1 /gen.mjs

import fs from 'node:fs/promises'
import { createRequire } from 'node:module'
import { proto } from '/app/node_modules/@adiwajshing/baileys/WAProto/index.js'

const require = createRequire('/app/dist/')
// WAHA loads Baileys lazily at startup; its helpers need that done first.
await require('/app/dist/vendor/esm.js').loadESMModules()
const core = require('/app/dist/core/engines/noweb/session.noweb.core.js')
const utils = require('/app/dist/core/engines/noweb/utils.js')
const extractReplyTo = core.WhatsappSessionNoWebCore.prototype.extractReplyTo

// The ingester's `_KINDS`, in its order.
const KINDS = {
	imageMessage: 'image', videoMessage: 'video', audioMessage: 'audio', documentMessage: 'document',
	stickerMessage: 'sticker', locationMessage: 'location', liveLocationMessage: 'location',
	contactMessage: 'contact', albumMessage: 'album'
}
const MEDIA = new Set(['image', 'video', 'audio', 'document', 'sticker'])

const quoted = { stanzaId: '3EB0QUOTED', participant: '111222333444555@lid', quotedMessage: { conversation: 'the original' } }
const key = { remoteJid: '111222333444555@lid', fromMe: false, id: '3EB0TARGET' }
const skdm = { groupId: '120363000000000001@g.us', axolotlSenderKeyDistributionMessage: Buffer.from([0x33, 1, 2]) }
const cases = {
	'plain text': { conversation: 'hello' },
	'extended text replying': { extendedTextMessage: { text: 'a reply', contextInfo: quoted } },
	'image with caption': { imageMessage: { mimetype: 'image/jpeg', caption: 'look' } },
	'image replying': { imageMessage: { mimetype: 'image/jpeg', contextInfo: quoted } },
	'image in a disappearing chat': { ephemeralMessage: { message: { imageMessage: { mimetype: 'image/jpeg', caption: 'gone soon' } } } },
	'text in a disappearing chat, replying': { ephemeralMessage: { message: { extendedTextMessage: { text: 'eph reply', contextInfo: quoted } } } },
	'video': { videoMessage: { mimetype: 'video/mp4', seconds: 12, caption: 'clip' } },
	'voice note': { audioMessage: { mimetype: 'audio/ogg; codecs=opus', seconds: 7, PTT: true } },
	'document with name and caption': { documentMessage: { mimetype: 'application/pdf', fileName: 'a.pdf', title: 'A', caption: 'see attached' } },
	'document with title only': { documentMessage: { mimetype: 'text/plain', title: 'notes' } },
	'document wrapped with caption': { documentWithCaptionMessage: { message: { documentMessage: { mimetype: 'application/pdf', fileName: 'b.pdf', caption: 'wrapped caption' } } } },
	'sticker': { stickerMessage: { mimetype: 'image/webp' } },
	'view-once image': { viewOnceMessageV2: { message: { imageMessage: { mimetype: 'image/jpeg', caption: 'once' } } } },
	'location': { locationMessage: { degreesLatitude: 52.2, name: 'here' } },
	'contact card': { contactMessage: { displayName: 'Someone' } },
	'album': { albumMessage: { expectedImageCount: 3 } },
	'key distribution and text': { senderKeyDistributionMessage: skdm, conversation: 'first in group' },
	'key distribution only': { senderKeyDistributionMessage: skdm },
	'edit': { protocolMessage: { key, type: 14, editedMessage: { conversation: 'fixed typo' }, timestampMs: 1700000000000 } },
	'edit, wrapped': { editedMessage: { message: { protocolMessage: { key, type: 14, editedMessage: { extendedTextMessage: { text: 'fixed again' } } } } } },
	'revoke': { protocolMessage: { key, type: 0 } },
	// What a phone may send: REVOKE is 0, the default, so the type is left off the wire.
	'revoke, type left off': { protocolMessage: { key } },
	'reaction': { reactionMessage: { key, text: '👍', senderTimestampMs: 1700000000123 } },
	'reaction removed': { reactionMessage: { key, text: '', senderTimestampMs: 1700000000456 } },
	// Two content fields at once: rare, but which one counts is a real rule on each side.
	'image and location together': { imageMessage: { mimetype: 'image/png' }, locationMessage: { degreesLatitude: 1 } },
	'conversation beside a reply': { conversation: 'plain', extendedTextMessage: { text: 'rich', contextInfo: quoted } },
	'sent from own phone': { deviceSentMessage: { destinationJid: '111222333444555@lid', message: { conversation: 'from my phone' } } }
}

const out = []
for (const [name, object] of Object.entries(cases)) {
	const bytes = Buffer.from(proto.Message.encode(proto.Message.fromObject(object)).finish())
	// As Baileys' decryptMessageNode leaves it: a phone's own message unwrapped.
	let message = proto.Message.decode(bytes)
	if (message.deviceSentMessage?.message) message = message.deviceSentMessage.message
	const envelope = JSON.parse(JSON.stringify(message))
	const kindKey = Object.keys(envelope).find(k => k in KINDS)
	const kind = KINDS[kindKey] ?? 'text'
	const media = kindKey ? envelope[kindKey] : {}
	const body = core.extractBody(message) || null
	const seconds = media?.seconds
	out.push({
		name,
		bytes: bytes.toString('base64'),
		body: body || media?.caption || null,
		kind,
		replyToId: extractReplyTo.call(null, message)?.id ?? null,
		hasMedia: Boolean(utils.extractMediaContent(message)) || MEDIA.has(kind),
		mediaMime: media?.mimetype ?? null,
		mediaFilename: media?.fileName || media?.title || null,
		mediaSeconds: typeof seconds === 'number' ? seconds : null
	})
}
await fs.writeFile('/out/content.json', JSON.stringify(out, null, 1))
console.log(`${out.length} messages`)
