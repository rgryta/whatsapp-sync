// Builds crates/client/tests/fixtures/media.json: a voice note encrypted by WAHA's own Baileys
// (`encryptedStream`, what a phone's upload amounts to), with its media key and hashes. The
// Rust client must decrypt it back to the same bytes and refuse it when the MAC is wrong.
//
//   docker run --rm --entrypoint node -v "$PWD/out:/out" \
//     -v "$PWD/scripts/baileys-media-fixture.mjs:/gen.mjs:ro" devlikeapro/waha:noweb-2026.10.1 /gen.mjs

import fs from 'node:fs/promises'
import { randomBytes } from 'node:crypto'
import { encryptedStream } from '/app/node_modules/@adiwajshing/baileys/lib/Utils/messages-media.js'

// Not real audio: the cipher does not care, and a fixture of noise holds nobody's voice.
const plaintext = randomBytes(3001)
const { mediaKey, encFilePath, fileEncSha256, fileSha256, fileLength } = await encryptedStream(plaintext, 'audio')
const encrypted = await fs.readFile(encFilePath)
await fs.rm(encFilePath, { force: true })
await fs.writeFile(
	'/out/media.json',
	JSON.stringify({
		mediaKey: Buffer.from(mediaKey).toString('base64'),
		fileEncSha256: Buffer.from(fileEncSha256).toString('base64'),
		fileSha256: Buffer.from(fileSha256).toString('base64'),
		fileLength: Number(fileLength),
		encrypted: encrypted.toString('base64'),
		plaintext: plaintext.toString('base64')
	}, null, 1)
)
console.log(`encrypted ${plaintext.length} bytes to ${encrypted.length}`)
