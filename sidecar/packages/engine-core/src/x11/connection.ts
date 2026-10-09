import net from 'node:net';

/**
 * A minimal client of the X11 core protocol, written for one purpose: the
 * behavior engine sends its mouse and keyboard input to a persona's own X
 * display as a device would (the XTEST extension), so a browser receives it
 * through the same path as input from hardware (spec §10.4).
 *
 * It speaks only what that needs: the connection setup, a handful of core
 * requests and extension requests by opcode. It talks to the local Unix
 * socket of an Xvfb the sidecar started itself, which accepts local clients
 * without a cookie. Byte order is little-endian throughout.
 */

/** The X server refused a request, or the connection broke. */
export class X11Error extends Error {
  override readonly name = 'X11Error';

  constructor(
    message: string,
    /** The protocol's error code, when the server sent one. */
    readonly code: number | null = null,
  ) {
    super(message);
  }
}

/** What the server says about itself when a client connects. */
export interface X11Setup {
  /** The root window of the first screen. */
  readonly root: number;
  /** Size of the first screen in pixels. */
  readonly width: number;
  readonly height: number;
  /** Range of key codes the server uses. */
  readonly minKeycode: number;
  readonly maxKeycode: number;
  readonly resourceIdBase: number;
  readonly resourceIdMask: number;
}

/** Core request opcodes this client uses. */
export const OPCODE = {
  createWindow: 1,
  internAtom: 16,
  changeProperty: 18,
  queryPointer: 38,
  getInputFocus: 43,
  queryExtension: 98,
  changeKeyboardMapping: 100,
  getKeyboardMapping: 101,
} as const;

interface Pending {
  readonly sequence: number;
  readonly resolve: (reply: Buffer) => void;
  readonly reject: (error: Error) => void;
}

const pad4 = (length: number) => (4 - (length % 4)) % 4;

/** The Unix socket of a display name such as `:100` or `:100.0`. */
export function socketPathOf(display: string): string {
  const match = /^:(\d+)(?:\.\d+)?$/.exec(display);
  if (match?.[1] === undefined) {
    throw new X11Error(`not a local display name: ${JSON.stringify(display)}`);
  }
  return `/tmp/.X11-unix/X${match[1]}`;
}

/**
 * Reads the server's answer to the connection setup.
 *
 * @returns the setup, or `null` while the answer is incomplete.
 * @throws {X11Error} when the server refused the connection.
 */
export function parseSetup(data: Buffer): { setup: X11Setup; consumed: number } | null {
  if (data.length < 8) {
    return null;
  }
  const total = 8 + data.readUInt16LE(6) * 4;
  if (data.length < total) {
    return null;
  }
  if (data[0] !== 1) {
    const reason = data.toString('latin1', 8, 8 + (data[1] ?? 0));
    throw new X11Error(`the X server refused the connection: ${reason || 'no reason given'}`);
  }
  const vendorLength = data.readUInt16LE(24);
  const formats = data[29] ?? 0;
  // The first screen follows the fixed part, the vendor string and the
  // pixmap formats (8 bytes each).
  const screen = 40 + vendorLength + pad4(vendorLength) + formats * 8;
  if (total < screen + 24) {
    throw new X11Error('the X server described no screen');
  }
  return {
    consumed: total,
    setup: {
      resourceIdBase: data.readUInt32LE(12),
      resourceIdMask: data.readUInt32LE(16),
      minKeycode: data[34] ?? 8,
      maxKeycode: data[35] ?? 255,
      root: data.readUInt32LE(screen),
      width: data.readUInt16LE(screen + 20),
      height: data.readUInt16LE(screen + 22),
    },
  };
}

/** An open connection to an X server. */
export class X11Connection {
  private buffer: Buffer = Buffer.alloc(0);
  private sequence = 0;
  private resources = 0;
  private closed = false;
  private readonly pending: Pending[] = [];
  /** Errors of requests that have no reply; `sync` reports the first one. */
  private readonly unclaimed: X11Error[] = [];

  private constructor(
    private readonly socket: net.Socket,
    readonly setup: X11Setup,
    rest: Buffer,
  ) {
    socket.on('data', (chunk: Buffer) => {
      this.receive(chunk);
    });
    socket.on('close', () => {
      this.fail(new X11Error('the X server closed the connection'));
    });
    socket.on('error', (error) => {
      this.fail(new X11Error(`the connection to the X server failed: ${error.message}`));
    });
    this.receive(rest);
  }

