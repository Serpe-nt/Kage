import http from 'node:http';
import net, { type AddressInfo } from 'node:net';

/** One request the test proxy was asked to relay. */
export interface ProxyEntry {
  /** `http` for an absolute-form request, `connect` for a tunnel. */
  readonly kind: 'http' | 'connect';
  /** `host:port` the browser asked for. */
  readonly target: string;
  readonly path: string | null;
  /** Whether the request carried the right credentials. */
  readonly authorized: boolean;
  /** Whether the proxy relayed it. */
  readonly relayed: boolean;
}

/** A running test proxy. */
export interface TestProxy {
  /** `http://127.0.0.1:<port>`. */
  readonly server: string;
  readonly username: string;
  readonly password: string;
  /** Everything the proxy was asked, oldest first. */
  readonly log: ProxyEntry[];
  stop(): Promise<void>;
}

/** Where the test proxy sends what it relays. */
export interface TestProxyRoutes {
  /**
   * Port on loopback that every plain-http request goes to, whatever host
   * the browser named: the names of the fixture site exist only here, so a
   * page that loads proves the browser asked the proxy and resolved nothing
   * itself.
   */
  readonly httpPort: number;
  /** Tunnels the proxy opens, by the `host:port` the browser asks for; every other CONNECT is refused. */
  readonly tunnels?: Readonly<Record<string, number>>;
}

/**
 * An HTTP proxy with Basic authentication for tests: absolute-form requests
 * and CONNECT, like the residential proxies Kage uses.
 *
 * It never reaches the internet. The browser's own background traffic
 * (updates, component downloads) arrives as CONNECT to hosts that are not in
 * `tunnels` and is refused, so a test run contacts nothing outside the
 * machine.
 */
export async function startTestProxy(routes: TestProxyRoutes): Promise<TestProxy> {
  const username = 'persona-user';
  const password = 'persona-pass';
  const expected = `Basic ${Buffer.from(`${username}:${password}`).toString('base64')}`;
  const log: ProxyEntry[] = [];
  const sockets = new Set<net.Socket>();

  const server = http.createServer((request, response) => {
    const authorized = request.headers['proxy-authorization'] === expected;
    let url: URL;
    try {
      url = new URL(request.url ?? '');
    } catch {
      response.writeHead(400).end();
      return;
    }
    log.push({
      kind: 'http',
      target: url.host,
      path: url.pathname + url.search,
      authorized,
      relayed: authorized,
    });
    if (!authorized) {
      response.writeHead(407, { 'proxy-authenticate': 'Basic realm="kage-test"' }).end();
      return;
    }
    // The header lines as the browser wrote them, in its order and
    // spelling, without the ones that are for the proxy itself.
    const headers: string[] = [];
    for (let index = 0; index < request.rawHeaders.length; index += 2) {
      const name = request.rawHeaders[index] ?? '';
      if (!/^proxy-/i.test(name)) {
        headers.push(name, request.rawHeaders[index + 1] ?? '');
      }
    }
    const upstream = http.request(
      {
        host: '127.0.0.1',
        port: routes.httpPort,
        method: request.method,
        path: url.pathname + url.search,
        headers,
      },
      (answer) => {
        response.writeHead(answer.statusCode ?? 502, answer.rawHeaders);
        answer.pipe(response);
      },
    );
    upstream.on('error', () => {
      response.writeHead(502).end();
    });
    request.pipe(upstream);
  });

  server.on('connect', (request, socket: net.Socket, head: Buffer) => {
    const target = request.url ?? '';
    const authorized = request.headers['proxy-authorization'] === expected;
    const port = routes.tunnels?.[target];
    const relayed = authorized && port !== undefined;
    log.push({ kind: 'connect', target, path: null, authorized, relayed });
    if (!authorized) {
      socket.end(
        'HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm="kage-test"\r\n\r\n',
      );
      return;
    }
    if (port === undefined) {
      socket.end('HTTP/1.1 403 Forbidden\r\n\r\n');
      return;
    }
    const upstream = net.connect(port, '127.0.0.1', () => {
      socket.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      if (head.length > 0) {
        upstream.write(head);
      }
      upstream.pipe(socket);
      socket.pipe(upstream);
    });
    upstream.on('error', () => socket.destroy());
    socket.on('error', () => upstream.destroy());
  });

  server.on('connection', (socket) => {
    sockets.add(socket);
    socket.once('close', () => sockets.delete(socket));
  });

  await new Promise<void>((resolve) => {
    server.listen(0, '127.0.0.1', resolve);
  });
  const { port } = server.address() as AddressInfo;
  return {
    server: `http://127.0.0.1:${port}`,
    username,
    password,
    log,
    stop: () =>
      new Promise((resolve) => {
        for (const socket of sockets) {
          socket.destroy();
        }
        server.close(() => {
          resolve();
        });
      }),
  };
}
