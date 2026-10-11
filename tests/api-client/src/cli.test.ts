import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';

import { runCli, type CliIo } from './cli.js';
import { generateTypes, validateDocument } from './document.js';

interface FakeIo extends CliIo {
  readonly files: Map<string, string>;
  readonly stdout: string[];
  readonly stderr: string[];
}

function fakeIo(files: Record<string, string> = {}): FakeIo {
  const io: FakeIo = {
    files: new Map(Object.entries(files)),
    stdout: [],
    stderr: [],
    read: (path) => io.files.get(path) ?? null,
    write: (path, text) => {
      io.files.set(path, text);
    },
    out: (line) => {
      io.stdout.push(line);
    },
    err: (line) => {
      io.stderr.push(line);
    },
  };
  return io;
}

/** A file of the repository, by its path from the root. */
const repository = (path: string): string =>
  readFileSync(fileURLToPath(new URL(`../../../${path}`, import.meta.url)), 'utf8');

const committed = repository('crates/kage-api/openapi.json');
const minimal = JSON.stringify({
  openapi: '3.1.0',
  info: { title: 'T', version: '1' },
  paths: {
    '/ping': {
      get: { operationId: 'ping', responses: { '204': { description: 'pong' } } },
    },
  },
});

describe('the committed document', () => {
  it('is a valid OpenAPI 3.1 document', async () => {
    assert.equal(await validateDocument(committed), '3.1');
  });

  it('is what the committed types were generated from', async () => {
    const types = repository('tests/api-client/src/schema.ts').replaceAll('\r\n', '\n');
    assert.equal(
      await generateTypes(committed),
      types,
      'src/schema.ts is out of date: run `just openapi`',
    );
  });

  it('describes the fetch a client is generated for', () => {
    const document = JSON.parse(committed) as {
      paths: Record<string, Record<string, { operationId?: string }>>;
    };
    assert.equal(document.paths['/v1/fetch']?.['post']?.operationId, 'fetch');
  });
});

describe('the command line', () => {
  it('validates a document', async () => {
    const io = fakeIo({ 'openapi.json': minimal });
    assert.equal(await runCli(['validate', 'openapi.json'], io), 0);
    assert.deepEqual(io.stdout, ['openapi.json is a valid OpenAPI 3.1 document']);
  });

  it('refuses a document that is not OpenAPI, and says why', async () => {
    for (const [text, reason] of [
      ['{', 'not JSON'],
      ['[]', 'not a JSON object'],
      [JSON.stringify({ openapi: '3.1.0', info: { title: 'T' }, paths: {} }), 'version'],
      [JSON.stringify({ swagger: '2.0', info: { title: 'T', version: '1' }, paths: {} }), '2.0'],
      [
        JSON.stringify({ openapi: '3.0.3', info: { title: 'T', version: '1' }, paths: {} }),
        'the API',
      ],
    ] as const) {
      const io = fakeIo({ 'openapi.json': text });
      assert.equal(await runCli(['validate', 'openapi.json'], io), 1, text);
      assert.match(io.stderr.join('\n'), new RegExp(reason), text);
    }
  });

  it('generates the types and checks that they are current', async () => {
    const io = fakeIo({ 'openapi.json': minimal });
    assert.equal(await runCli(['generate', 'openapi.json', 'schema.ts'], io), 0);
    const types = io.files.get('schema.ts') ?? '';
    assert.match(types, /Do not edit/);
    assert.match(types, /"\/ping": \{/);
    assert.deepEqual(io.stdout, ['wrote schema.ts']);

    assert.equal(await runCli(['check', 'openapi.json', 'schema.ts'], io), 0);
    // A checkout with other line endings is still current.
    io.files.set('schema.ts', types.replaceAll('\n', '\r\n'));
    assert.equal(await runCli(['check', 'openapi.json', 'schema.ts'], io), 0);

    // Types of another document, and types that are missing, are not.
    io.files.set('schema.ts', types.replace('/ping', '/pong'));
    assert.equal(await runCli(['check', 'openapi.json', 'schema.ts'], io), 1);
    assert.match(io.stderr.join('\n'), /just openapi/);
    io.files.delete('schema.ts');
    assert.equal(await runCli(['check', 'openapi.json', 'schema.ts'], io), 1);
  });

  it('says how it is used', async () => {
    for (const argv of [[], ['emit'], ['validate'], ['generate', 'openapi.json'], ['check']]) {
      const io = fakeIo({ 'openapi.json': minimal });
      assert.equal(await runCli(argv, io), 2, argv.join(' '));
      assert.match(io.stderr.join('\n'), /^usage: api-client/);
    }
    const io = fakeIo();
    assert.equal(await runCli(['validate', 'missing.json'], io), 1);
    assert.match(io.stderr.join('\n'), /does not exist/);
  });
});
