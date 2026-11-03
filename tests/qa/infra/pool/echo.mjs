#!/usr/bin/env node
// A programmable copy of `world.mjs echo` for the pool's QA (round two):
// the health probe's echo and geo lookup, which can give each proxy of the
// world an exit address and an ASN of its own, so that the /24 and ASN rules
// of spec §8.2 can be observed in a world where every container shares one
// compose subnet.
//
//   node echo.mjs --port 8089 --exits 'proxy-a=203.0.113.10|AS64501;proxy-b=203.0.113.20|AS64501;...'
//
// /ip      the address the request came from; when it is the address a name
//          of --exits resolves to (looked up at the time of asking), the exit
//          address declared for that name instead.
// /geo     JSON as ipinfo.io writes it: the declared exit and its `org`
//          (`AS64501 QA Net 64501`), in Berlin; an unknown source keeps its
//          own address and `AS64500 Example Broadband`.
// POST /__set  {"name":"proxy-a","exit":"198.51.100.7","asn":"AS64502"} changes
//          or adds one mapping at run time; {"name":"proxy-a"} removes it.
// GET /__log   every request: path, the source, what was answered.
import dns from 'node:dns';
import http from 'node:http';

const args = process.argv.slice(2);
const option = (name, fallback) => {
  const index = args.indexOf(`--${name}`);
  return index >= 0 && args[index + 1] !== undefined ? args[index + 1] : fallback;
};
const port = Number(option('port', '8089'));
/** name -> { exit, asn } */
const exits = new Map();
for (const spec of option('exits', '').split(';').filter(Boolean)) {
  const [name, rest] = spec.split('=');
  const [exit, asn] = (rest ?? '').split('|');
  exits.set(name, { exit, asn: asn || 'AS64500' });
}
const log = [];
const remote = (socket) => (socket.remoteAddress ?? '').replace(/^::ffff:/, '');

const mappingOf = async (ip) => {
  for (const [name, mapping] of exits) {
    try {
      const { address } = await dns.promises.lookup(name, { family: 4 });
      if (address === ip) return { name, ...mapping };
    } catch {
      // Not up yet: not this one.
    }
  }
  return null;
};

http
  .createServer(async (req, res) => {
    const source = remote(req.socket);
    const path = new URL(req.url ?? '/', 'http://echo.invalid').pathname;
    if (req.method === 'POST' && path === '/__set') {
      let body = '';
      req.on('data', (chunk) => (body += chunk));
      req.on('end', () => {
        try {
          const change = JSON.parse(body);
          if (change.exit) exits.set(change.name, { exit: change.exit, asn: change.asn || 'AS64500' });
          else exits.delete(change.name);
          res.writeHead(200, { 'content-type': 'application/json' });
          res.end(JSON.stringify(Object.fromEntries(exits)));
        } catch (error) {
          res.writeHead(400).end(String(error));
        }
      });
      return;
    }
    if (path === '/__log') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(JSON.stringify({ exits: Object.fromEntries(exits), log: log.slice(-500) }));
      return;
    }
    const mapping = await mappingOf(source);
    const ip = mapping ? mapping.exit : source;
    const asn = mapping ? mapping.asn : 'AS64500';
    log.push({ at: new Date().toISOString(), path, source, name: mapping?.name ?? null, ip, asn });
    if (path === '/geo') {
      res.writeHead(200, { 'content-type': 'application/json' });
      res.end(
        JSON.stringify({
          ip,
          city: 'Berlin',
          region: 'Berlin',
          country: 'DE',
          loc: '52.5200,13.4050',
          org: mapping ? `${asn} QA Net ${asn.replace(/^AS/, '')}` : 'AS64500 Example Broadband',
          timezone: 'Europe/Berlin',
        }),
      );
      return;
    }
    res.writeHead(200, { 'content-type': 'text/plain' });
    res.end(`${ip}\n`);
  })
  .listen(port, '0.0.0.0');

for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => process.exit(0));
