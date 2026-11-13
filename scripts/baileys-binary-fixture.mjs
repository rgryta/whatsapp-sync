// Builds crates/client/tests/fixtures/binary.json: WhatsApp's binary node encoding as WAHA's
// own Baileys writes and reads it. The Rust codec passes only if it decodes every frame to the
// structure Baileys decodes, and encodes that structure back to the identical bytes.
//
// Runs inside WAHA's image, like baileys-fixture.mjs:
//
//   docker run --rm --entrypoint node -v "$PWD/out:/out" \
//     -v "$PWD/scripts/baileys-binary-fixture.mjs:/gen.mjs:ro" devlikeapro/waha:noweb-2026.10.1 /gen.mjs

import fs from 'node:fs/promises'
import * as constants from '/app/node_modules/@adiwajshing/baileys/lib/WABinary/constants.js'
import { encodeBinaryNode, decodeBinaryNode } from '/app/node_modules/@adiwajshing/baileys/lib/WABinary/index.js'
import { WAJIDDomains } from '/app/node_modules/@adiwajshing/baileys/lib/WABinary/jid-utils.js'

// Baileys' decoded node, as plain JSON: bytes become {bytes: base64}, token or packed content
// stays a string.
const canonical = node => ({
	tag: node.tag,
	attrs: Object.entries(node.attrs || {}),
	content:
		node.content === undefined || node.content === null
			? null
			: Array.isArray(node.content)
				? node.content.map(canonical)
				: typeof node.content === 'string'
					? { text: node.content }
					: { bytes: Buffer.from(node.content).toString('base64') }
})

const cases = []
const add = async (name, node) => {
	const bytes = encodeBinaryNode(node)
	cases.push({ name, bytes: Buffer.from(bytes).toString('base64'), node: canonical(await decodeBinaryNode(bytes)) })
}

await add('iq with a child', {
	tag: 'iq',
	attrs: { id: '1234.5678-1', to: 's.whatsapp.net', type: 'get', xmlns: 'encrypt' },
	content: [{ tag: 'count', attrs: {} }]
})
await add('group message from a LID device', {
	tag: 'message',
	attrs: {
		from: '120363000000000001@g.us',
		participant: '111111111111111:3@lid',
		id: '3EB0C431A2B4D5E6F708',
		type: 'text',
		t: '1700000000',
		notify: 'Bob'
	},
	content: [{ tag: 'enc', attrs: { v: '2', type: 'skmsg' }, content: Buffer.from([0x33, 0x0a, 1, 2, 3, 255]) }]
})
await add('jids of every shape', {
	tag: 'jids',
	attrs: {
		phone_device: '48000000000:3@s.whatsapp.net',
		phone_device_zero: '48000000000:0@s.whatsapp.net',
		phone: '48000000000@s.whatsapp.net',
		lid: '111111111111111@lid',
		hosted: '48000000000:99@hosted',
		hosted_lid: '111111111111111:99@hosted.lid',
		status: 'status@broadcast',
		server_only: 's.whatsapp.net',
		newsletter: '120363000000000002@newsletter'
	}
})
await add('strings: raw utf8, packed nibble, packed hex, odd lengths, past the pack limit', {
	tag: 'strings',
	attrs: {
		utf8: 'zażółć gęślą jaźń',
		nibble: '123-456.78',
		nibble_odd: '12345',
		hex: 'ABCDEF0123',
		hex_odd: 'ABC',
		lower_hex_is_raw: 'abcdef',
		long_digits: '1'.repeat(200)
	}
})
await add('token content', { tag: 'presence', attrs: { type: 'available' }, content: 'available' })
await add('bare tag', { tag: 'ping', attrs: {} })
await add('300 children (16-bit list length)', {
	tag: 'list',
	attrs: {},
	content: Array.from({ length: 300 }, (_, i) => ({ tag: 'item', attrs: { n: String(i) } }))
})
await add('70000 bytes (20-bit length)', { tag: 'blob', attrs: {}, content: Buffer.alloc(70000, 7) })

// Seeded random nodes over every value shape the encoder distinguishes.
let seed = 0x2545f491
const rand = n => {
	seed ^= seed << 13
	seed ^= seed >>> 17
	seed ^= seed << 5
	return (seed >>> 0) % n
}
const pick = list => list[rand(list.length)]
const singles = constants.SINGLE_BYTE_TOKENS.filter(Boolean)
const doubles = constants.DOUBLE_BYTE_TOKENS.flat()
const value = () =>
	pick([
		() => pick(singles),
		() => pick(doubles),
		() => String(rand(1e9)),
		() => rand(16 ** 6).toString(16).toUpperCase(),
		() => `${rand(1e12)}@s.whatsapp.net`,
		() => `${rand(1e12)}:${1 + rand(20)}@s.whatsapp.net`,
		() => `${rand(1e15)}:${rand(20)}@lid`,
		() => `${rand(1e15)}@g.us`,
		() => `word-${rand(1000)} ${pick(['ü', 'ß', 'ł', '日本', ''])}`
	])()
const randomNode = depth => {
	const attrs = {}
	for (let i = rand(5); i > 0; i--) attrs[`${pick(singles)}${rand(3) ? '' : '_x'}`] = value()
	const kind = depth > 2 ? rand(3) : rand(4)
	return {
		tag: pick(singles),
		attrs,
		content:
			kind === 0
				? undefined
				: kind === 1
					? Buffer.from(Array.from({ length: rand(300) }, () => rand(256)))
					: kind === 2
						? value()
						: Array.from({ length: 1 + rand(4) }, () => randomNode(depth + 1))
	}
}
for (let i = 0; i < 300; i++) await add(`random ${i}`, randomNode(0))

await fs.writeFile(
	'/out/binary.json',
	JSON.stringify({
		singleByteTokens: constants.SINGLE_BYTE_TOKENS,
		doubleByteTokens: constants.DOUBLE_BYTE_TOKENS,
		domains: WAJIDDomains,
		cases
	})
)
console.log(`${cases.length} cases`)
