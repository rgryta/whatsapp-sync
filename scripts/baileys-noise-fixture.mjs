// Builds crates/client/tests/fixtures/noise.json: one complete WhatsApp Noise handshake and a
// few transport frames, with WAHA's own Baileys as the client.
//
// The server half below is written here, since only WhatsApp has the real one. It is proven by
// Baileys' client completing the handshake against it, root-of-trust check included (against a
// root key made here, swapped in for WhatsApp's, which nobody outside WhatsApp can sign with).
// The Rust client, given the same keys and server reply, must produce identical bytes.
//
// Runs inside WAHA's image, like baileys-fixture.mjs:
//
//   docker run --rm --entrypoint node -v "$PWD/out:/out" \
//     -v "$PWD/scripts/baileys-noise-fixture.mjs:/gen.mjs:ro" devlikeapro/waha:noweb-2026.10.1 /gen.mjs

import fs from 'node:fs/promises'
import { proto } from '/app/node_modules/@adiwajshing/baileys/WAProto/index.js'
import { DEFAULT_CONNECTION_CONFIG, NOISE_MODE, NOISE_WA_HEADER, WA_CERT_DETAILS } from '/app/node_modules/@adiwajshing/baileys/lib/Defaults/index.js'
import { generateLoginNode } from '/app/node_modules/@adiwajshing/baileys/lib/Utils/validate-connection.js'
import { aesDecryptGCM, aesEncryptGCM, Curve, hkdf, sha256 } from '/app/node_modules/@adiwajshing/baileys/lib/Utils/crypto.js'
import { makeNoiseHandler } from '/app/node_modules/@adiwajshing/baileys/lib/Utils/noise-handler.js'
import { encodeBinaryNode } from '/app/node_modules/@adiwajshing/baileys/lib/WABinary/index.js'

const logger = { level: 'info', trace() {}, debug() {}, info() {}, warn() {}, error() {}, child() { return logger } }
const hex = b => Buffer.from(b).toString('hex')
const pair = kp => ({ private: hex(kp.private), public: hex(kp.public) })

const clientEphemeral = Curve.generateKeyPair()
const noiseKey = Curve.generateKeyPair()
const serverEphemeral = Curve.generateKeyPair()
const serverStatic = Curve.generateKeyPair()
const root = Curve.generateKeyPair()
const intermediate = Curve.generateKeyPair()
const routingInfo = Buffer.from([8, 8, 8, 2, 8, 5])
const now = 1794567392

// The certificate chain the server presents: root signs the intermediate, which signs the leaf
// naming the server's static key.
const details = (d) => Buffer.from(proto.CertChain.NoiseCertificate.Details.encode(d).finish())
const intermediateDetails = details({ serial: 7, issuerSerial: 0, key: intermediate.public, notBefore: now - 86400, notAfter: now + 86400 })
const leafDetails = details({ serial: 9, issuerSerial: 7, key: serverStatic.public, notBefore: now - 3600, notAfter: now + 3600 })
const certChain = Buffer.from(
	proto.CertChain.encode({
		intermediate: { details: intermediateDetails, signature: Curve.sign(root.private, intermediateDetails) },
		leaf: { details: leafDetails, signature: Curve.sign(intermediate.private, leafDetails) }
	}).finish()
)
WA_CERT_DETAILS.PUBLIC_KEY = Buffer.from(root.public)

// The server's half of the same symmetric state.
const makeServer = () => {
	let hash = Buffer.from(NOISE_MODE)
	let salt = hash
	let key = hash
	let counter = 0
	const iv = n => { const b = Buffer.alloc(12); b.writeUInt32BE(n, 8); return b }
	const mixHash = d => { hash = sha256(Buffer.concat([hash, d])) }
	const mixKey = d => { const k = hkdf(Buffer.from(d), 64, { salt, info: '' }); salt = k.subarray(0, 32); key = k.subarray(32); counter = 0 }
	return {
		mixHash,
		mixKey,
		encrypt: p => { const c = aesEncryptGCM(p, key, iv(counter++), hash); mixHash(c); return c },
		decrypt: c => { const p = aesDecryptGCM(c, key, iv(counter++), hash); mixHash(c); return p },
		split: () => { const k = hkdf(Buffer.alloc(0), 64, { salt, info: '' }); return { clientWrite: k.subarray(0, 32), clientRead: k.subarray(32) } }
	}
}
const server = makeServer()
// The server's reply for a given certificate chain, from a fresh server state.
const serverHelloWith = chain => {
	const s = makeServer()
	s.mixHash(NOISE_WA_HEADER)
	s.mixHash(clientEphemeral.public)
	s.mixHash(serverEphemeral.public)
	s.mixKey(Curve.sharedKey(serverEphemeral.private, clientEphemeral.public))
	const st = s.encrypt(serverStatic.public)
	s.mixKey(Curve.sharedKey(serverStatic.private, clientEphemeral.public))
	const pl = s.encrypt(chain)
	return hex(proto.HandshakeMessage.encode({ serverHello: { ephemeral: serverEphemeral.public, static: st, payload: pl } }).finish())
}
const chainOf = (interDetails, interSigner, leafDetailsBytes, leafSigner) =>
	Buffer.from(
		proto.CertChain.encode({
			intermediate: { details: interDetails, signature: Curve.sign(interSigner.private, interDetails) },
			leaf: { details: leafDetailsBytes, signature: Curve.sign(leafSigner.private, leafDetailsBytes) }
		}).finish()
	)
