/**
 * Every number the room is bounded by, in one file.
 *
 * They are here together rather than inline at each use because ADR-009's cost
 * model is an argument made *from* these numbers, and an argument you cannot
 * grep against the code is one that quietly stops being true. The arithmetic it
 * rests on:
 *
 * - Outgoing messages and protocol pings are free; incoming bill at 20:1.
 * - One active session at the daemon's ~30ms coalescing floor is ~8 incoming
 *   messages/second — ~1,440 billed requests/hour, ~$0.0002/hour.
 * - Duration dominates and is not billed while hibernating, which is why "never
 *   write storage on the data path" is a policy and not a preference.
 *
 * Three of the limits below were **measured against workerd**, not read off a
 * doc page: `MAX_TAGS`, `MAX_TAG_CHARS` and `MAX_ATTACHMENT_BYTES`, each driven
 * past its boundary under miniflare until it threw. `MAX_MESSAGE_BYTES` is
 * **not** one of them — it is carried from ADR-009's prose, and a 32 MiB
 * message is expensive enough to send that nobody has driven a Durable Object
 * past it here.
 *
 * The distinction is laboured on purpose. This repo keeps a "measured, so it is
 * not folklore" discipline, and a comment that quietly widens what was actually
 * checked is how a number stops being either. The message ceiling has already
 * moved once (it was 1 MiB before 2025-10-31), so all of these are dated by the
 * compatibility date in `wrangler.jsonc` rather than eternal.
 */

// --- what the platform enforces --------------------------------------------

/** `acceptWebSocket(ws, tags)` throws above this. Distinct tags, not total. */
export const MAX_TAGS_PER_SOCKET = 10;

/** Per tag, in characters. Above it, `acceptWebSocket` names the offending tag. */
export const MAX_TAG_LENGTH = 256;

/**
 * `serializeAttachment` throws above this.
 *
 * The bound is on the **serialized** size, not on the value's own length: a
 * 16,384-character string serializes to 16,390 bytes and is refused. Anything
 * sizing an attachment has to serialize it first, which is why the fake
 * platform in `test/` measures with `node:v8`'s serializer — it agrees with
 * workerd byte for byte at this boundary, verified on both a string and an
 * object.
 */
export const MAX_ATTACHMENT_BYTES = 16_384;

/**
 * The largest WebSocket message the edge will carry.
 *
 * It was 1 MiB until 2025-10-31, and an earlier draft of ADR-009 called for
 * splitting relay writes into 256 KiB chunks because of it. That is now not
 * just unnecessary but actively wrong: billing is per message, so splitting an
 * 8 MiB scrollback response into 32 messages multiplies its cost by 32 on
 * exactly the responses the split was meant to protect.
 */
export const MAX_MESSAGE_BYTES = 32 * 1024 * 1024;

// --- what the protocol can put through it ----------------------------------

/**
 * `zest_proto::MAX_FRAME`, which bounds the **plaintext** of a frame.
 *
 * Kept here so the headroom against `MAX_MESSAGE_BYTES` is a comparison
 * anybody can make in one file rather than a fact about two repositories.
 */
export const MAX_FRAME_BYTES = 8 * 1024 * 1024;

/** ChaCha20-Poly1305's tag. A sealed frame is this much longer than its plaintext. */
export const SEAL_TAG_BYTES = 16;
