/**
 * What the compose end-to-end test of the API runs (tests/e2e/api.sh): a
 * page fetched through `POST /v1/fetch` in the browser of a persona of
 * each engine, with a client that was generated from the API's document
 * and is not Rust. It reads the stored result and the kept artifacts
 * back, and checks that a refusal is a problem document.
 *
 *   KAGE_API_URL      where the API listens
 *   KAGE_API_KEY      a key with the `fetch` scope
 *   KAGE_E2E_URL      the page to fetch
 *   KAGE_E2E_EXPECT   text the page's document contains
 *   KAGE_E2E_ENGINES  the engines to fetch it with (default both)
 *
 * It prints one JSON document and exits 0 when everything held.
 */
import assert from 'node:assert/strict';

import {
  createKageClient,
  fetchPage,
  KageProblemError,
  readArtifact,
  readResult,
  type FetchRequest,
} from './client.js';

function env(name: string, fallback?: string): string {
  const value = process.env[name] ?? fallback;
  if (value === undefined || value === '') {
    throw new Error(`${name} is not set`);
  }
  return value;
}

type Engine = NonNullable<NonNullable<FetchRequest['persona']>['engines']>[number];

function engines(): Engine[] {
  return env('KAGE_E2E_ENGINES', 'chromium,firefox')
    .split(',')
    .map((name) => {
      const engine = name.trim();
      assert.ok(engine === 'chromium' || engine === 'firefox', `${engine} is not an engine`);
      return engine;
    });
}

async function main(): Promise<unknown> {
  const client = createKageClient({ baseUrl: env('KAGE_API_URL'), token: env('KAGE_API_KEY') });
  const url = env('KAGE_E2E_URL');
  const expected = env('KAGE_E2E_EXPECT');
  const pages = [];

  for (const engine of engines()) {
    const fetched = await fetchPage(client, {
      url,
      render: 'browser',
      include: ['dom', 'headers', 'screenshot'],
      persona: { engines: [engine] },
      timeout_ms: 60_000,
    });
    assert.ok('outcome' in fetched, `${engine}: the fetch outlasted the wall time`);
    assert.equal(fetched.outcome, 'ok', `${engine}: ${fetched.reason ?? fetched.outcome}`);
    assert.equal(fetched.status, 200);
    assert.equal(fetched.attempts.at(-1)?.render, 'browser');
    // A real persona of that engine, named by a handle and not by its id.
    assert.equal(fetched.persona?.engine, engine);
    assert.match(fetched.persona.id, /^p_[0-9a-f]{16}$/);
    assert.ok(fetched.dom_html?.includes(expected), `${engine}: the document lacks ${expected}`);
    assert.ok((fetched.headers ?? []).some((header) => header.name === 'content-type'));
    assert.equal(fetched.behavior?.applied, 'policy');

    // What was kept is served again: the document as it was returned,
    // and a picture.
    const documentId = fetched.artifacts['dom_html'];
    const pictureId = fetched.artifacts['screenshot'];
    assert.ok(documentId !== undefined && pictureId !== undefined, `${engine}: artifacts`);
    const document = await readArtifact(client, documentId);
    assert.equal(Buffer.from(document.bytes).toString('utf8'), fetched.dom_html);
    const picture = await readArtifact(client, pictureId);
    // WebP unless asked otherwise; Firefox encodes PNG and JPEG only, and
    // the artifact says which it is.
    const [type, magic] = engine === 'firefox' ? ['image/png', '\x89PNG'] : ['image/webp', 'RIFF'];
    assert.equal(picture.contentType, type, `${engine}: the screenshot's type`);
    assert.equal(Buffer.from(picture.bytes.subarray(0, 4)).toString('latin1'), magic);

    // And the result is found again under the request's id.
    const stored = await readResult(client, fetched.id);
    assert.ok('outcome' in stored);
    assert.equal(stored.outcome, 'ok');
    assert.equal(stored.dom_html, fetched.dom_html);
    assert.deepEqual(stored.artifacts, fetched.artifacts);

    pages.push({
      engine,
      id: fetched.id,
      persona: fetched.persona.id,
      status: fetched.status,
      document_bytes: document.bytes.length,
      screenshot_bytes: picture.bytes.length,
      dwell_ms: fetched.behavior.dwell_ms ?? null,
      total_ms: fetched.timing.total_ms,
    });
  }

  // A request that cannot be made is refused with a stable code.
  const refusal = await fetchPage(client, { url: 'http://169.254.169.254/latest/meta-data/' }).then(
    () => null,
    (error: unknown) => error,
  );
  assert.ok(refusal instanceof KageProblemError, 'a metadata address was fetched');
  assert.equal(refusal.status, 422);
  assert.equal(refusal.problem.code, 'ssrf_denied');

  return { pages, refused: refusal.problem.code };
}

try {
  process.stdout.write(`${JSON.stringify(await main())}\n`);
} catch (error) {
  process.stderr.write(
    `${error instanceof Error ? (error.stack ?? error.message) : String(error)}\n`,
  );
  process.exitCode = 1;
}