  /**
   * Connects to a local display.
   *
   * @throws {X11Error} when the display does not exist, refuses, or does not answer in time.
   */
  static connect(display: string, timeoutMs = 5000): Promise<X11Connection> {
    const path = socketPathOf(display);
    return new Promise((resolve, reject) => {
      const socket = net.createConnection({ path });
      let received: Buffer = Buffer.alloc(0);
      const timer = setTimeout(() => {
        socket.destroy();
        reject(new X11Error(`the X server at ${display} did not answer within ${timeoutMs} ms`));
      }, timeoutMs);
      const onError = (error: Error) => {
        clearTimeout(timer);
        reject(new X11Error(`cannot connect to the X server at ${display}: ${error.message}`));
      };
      const onData = (chunk: Buffer) => {
        received = Buffer.concat([received, chunk]);
        try {
          const parsed = parseSetup(received);
          if (parsed === null) {
            return;
          }
          clearTimeout(timer);
          socket.off('data', onData);
          socket.off('error', onError);
          resolve(new X11Connection(socket, parsed.setup, received.subarray(parsed.consumed)));
        } catch (error) {
          clearTimeout(timer);
          socket.destroy();
          reject(error instanceof Error ? error : new X11Error(String(error)));
        }
      };
      socket.once('error', onError);
      socket.on('data', onData);
      socket.once('connect', () => {
        // Little-endian client, protocol 11.0, no authorization.
        const hello = Buffer.alloc(12);
        hello[0] = 0x6c;
        hello.writeUInt16LE(11, 2);
        socket.write(hello);
      });
    });
  }

  private fail(error: X11Error): void {
    if (this.closed) {
      return;
    }
    this.closed = true;
    for (const waiting of this.pending.splice(0)) {
      waiting.reject(error);
    }
  }

  private receive(chunk: Buffer): void {
    this.buffer = this.buffer.length === 0 ? chunk : Buffer.concat([this.buffer, chunk]);
    while (this.buffer.length >= 32) {
      const kind = this.buffer[0] ?? 0;
      // Replies and generic events carry more than the fixed 32 bytes.
      const extra = kind === 1 || (kind & 0x7f) === 35 ? this.buffer.readUInt32LE(4) * 4 : 0;
      if (this.buffer.length < 32 + extra) {
        return;
      }
      const packet = this.buffer.subarray(0, 32 + extra);
      this.buffer = this.buffer.subarray(32 + extra);
      if (kind === 1) {
        this.settle(packet.readUInt16LE(2), (waiting) => {
          waiting.resolve(packet);
        });
      } else if (kind === 0) {
        const error = new X11Error(
          `the X server answered error ${packet[1] ?? 0} to request ${packet[10] ?? 0}.${packet.readUInt16LE(8)}`,
          packet[1] ?? 0,
        );
        if (
          !this.settle(packet.readUInt16LE(2), (waiting) => {
            waiting.reject(error);
          })
        ) {
          this.unclaimed.push(error);
        }
      }
      // Everything else is an event; this client asks for none and reads none.
    }
  }

  private settle(sequence: number, apply: (waiting: Pending) => void): boolean {
    const index = this.pending.findIndex((waiting) => (waiting.sequence & 0xffff) === sequence);
    const waiting = index < 0 ? undefined : this.pending.splice(index, 1)[0];
    if (waiting === undefined) {
      return false;
    }
    apply(waiting);
    return true;
  }

  private write(opcode: number, data: number, body: Buffer): number {
    if (this.closed) {
      throw new X11Error('the connection to the X server is closed');
    }
    const padded = body.length + pad4(body.length);
    const packet = Buffer.alloc(4 + padded);
    packet[0] = opcode;
    packet[1] = data;
    packet.writeUInt16LE(packet.length / 4, 2);
    body.copy(packet, 4);
    this.sequence += 1;
    this.socket.write(packet);
    return this.sequence;
  }

  /** Sends a request that has no reply. A refusal shows at the next {@link sync}. */
  send(opcode: number, data: number, body: Buffer = Buffer.alloc(0)): void {
    this.write(opcode, data, body);
  }

  /**
   * Sends a request and waits for its reply.
   *
   * @throws {X11Error} when the server answers with an error or the connection breaks.
   */
  request(opcode: number, data: number, body: Buffer = Buffer.alloc(0)): Promise<Buffer> {
    return new Promise((resolve, reject) => {
      try {
        this.pending.push({ sequence: this.write(opcode, data, body), resolve, reject });
      } catch (error) {
        reject(error instanceof Error ? error : new X11Error(String(error)));
      }
    });
  }

