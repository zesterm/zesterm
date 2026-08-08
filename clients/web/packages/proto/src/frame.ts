/**
 * Length-prefixed framing, mirroring `crates/zest-proto/src/frame.rs`.
 *
 * A `u32` little-endian length, then that many bytes of MessagePack. Redundant
 * over a WebSocket, which already delivers whole messages — but the daemon emits
 * the prefix on every transport, so a client reading a raw stream still has to
 * strip four bytes per message.
 */

/** Matches `frame.rs`'s `MAX_FRAME`. */
export const MAX_FRAME = 8 * 1024 * 1024;

export class FrameError extends Error {
  override name = 'FrameError';
}

/**
 * Reassembles frames from a byte stream that arrives in arbitrary pieces.
 *
 * `feed` then `next` until it returns `undefined`, which is "not yet", never
 * "end". A stream is a stream: a frame's header can be split across two reads,
 * and a decoder that assumes otherwise works on loopback and fails on a LAN.
 */
export class FrameReader {
  #buf = new Uint8Array(0);

  feed(chunk: Uint8Array): void {
    if (this.#buf.length === 0) {
      this.#buf = chunk.slice();
      return;
    }
    const merged = new Uint8Array(this.#buf.length + chunk.length);
    merged.set(this.#buf, 0);
    merged.set(chunk, this.#buf.length);
    this.#buf = merged;
  }

  /** The next complete frame's payload, or `undefined` if one has not arrived. */
  next(): Uint8Array | undefined {
    if (this.#buf.length < 4) return undefined;

    const len = new DataView(this.#buf.buffer, this.#buf.byteOffset, 4).getUint32(0, true);

    // Checked before allocating, not after: a corrupt or hostile length is the
    // one input that can turn a framing bug into an out-of-memory.
    if (len > MAX_FRAME) {
      throw new FrameError(`frame of ${len} bytes exceeds the ${MAX_FRAME} byte limit`);
    }

    if (this.#buf.length < 4 + len) return undefined;

    const body = this.#buf.slice(4, 4 + len);
    this.#buf = this.#buf.slice(4 + len);
    return body;
  }

  /** Bytes held that do not yet make a frame. For diagnostics only. */
  get pending(): number {
    return this.#buf.length;
  }
}
