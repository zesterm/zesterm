/**
 * MessagePack encoding, restricted to what `ClientMessage` needs — the
 * mirror-image of `msgpack.ts`, with the same policy: hand-rolled,
 * dependency-free, and honest about its subset.
 *
 * # Byte-for-byte parity with `rmp_serde::to_vec_named`
 *
 * This encoder does not merely round-trip; it aims to produce the *identical
 * bytes* the Rust encoder would, so a golden fixture can assert equality
 * rather than mere acceptance. Three rules carry that:
 *
 * - **Integers take the narrowest encoding that fits**, exactly as `rmp_serde`
 *   writes them: positive fixint, u8, u16, u32, u64 — and for negatives,
 *   negative fixint, i8, i16, i32, i64. A signed value that is non-negative
 *   uses the unsigned family, which is what `rmp`'s `write_sint` does.
 * - **Maps are written in insertion order.** `to_vec_named` emits struct
 *   fields in declaration order; the callers in `wire-client.ts` construct
 *   objects in that same order, and JavaScript preserves it.
 * - **`Uint8Array` becomes an array of integers**, not `bin`. Serde's default
 *   for `Vec<u8>` is a sequence (no `serde_bytes` on the wire types), and the
 *   decoder's subset note says the same thing from the other side.
 *
 * Floats are refused rather than encoded: nothing on this wire is a float, so
 * one appearing means a caller bug, and writing `f64` bytes would smuggle it
 * to the host as a decode error there instead of a stack trace here.
 */

import { MsgpackError } from './msgpack.ts';

const TEXT_ENCODER = new TextEncoder();

/** Encode one value. The inverse of `msgpack.ts`'s `decode`. */
export function encode(value: unknown): Uint8Array {
  const w = new Writer();
  w.value(value);
  return w.take();
}

class Writer {
  #buf = new Uint8Array(256);
  #len = 0;

  take(): Uint8Array {
    return this.#buf.slice(0, this.#len);
  }

  value(v: unknown): void {
    if (v === null || v === undefined) {
      // Absent optional fields are handled by callers omitting the key
      // (`skip_serializing_if` on the Rust side); an explicit null is `nil`.
      this.#u8(0xc0);
    } else if (typeof v === 'boolean') {
      this.#u8(v ? 0xc3 : 0xc2);
    } else if (typeof v === 'number' || typeof v === 'bigint') {
      this.#int(v);
    } else if (typeof v === 'string') {
      this.#str(v);
    } else if (v instanceof Uint8Array) {
      // Serde's `Vec<u8>` is a sequence of integers on this wire — see the
      // module docs. `bin` would be shorter and would also be wrong.
      this.#arrayHeader(v.length);
      for (const b of v) this.#int(b);
    } else if (Array.isArray(v)) {
      this.#arrayHeader(v.length);
      for (const item of v) this.value(item);
    } else if (typeof v === 'object') {
      const entries = Object.entries(v as Record<string, unknown>);
      this.#mapHeader(entries.length);
      for (const [key, item] of entries) {
        this.#str(key);
        this.value(item);
      }
    } else {
      throw new MsgpackError(`cannot encode a ${typeof v} -- nothing on this wire is one`);
    }
  }

  #int(v: number | bigint): void {
    if (typeof v === 'number' && !Number.isInteger(v)) {
      throw new MsgpackError(`cannot encode ${v} -- nothing on this wire is a float`);
    }
    const n = typeof v === 'bigint' ? v : BigInt(v);
    if (n >= 0n) {
      if (n <= 0x7fn) {
        this.#u8(Number(n));
      } else if (n <= 0xffn) {
        this.#u8(0xcc);
        this.#u8(Number(n));
      } else if (n <= 0xffffn) {
        this.#u8(0xcd);
        this.#u16(Number(n));
      } else if (n <= 0xffff_ffffn) {
        this.#u8(0xce);
        this.#u32(Number(n));
      } else if (n <= 0xffff_ffff_ffff_ffffn) {
        this.#u8(0xcf);
        this.#u64(n);
      } else {
        throw new MsgpackError(`${n} does not fit in a u64`);
      }
    } else if (n >= -0x20n) {
      // Negative fixint.
      this.#u8(Number(n) & 0xff);
    } else if (n >= -0x80n) {
      this.#u8(0xd0);
      this.#u8(Number(n) & 0xff);
    } else if (n >= -0x8000n) {
      this.#u8(0xd1);
      this.#u16(Number(n) & 0xffff);
    } else if (n >= -0x8000_0000n) {
      this.#u8(0xd2);
      this.#u32(Number(n) >>> 0);
    } else if (n >= -0x8000_0000_0000_0000n) {
      this.#u8(0xd3);
      this.#u64(BigInt.asUintN(64, n));
    } else {
      throw new MsgpackError(`${n} does not fit in an i64`);
    }
  }

  #str(s: string): void {
    const bytes = TEXT_ENCODER.encode(s);
    if (bytes.length <= 31) {
      this.#u8(0xa0 | bytes.length);
    } else if (bytes.length <= 0xff) {
      this.#u8(0xd9);
      this.#u8(bytes.length);
    } else if (bytes.length <= 0xffff) {
      this.#u8(0xda);
      this.#u16(bytes.length);
    } else {
      this.#u8(0xdb);
      this.#u32(bytes.length);
    }
    this.#raw(bytes);
  }

  #arrayHeader(len: number): void {
    if (len <= 15) {
      this.#u8(0x90 | len);
    } else if (len <= 0xffff) {
      this.#u8(0xdc);
      this.#u16(len);
    } else {
      this.#u8(0xdd);
      this.#u32(len);
    }
  }

  #mapHeader(len: number): void {
    if (len <= 15) {
      this.#u8(0x80 | len);
    } else if (len <= 0xffff) {
      this.#u8(0xde);
      this.#u16(len);
    } else {
      this.#u8(0xdf);
      this.#u32(len);
    }
  }

  #grow(need: number): void {
    if (this.#len + need <= this.#buf.length) return;
    const next = new Uint8Array(Math.max(this.#buf.length * 2, this.#len + need));
    next.set(this.#buf.subarray(0, this.#len));
    this.#buf = next;
  }

  #u8(b: number): void {
    this.#grow(1);
    this.#buf[this.#len++] = b;
  }

  #u16(v: number): void {
    this.#grow(2);
    this.#buf[this.#len++] = (v >> 8) & 0xff;
    this.#buf[this.#len++] = v & 0xff;
  }

  #u32(v: number): void {
    this.#grow(4);
    this.#buf[this.#len++] = (v >>> 24) & 0xff;
    this.#buf[this.#len++] = (v >>> 16) & 0xff;
    this.#buf[this.#len++] = (v >>> 8) & 0xff;
    this.#buf[this.#len++] = v & 0xff;
  }

  #u64(v: bigint): void {
    this.#grow(8);
    for (let shift = 56n; shift >= 0n; shift -= 8n) {
      this.#buf[this.#len++] = Number((v >> shift) & 0xffn);
    }
  }

  #raw(bytes: Uint8Array): void {
    this.#grow(bytes.length);
    this.#buf.set(bytes, this.#len);
    this.#len += bytes.length;
  }
}