  /**
   * Waits until the server has processed everything sent so far.
   *
   * @throws {X11Error} the first refusal of a request without a reply since the last call.
   */
  async sync(): Promise<void> {
    await this.request(OPCODE.getInputFocus, 0);
    const error = this.unclaimed.shift();
    this.unclaimed.length = 0;
    if (error !== undefined) {
      throw error;
    }
  }

  /** A new resource id for a window this client creates. */
  newResourceId(): number {
    this.resources += 1;
    return (this.setup.resourceIdBase | (this.resources & this.setup.resourceIdMask)) >>> 0;
  }

  /** Looks an extension up; `null` when the server does not have it. */
  async queryExtension(name: string): Promise<{ majorOpcode: number } | null> {
    const text = Buffer.from(name, 'latin1');
    const body = Buffer.alloc(4 + text.length);
    body.writeUInt16LE(text.length, 0);
    text.copy(body, 4);
    const reply = await this.request(OPCODE.queryExtension, 0, body);
    return reply[8] === 1 ? { majorOpcode: reply[9] ?? 0 } : null;
  }

  /** The atom of a name, created when it does not exist. */
  async internAtom(name: string): Promise<number> {
    const text = Buffer.from(name, 'latin1');
    const body = Buffer.alloc(4 + text.length);
    body.writeUInt16LE(text.length, 0);
    text.copy(body, 4);
    return (await this.request(OPCODE.internAtom, 0, body)).readUInt32LE(8);
  }

  /** Replaces a property of a window. `format` is the unit size in bits: 8 or 32. */
  changeProperty(
    window: number,
    property: number,
    type: number,
    format: 8 | 32,
    data: Buffer,
  ): void {
    const body = Buffer.alloc(20 + data.length);
    body.writeUInt32LE(window, 0);
    body.writeUInt32LE(property, 4);
    body.writeUInt32LE(type, 8);
    body[12] = format;
    body.writeUInt32LE(data.length / (format / 8), 16);
    data.copy(body, 20);
    this.send(OPCODE.changeProperty, 0, body);
  }

  /** Creates an input-only, never mapped 1x1 window below the root: something to hang properties on. */
  createHiddenWindow(): number {
    const id = this.newResourceId();
    const body = Buffer.alloc(28);
    body.writeUInt32LE(id, 0);
    body.writeUInt32LE(this.setup.root, 4);
    body.writeInt16LE(-10, 8);
    body.writeInt16LE(-10, 10);
    body.writeUInt16LE(1, 12);
    body.writeUInt16LE(1, 14);
    // Border width 0, class InputOnly (2), visual and depth copied from the parent, no attributes.
    body.writeUInt16LE(2, 18);
    this.send(OPCODE.createWindow, 0, body);
    return id;
  }

  /** Where the pointer is, in pixels of the root window. */
  async queryPointer(): Promise<{ x: number; y: number }> {
    const body = Buffer.alloc(4);
    body.writeUInt32LE(this.setup.root, 0);
    const reply = await this.request(OPCODE.queryPointer, 0, body);
    return { x: reply.readInt16LE(16), y: reply.readInt16LE(18) };
  }

  /** The key symbols of every key code: one row per code, from `setup.minKeycode` on. */
  async keyboardMapping(): Promise<number[][]> {
    const count = this.setup.maxKeycode - this.setup.minKeycode + 1;
    const body = Buffer.alloc(4);
    body[0] = this.setup.minKeycode;
    body[1] = count;
    const reply = await this.request(OPCODE.getKeyboardMapping, 0, body);
    const perCode = reply[1] ?? 0;
    const rows: number[][] = [];
    for (let code = 0; code < count; code += 1) {
      const row: number[] = [];
      for (let column = 0; column < perCode; column += 1) {
        row.push(reply.readUInt32LE(32 + (code * perCode + column) * 4));
      }
      rows.push(row);
    }
    return rows;
  }

  /** Gives one key code new key symbols. */
  changeKeyboardMapping(keycode: number, keysyms: readonly number[]): void {
    const body = Buffer.alloc(4 + keysyms.length * 4);
    body[0] = keycode;
    body[1] = keysyms.length;
    keysyms.forEach((keysym, index) => body.writeUInt32LE(keysym, 4 + index * 4));
    this.send(OPCODE.changeKeyboardMapping, 1, body);
  }

  /** Closes the connection. Windows this client created go with it. */
  close(): void {
    if (!this.closed) {
      this.fail(new X11Error('the connection to the X server was closed'));
      this.socket.destroy();
    }
  }
}
