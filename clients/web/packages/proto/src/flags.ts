/**
 * The cell-attribute and terminal-mode bits.
 *
 * `AttrDef.flags` and `DeltaOp::Modes.bits` are bare numbers in the generated
 * bindings — `ts-rs` has no bitflags type — so the names live only in
 * `zest-core/src/cell.rs` and `modes.rs`, and a client has to hard-code them.
 *
 * **The recordings cannot catch a mistake here.** They compare `flags` as raw
 * integers, so a table that maps `ITALIC` to the wrong bit passes every fixture
 * in the corpus. `crates/zest-proto/fixtures/bits.json` is exported from the
 * Rust `bitflags` definitions for exactly this reason, and `flags.test.ts`
 * checks these tables against it in both directions.
 */

/** `zest_core::CellFlags`. */
export const CellFlags = {
  BOLD: 1 << 0,
  DIM: 1 << 1,
  ITALIC: 1 << 2,
  UNDERLINE: 1 << 3,
  DOUBLE_UNDERLINE: 1 << 4,
  UNDERCURL: 1 << 5,
  STRIKEOUT: 1 << 6,
  INVERSE: 1 << 7,
  HIDDEN: 1 << 8,
  BLINK: 1 << 9,
  /** A double-width character (CJK, most emoji). */
  WIDE: 1 << 10,
  /** Its right half. Holds no glyph; the renderer skips it. */
  WIDE_SPACER: 1 << 11,
  /** Last cell of a row that wrapped rather than ended. */
  WRAPLINE: 1 << 12,
} as const;

/** `zest_core::Modes`. */
export const Modes = {
  AUTO_WRAP: 1 << 0,
  ORIGIN: 1 << 1,
  APP_CURSOR: 1 << 2,
  APP_KEYPAD: 1 << 3,
  SHOW_CURSOR: 1 << 4,
  INSERT: 1 << 5,
  LINE_FEED_NEW_LINE: 1 << 6,
  ALT_SCREEN: 1 << 7,
  BRACKETED_PASTE: 1 << 8,
  MOUSE_CLICK: 1 << 9,
  MOUSE_DRAG: 1 << 10,
  MOUSE_MOTION: 1 << 11,
  MOUSE_SGR: 1 << 12,
  FOCUS_REPORTING: 1 << 13,
  SYNC_UPDATE: 1 << 14,
  WIN32_INPUT: 1 << 15,
  /**
   * The Kitty keyboard flags, at `1 << (16 + k)` for the protocol's `1 << k`.
   *
   * A client that drops these keeps encoding keystrokes the legacy way at a
   * program that has stopped expecting it, which is a silent and total input
   * failure rather than a degraded one. The gaps at 18 and 20 are kitty flags
   * 4 and 16, which the terminal does not implement yet.
   */
  KITTY_DISAMBIGUATE: 1 << 16,
  KITTY_EVENT_TYPES: 1 << 17,
  KITTY_REPORT_ALL: 1 << 19,
} as const;

/** Every bit this client knows how to interpret. */
export const KNOWN_CELL_FLAGS = Object.values(CellFlags).reduce((a, b) => a | b, 0);
export const KNOWN_MODES = Object.values(Modes).reduce((a, b) => a | b, 0);

export function hasFlag(bits: number, flag: number): boolean {
  return (bits & flag) !== 0;
}

/**
 * Drop bits this client does not know, keeping the ones it does.
 *
 * `from_bits_truncate` in Rust, and the same reasoning: "a newer host that has
 * learned an attribute should render plainer here, not fail to render at all".
 * The raw value is still what gets compared — this is for interpretation only.
 */
export function knownCellFlags(bits: number): number {
  return bits & KNOWN_CELL_FLAGS;
}

export function knownModes(bits: number): number {
  return bits & KNOWN_MODES;
}

/**
 * The Kitty keyboard flags in force, as the protocol numbers them.
 *
 * Mirrors `Modes::kitty_flags` in `zest-core/src/modes.rs`: the flags sit at
 * `1 << (16 + k)` for the protocol's `1 << k`, so this is a shift and a mask.
 * The result is 0 for every program that never asked, which is the case the
 * encoders check first.
 */
export const KITTY_SUPPORTED = 1 | 2 | 8;
export function kittyFlags(modes: number): number {
  return (modes >>> 16) & KITTY_SUPPORTED;
}

/** For failure messages. */
export function cellFlagNames(bits: number): string {
  const names = Object.entries(CellFlags)
    .filter(([, bit]) => hasFlag(bits, bit))
    .map(([name]) => name);
  const unknown = bits & ~KNOWN_CELL_FLAGS;
  if (unknown !== 0) names.push(`unknown(0x${unknown.toString(16)})`);
  return names.length > 0 ? names.join('|') : 'none';
}
