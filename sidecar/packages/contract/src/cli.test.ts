import { describe, expect, it } from 'vitest';

import { buildOpenApiDocument, runCli, serializeOpenApi, type CliIo } from './index.js';

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

const current = serializeOpenApi(buildOpenApiDocument());

describe('contract command line', () => {
  it('emits the document', () => {
    const io = fakeIo();
    expect(runCli(['emit', 'openapi.json'], io)).toBe(0);
    expect(io.files.get('openapi.json')).toBe(current);
    expect(io.stdout).toEqual(['wrote openapi.json']);
  });

  it('checks a committed document', () => {
    expect(runCli(['check', 'openapi.json'], fakeIo({ 'openapi.json': current }))).toBe(0);

    const stale = fakeIo({
      'openapi.json': current.replace('Kage browser sidecar', 'Something else'),
    });
    expect(runCli(['check', 'openapi.json'], stale)).toBe(1);
    expect(stale.stderr.join('\n')).toContain('just openapi');

    const missing = fakeIo();
    expect(runCli(['check', 'openapi.json'], missing)).toBe(1);
  });

  it('accepts a contract that is new or unchanged', () => {
    const fresh = fakeIo({ 'new.json': current });
    expect(runCli(['breaking', 'old.json', 'new.json'], fresh)).toBe(0);
    expect(fresh.stdout.join('\n')).toContain('the contract is new');

    const same = fakeIo({ 'old.json': current, 'new.json': current });
    expect(runCli(['breaking', 'old.json', 'new.json'], same)).toBe(0);
    expect(same.stdout).toEqual(['no breaking change in the sidecar contract']);
  });

  it('fails on a breaking change and names it', () => {
    const next = buildOpenApiDocument();
    delete next.paths['/v1/health'];
    const io = fakeIo({ 'old.json': current, 'new.json': serializeOpenApi(next) });
    expect(runCli(['breaking', 'old.json', 'new.json'], io)).toBe(1);
    expect(io.stderr.join('\n')).toContain('without a new major version');
    expect(io.stderr.join('\n')).toContain('GET /v1/health: the operation was removed');
  });

  it('passes a breaking change that comes with a new major version', () => {
    const next = buildOpenApiDocument();
    delete next.paths['/v1/health'];
    next.info['version'] = '2.0.0';
    const io = fakeIo({ 'old.json': current, 'new.json': serializeOpenApi(next) });
    expect(runCli(['breaking', 'old.json', 'new.json'], io)).toBe(0);
    expect(io.stdout.join('\n')).toContain('covered by the new major version');
    expect(io.stderr).toEqual([]);
  });

  it('refuses a missing new document and a file that is not JSON', () => {
    expect(runCli(['breaking', 'old.json', 'new.json'], fakeIo({ 'old.json': current }))).toBe(1);
    expect(() =>
      runCli(
        ['breaking', 'old.json', 'new.json'],
        fakeIo({ 'old.json': '{', 'new.json': current }),
      ),
    ).toThrow(/old\.json is not JSON/);
  });

  it.each([[[]], [['emit']], [['check']], [['breaking', 'one']], [['frobnicate']]])(
    'prints the usage for %j',
    (argv) => {
      const io = fakeIo();
      expect(runCli(argv, io)).toBe(2);
      expect(io.stderr.join('\n')).toContain('usage: contract');
    },
  );
});
