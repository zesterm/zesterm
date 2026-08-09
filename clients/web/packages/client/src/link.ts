/**
 * The transport seam.
 *
 * A `Dial` opens one connection and reports bytes and lifecycle through the
 * handlers it was given. Everything above this line is platform-blind: the
 * browser adapts a `WebSocket`, the sidecar adapts a `net.Socket`, the phone
 * adapts Lynx's socket — and this package tests against a scripted fake.
 *
 * Bytes, not messages: the daemon emits its u32 length prefix on every
 * transport, so the layer above always runs a `FrameReader` and a transport
 * that happens to preserve message boundaries (WebSocket) simply hands it
 * pre-chunked input.
 */

export interface ByteLinkHandlers {
  /** The transport is open; the handshake may begin. */
  onOpen(): void;
  onMessage(bytes: Uint8Array): void;
  /** The transport is gone — dropped, refused, or closed. Terminal per dial. */
  onClose(): void;
}

export interface ByteLink {
  send(bytes: Uint8Array): void;
  close(): void;
}

export type Dial = (handlers: ByteLinkHandlers) => ByteLink;