const other = Curve.generateKeyPair()
const badChains = {
	'intermediate has the wrong issuer serial': chainOf(
		details({ serial: 7, issuerSerial: 1, key: intermediate.public, notBefore: now - 86400, notAfter: now + 86400 }), root, leafDetails, intermediate),
	'leaf is not signed by the intermediate': chainOf(intermediateDetails, root, leafDetails, root),
	'leaf was not issued by this intermediate': chainOf(
		intermediateDetails, root, details({ serial: 9, issuerSerial: 8, key: serverStatic.public, notBefore: now - 3600, notAfter: now + 3600 }), intermediate),
	'leaf names a different key than the server holds': chainOf(
		intermediateDetails, root, details({ serial: 9, issuerSerial: 7, key: other.public, notBefore: now - 3600, notAfter: now + 3600 }), intermediate)
}

// Client hello, as Baileys' socket sends it.
const noise = makeNoiseHandler({ keyPair: clientEphemeral, NOISE_HEADER: NOISE_WA_HEADER, logger, routingInfo })
const hello = Buffer.from(proto.HandshakeMessage.encode({ clientHello: { ephemeral: clientEphemeral.public } }).finish())
const helloFrame = noise.encodeFrame(hello)

server.mixHash(NOISE_WA_HEADER)
server.mixHash(clientEphemeral.public)
server.mixHash(serverEphemeral.public)
server.mixKey(Curve.sharedKey(serverEphemeral.private, clientEphemeral.public))
const staticCiphertext = server.encrypt(serverStatic.public)
server.mixKey(Curve.sharedKey(serverStatic.private, clientEphemeral.public))
const certCiphertext = server.encrypt(certChain)
const serverHello = Buffer.from(
	proto.HandshakeMessage.encode({ serverHello: { ephemeral: serverEphemeral.public, static: staticCiphertext, payload: certCiphertext } }).finish()
)

// Baileys' client: verifies the chain, sends its noise key and the login payload.
// The login WAHA sends for a paired device: its socket config (session.noweb.core.js) is the
// Baileys default with browser ['Ubuntu', 'Chrome', '22.0.4'] and full history sync on.
const login = { user: '48000000000', device: 3 }
const loginConfig = { ...DEFAULT_CONNECTION_CONFIG, browser: ['Ubuntu', 'Chrome', '22.0.4'], syncFullHistory: true }
const clientPayload = Buffer.from(
	proto.ClientPayload.encode(generateLoginNode(`${login.user}:${login.device}@s.whatsapp.net`, loginConfig)).finish()
)
const keyEnc = noise.processHandshake(proto.HandshakeMessage.decode(serverHello), noiseKey)
const payloadEnc = noise.encrypt(clientPayload)
const finish = Buffer.from(proto.HandshakeMessage.encode({ clientFinish: { static: keyEnc, payload: payloadEnc } }).finish())
const finishFrame = noise.encodeFrame(finish)
await noise.finishInit()

// The server reads the finish, which proves both halves agree, then switches to transport keys.
const finishMsg = proto.HandshakeMessage.decode(finish).clientFinish
const clientNoisePublic = server.decrypt(finishMsg.static)
if (hex(clientNoisePublic) !== hex(noiseKey.public)) throw new Error('server read the wrong noise key')
server.mixKey(Curve.sharedKey(serverEphemeral.private, clientNoisePublic))
if (hex(server.decrypt(finishMsg.payload)) !== hex(clientPayload)) throw new Error('server read the wrong payload')
const { clientRead } = server.split()

const nodes = [
	{ tag: 'iq', attrs: { id: '1', to: 's.whatsapp.net', type: 'get', xmlns: 'w:p' }, content: [{ tag: 'ping', attrs: {} }] },
	{ tag: 'presence', attrs: { type: 'available' } }
]
const clientFrames = nodes.map(n => {
	const plaintext = encodeBinaryNode(n)
	return { plaintext: hex(plaintext), frame: hex(noise.encodeFrame(plaintext)) }
})
// A server frame, and Baileys reading it, so decryption is pinned to a decoder too.
const serverNode = { tag: 'success', attrs: { t: String(now), location: 'frc' } }
const serverPlain = encodeBinaryNode(serverNode)
const iv0 = Buffer.alloc(12)
const serverFrameBody = aesEncryptGCM(serverPlain, clientRead, iv0, Buffer.alloc(0))
const serverFrame = Buffer.concat([Buffer.from([0, 0, serverFrameBody.length]), serverFrameBody])
let received
await noise.decodeFrame(serverFrame, frame => { received = frame })
if (received?.tag !== 'success') throw new Error('Baileys did not read the server frame')

await fs.writeFile(
	'/out/noise.json',
	JSON.stringify(
		{
			clientEphemeral: pair(clientEphemeral),
			noiseKey: pair(noiseKey),
			root: hex(root.public),
			now,
			routingInfo: hex(routingInfo),
			clientPayload: hex(clientPayload),
			login: { ...login, version: loginConfig.version, country: loginConfig.countryCode },
			helloFrame: hex(helloFrame),
			serverHello: hex(serverHello),
			finishFrame: hex(finishFrame),
			clientFrames,
			serverFrame: hex(serverFrame),
			serverPlaintext: hex(serverPlain),
			badServerHellos: Object.fromEntries(Object.entries(badChains).map(([reason, chain]) => [reason, serverHelloWith(chain)]))
		},
		null,
		1
	)
)
console.log('handshake complete on both sides')
