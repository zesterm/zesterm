/**
 * MessagePack, restricted to what the daemon actually emits.
 *
 * Hand-rolled and dependency-free, for the reason this workspace is generally:
 * `rmp_serde::to_vec_named` produces a closed subset — nil, bool, integers,
 * strings, arrays and maps — and nothing here has to guess, because 44 recorded
 * frames prove it on the first run. That is not usually true of hand-rolled
 * parsers and it is true of this one.
 *
 * Anything outside the subset throws naming the byte and its offset rather than
 * returning something plausible. If a wire change ever brings in `ext`,
 * timestamps or `bin` (which `ClientMessage::Input` would, if its `Vec<u8>` ever
 * gained `serde_bytes`), the honest move is to swap this module for
 * `@msgpack/msgpack` — nothing outside this file would know.
 */

export class MsgpackError extends Error {
  override name = 'MsgpackError';
}

const TEXT = new TextDecoder('utf-8', { fatal: true });

/**
 * Decode exactly one value, and insist it was the whole input.
 *
 * Trailing bytes mean the frame was longer than the value inside it, which is a
 * framing bug — and one that would otherwise be swallowed silently here and
 * surface much later as a missing update.
 */
export function decode(bytes: Uint8Array): unknown {
  const r = new Reader(bytes);
  const value = r.value();
  if (!r.done()) {
    throw new MsgpackError(
      `${r.remaining()} trailing bytes after a complete value -- the frame and the message disagree about length`,
    );
  }
  return value;
}

class Reader {
  #bytes: Uint8Array;
  #view: DataView;
  #pos = 0;

  constructor(bytes: Uint8Array) {
    this.#bytes = bytes;
    this.#view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  }

  done(): boolean {
    return this.#pos >= this.#bytes.length;
  }

  remaining(): number {
    return this.#bytes.length - this.#pos;
  }

  value(): unknown {
    const at = this.#pos;
    const b = this.#u8();

    // Positive fixint, and negative fixint at the top of the range.
    if (b <= 0x7f) return b;
    if (b >= 0xe0) return b - 0x100;

    if (b >= 0x80 && b <= 0x8f) return this.#map(b & 0x0f);
    if (b >= 0x90 && b <= 0x9f) return this.#array(b & 0x0f);
    if (b >= 0xa0 && b <= 0xbf) return this.#str(b & 0x1f);

    switch (b) {
      case 0xc0:
        return null;
      case 0xc2:
        return false;
      case 0xc3:
        return true;

      case 0xc4:
        return this.#bin(this.#u8());
      case 0xc5:
        return this.#bin(this.#u16());
      case 0xc6:
        return this.#bin(this.#u32());

      case 0xcc:
        return this.#u8();
      case 0xcd:
        return this.#u16();
      case 0xce:
        return this.#u32();
      case 0xcf:
        return this.#u64(false);

      case 0xd0:
        return this.#i8();
      case 0xd1:
        return this.#i16();
      case 0xd2:
        return this.#i32();
      case 0xd3:
        return this.#u64(true);

      case 0xd9:
        return this.#str(this.#u8());
      case 0xda:
        return this.#str(this.#u16());
      case 0xdb:
        return this.#str(this.#u32());

      case 0xdc:
        return this.#array(this.#u16());
      case 0xdd:
        return this.#array(this.#u32());

      case 0xde:
        return this.#map(this.#u16());
      case 0xdf:
        return this.#map(this.#u32());

      default:
        throw new MsgpackError(
          `unsupported type byte 0x${b.toString(16).padStart(2, '0')} at offset ${at}` +
            ` -- this decoder covers only what the daemon emits`,
        );
    }
  }

  #need(n: number): void {
    if (this.#pos + n > this.#bytes.length) {
      throw new MsgpackError(
        `truncated: need ${n} bytes at offset ${this.#pos}, ${this.remaining()} left`,
      );
    }
  }

  #u8(): number {
    this.#need(1);
    return this.#view.getUint8(this.#pos++);
  }

  #u16(): number {
    this.#need(2);
    const v = this.#view.getUint16(this.#pos);
    this.#pos += 2;
    return v;
  }

  #u32(): number {
    this.#need(4);
    const v = this.#view.getUint32(this.#pos);
    this.#pos += 4;
    return v;
  }

  #i8(): number {
    this.#need(1);
    return this.#view.getInt8(this.#pos++);
  }

  #i16(): number {
    this.#need(2);
    const v = this.#view.getInt16(this.#pos);
    this.#pos += 2;
    return v;
  }

  #i32(): number {
    this.#need(4);
    const v = this.#view.getInt32(this.#pos);
    this.#pos += 4;
    return v;
  }

  /**
   * A 64-bit integer, as a `number` when that is lossless and a `bigint` when it
   * is not.
   *
   * Narrowing on value rather than on width is deliberate. `rmp_serde` writes
   * the *narrowest* encoding that fits, so a `Seq` of 3 arrives as a fixint and
   * is already a `number` above; a decoder that returned `bigint` only for the
   * 0xcf/0xd3 tags would still hand out both types for the same field depending
   * on how large it happened to be. Callers normalize in `wire.ts` either way.
   */
  #u64(signed: boolean): number | bigint {
    this.#need(8);
    const v = signed ? this.#view.getBigInt64(this.#pos) : this.#view.getBigUint64(this.#pos);
    this.#pos += 8;
    return v >= BigInt(Number.MIN_SAFE_INTEGER) && v <= BigInt(Number.MAX_SAFE_INTEGER)
      ? Number(v)
      : v;
  }

  #str(len: number): string {
    this.#need(len);
    const s = TEXT.decode(this.#bytes.subarray(this.#pos, this.#pos + len));
    this.#pos += len;
    return s;
  }

  #bin(len: number): Uint8Array {
    this.#need(len);
    const b = this.#bytes.slice(this.#pos, this.#pos + len);
    this.#pos += len;
    return b;
  }

  #array(len: number): unknown[] {
    const out: unknown[] = new Array<unknown>(len);
    for (let i = 0; i < len; i++) out[i] = this.value();
    return out;
  }

  #map(len: number): Record<string, unknown> {
    const out: Record<string, unknown> = {};
    for (let i = 0; i < len; i++) {
      const at = this.#pos;
      const key = this.value();
      if (typeof key !== 'string') {
        throw new MsgpackError(
          `map key at offset ${at} is ${typeof key}, not a string` +
            ` -- the wire is written with to_vec_named, so every key is a field name`,
        );
      }
      out[key] = this.value();
    }
    return out;
  }
}
