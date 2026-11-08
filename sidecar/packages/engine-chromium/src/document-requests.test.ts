import { describe, expect, it } from 'vitest';

import { DocumentRequests, wireLines } from './document-requests.js';

/**
 * The debugger's events in the order Chrome 155 sent them in the probe of
 * F167 (a 302 to `file:` is refused, `net::ERR_UNSAFE_REDIRECT`; one to
 * `ftp:` is followed and aborted, `net::ERR_ABORTED`; one to `/ok` arrives).
 */
const MAIN = 'MAIN';
const asked = (requestId: string, url: string, redirected = false, frameId = MAIN) => ({
  requestId,
  frameId,
  type: 'Document',
  request: { url },
  ...(redirected ? { redirectResponse: { status: 302 } } : {}),
});
const seen = (requestId: string, statusCode: number, location?: string) => ({
  requestId,
  statusCode,
  headers: location === undefined ? {} : { Location: location },
});

describe('the main frame’s document requests as the debugger saw them (F167)', () => {
  it('names a redirect Chrome refused to follow, which the driver never saw', () => {
    const requests = new DocumentRequests(MAIN);
    requests.requested(asked('A', 'http://site.test/f'));
    requests.seen(seen('A', 302, 'file:///etc/passwd'));
    expect(requests.unanswered()).toEqual([
      { url: 'http://site.test/f', status: 302, location: 'file:///etc/passwd' },
    ]);
  });

  it('names a redirect Chrome followed to a scheme it then gave up on', () => {
    const requests = new DocumentRequests(MAIN);
    requests.requested(asked('B', 'http://site.test/x'));
    requests.seen(seen('B', 302, '/y'));
    requests.requested(asked('B', 'http://site.test/y', true));
    requests.seen(seen('B', 301, 'ftp://site.test/x'));
    requests.requested(asked('B', 'ftp://site.test/x', true));
    expect(requests.unanswered()).toEqual([
      { url: 'http://site.test/x', status: 302, location: '/y' },
      { url: 'http://site.test/y', status: 301, location: 'ftp://site.test/x' },
    ]);
  });

  it('says nothing of a request a document came of, of other frames, or of other answers', () => {
    const requests = new DocumentRequests(MAIN);
    expect(requests.unanswered()).toEqual([]);
    requests.requested(asked('C', 'http://site.test/h'));
    requests.seen(seen('C', 302, '/ok'));
    requests.requested(asked('C', 'http://site.test/ok', true));
    requests.seen(seen('C', 200));
    requests.answered({
      requestId: 'C',
      type: 'Document',
      response: { url: 'http://site.test/ok', status: 200, headers: {} },
    });
    expect(requests.unanswered()).toEqual([]);

    // An iframe's document, a script, and a new navigation of the tab.
    requests.requested(asked('D', 'http://ads.test/frame', false, 'FRAME'));
    requests.seen(seen('D', 302, 'data:text/html,x'));
    requests.requested({ ...asked('E', 'http://site.test/app.js'), type: 'Script' });
    expect(requests.unanswered()).toEqual([]);
    requests.requested(asked('F', 'http://site.test/next'));
    requests.seen(seen('F', 302));
    requests.seen(seen('other', 302, 'file:///x'));
    requests.answered({
      requestId: 'other',
      type: 'Document',
      response: { url: 'http://site.test/ok', status: 200, headers: {} },
    });
    expect(requests.unanswered()).toEqual([]);
  });

  it('takes every frame’s document for the tab’s until the main frame is known', () => {
    const requests = new DocumentRequests();
    requests.requested(asked('G', 'http://site.test/j', false, 'ANY'));
    requests.seen(seen('G', 307, 'javascript:alert(1)'));
    expect(requests.unanswered()[0]?.location).toBe('javascript:alert(1)');
    requests.knowMainFrame(MAIN);
    requests.requested(asked('H', 'http://site.test/k', false, 'ANY'));
    expect(requests.unanswered()[0]?.location).toBe('javascript:alert(1)');
  });

  it('keeps no more than fifty redirects of one request', () => {
    const requests = new DocumentRequests(MAIN);
    requests.requested(asked('I', 'http://site.test/0'));
    for (let hop = 1; hop <= 60; hop += 1) {
      requests.seen(seen('I', 302, `/${hop}`));
      requests.requested(asked('I', `http://site.test/${hop}`, true));
    }
    const hops = requests.unanswered();
    expect(hops).toHaveLength(50);
    expect(hops[49]).toEqual({ url: 'http://site.test/49', status: 302, location: '/50' });
  });
});

describe('the response the tab shows, as the debugger announced it (F178, F204)', () => {
  const answered = (
    requestId: string,
    url: string,
    status: number,
    headers: Record<string, string>,
  ) => ({
    requestId,
    type: 'Document',
    response: { url, status, headers },
  });

  it('is the kept page Chrome shows where the network answered 304', () => {
    const requests = new DocumentRequests(MAIN);
    requests.requested(asked('K', 'http://site.test/kept#top'));
    requests.seen(seen('K', 304));
    requests.answered(
      answered('K', 'http://site.test/kept', 200, {
        'Content-Type': 'text/html',
        ETag: '"v1"',
        'X-Twice': 'one\ntwo',
      }),
    );
    expect(requests.revalidated('http://site.test/kept')).toEqual({
      status: 200,
      headers: [
        { name: 'Content-Type', value: 'text/html' },
        { name: 'ETag', value: '"v1"' },
        { name: 'X-Twice', value: 'one' },
        { name: 'X-Twice', value: 'two' },
      ],
    });
    // Another address, or a page the network answered in full: the driver's own.
    expect(requests.revalidated('http://site.test/other')).toBeNull();
    requests.requested(asked('L', 'http://site.test/full'));
    requests.seen(seen('L', 200));
    requests.answered(answered('L', 'http://site.test/full', 200, { ETag: '"v2"' }));
    expect(requests.revalidated('http://site.test/full')).toBeNull();
    expect(new DocumentRequests(MAIN).revalidated('http://site.test/full')).toBeNull();
  });

  it('says the status of an error answer Chrome replaced with its own page, at the address it answered', () => {
    const requests = new DocumentRequests(MAIN);
    expect(requests.replacedStatus('http://site.test/x')).toBeNull();
    // Heard on the wire only.
    requests.requested(asked('M', 'http://site.test/walled'));
    requests.seen(seen('M', 403));
    expect(requests.replacedStatus('http://site.test/walled#f')).toBe(403);
    expect(requests.replacedStatus('http://site.test/other')).toBeNull();
    // Announced with a document too, after a redirect.
    requests.requested(asked('N', 'http://site.test/a'));
    requests.seen(seen('N', 302, '/b'));
    requests.requested(asked('N', 'http://site.test/b', true));
    requests.answered(answered('N', 'http://site.test/b', 451, {}));
    expect(requests.replacedStatus('http://site.test/b')).toBe(451);
    expect(requests.replacedStatus('http://site.test/a')).toBeNull();
    // An answer that is no error, or none at all.
    requests.requested(asked('O', 'http://site.test/fine'));
    requests.seen(seen('O', 200));
    expect(requests.replacedStatus('http://site.test/fine')).toBeNull();
    requests.requested(asked('P', 'http://site.test/silent'));
    expect(requests.replacedStatus('http://site.test/silent')).toBeNull();
    // A proxy's 407 asks for the persona's credentials: no answer of the site.
    requests.requested(asked('Q', 'http://site.test/behind'));
    requests.seen(seen('Q', 407));
    requests.answered(answered('Q', 'http://site.test/behind', 407, {}));
    expect(requests.replacedStatus('http://site.test/behind')).toBeNull();
  });
});

describe('the field lines of a document in the order the site sent them (browser-04)', () => {
  const text = [
    'HTTP/1.1 200 OK',
    'Content-Type: text/html; charset=utf-8',
    'x-qa-zulu: 1',
    'X-QA-Alpha:2',
    'x-qa-mike: 3',
    'Set-Cookie: a=1; Path=/',
    'x-qa-bravo:   4  ',
    'Set-Cookie: b=2',
    'X-Folded: one',
    ' two',
    '\tthree',
    'not a field line',
    '',
    '',
  ].join('\r\n');
  const inOrder = [
    { name: 'Content-Type', value: 'text/html; charset=utf-8' },
    { name: 'x-qa-zulu', value: '1' },
    { name: 'X-QA-Alpha', value: '2' },
    { name: 'x-qa-mike', value: '3' },
    { name: 'Set-Cookie', value: 'a=1; Path=/' },
    { name: 'x-qa-bravo', value: '4' },
    { name: 'Set-Cookie', value: 'b=2' },
    { name: 'X-Folded', value: 'one two three' },
  ];

  it('reads the head the debugger quoted from the wire', () => {
    expect(wireLines(text)).toEqual(inOrder);
    expect(wireLines(text.split('\r\n').join('\n'))).toEqual(inOrder);
    expect(wireLines('')).toBeNull();
    expect(wireLines('Content-Type: text/html\r\n')).toBeNull();
    expect(wireLines('HTTP/1.1 204 No Content\r\n\r\n')).toEqual([]);
  });

  it('is the document’s head where the debugger had its text, and nothing else', () => {
    const seenWith = (requestId: string, statusCode: number, headersText?: string) => ({
      requestId,
      statusCode,
      headers: { 'content-type': 'text/html' },
      ...(headersText === undefined ? {} : { headersText }),
    });
    const document = (requestId: string, url: string, status: number) => ({
      requestId,
      type: 'Document',
      response: { url, status, headers: {} },
    });
    const requests = new DocumentRequests(MAIN);
    expect(requests.asSent('http://site.test/p', 200)).toBeNull();
    requests.requested(asked('R', 'http://site.test/a'));
    requests.seen(seen('R', 302, '/p'));
    requests.requested(asked('R', 'http://site.test/p', true));
    requests.seen(seenWith('R', 200, text));
    // Not before the document came.
    expect(requests.asSent('http://site.test/p', 200)).toBeNull();
    requests.answered(document('R', 'http://site.test/p', 200));
    expect(requests.asSent('http://site.test/p#top', 200)).toEqual({
      status: 200,
      headers: inOrder,
    });
    // The redirect's address, another status: not this document's head.
    expect(requests.asSent('http://site.test/a', 200)).toBeNull();
    expect(requests.asSent('http://site.test/p', 304)).toBeNull();
    // HTTP/2: the debugger has no text, the driver's lines stay.
    requests.requested(asked('S', 'https://site.test/h2'));
    requests.seen(seenWith('S', 200));
    requests.answered(document('S', 'https://site.test/h2', 200));
    expect(requests.asSent('https://site.test/h2', 200)).toBeNull();
  });
});

describe('a 304 Chrome kept no page for (class-02)', () => {
  const announced = (requestId: string, url: string, status: number) => ({
    requestId,
    type: 'Document',
    response: { url, status, headers: {} },
  });

  it('is the wire’s 304 and its fields at the address it answered', () => {
    const requests = new DocumentRequests(MAIN);
    expect(requests.notModified('http://site.test/nm')).toBeNull();
    requests.requested(asked('T', 'http://site.test/nm'));
    requests.seen({
      requestId: 'T',
      statusCode: 304,
      headers: { etag: '"x"', date: 'Sun, 08 Nov 2026 12:00:00 GMT' },
      headersText:
        'HTTP/1.1 304 Not Modified\r\nETag: "x"\r\nDate: Sun, 08 Nov 2026 12:00:00 GMT\r\n\r\n',
    });
    expect(requests.notModified('http://site.test/nm#f')).toEqual({
      status: 304,
      headers: [
        { name: 'ETag', value: '"x"' },
        { name: 'Date', value: 'Sun, 08 Nov 2026 12:00:00 GMT' },
      ],
    });
    // Announced as a document of its own, still the wire's 304.
    requests.answered(announced('T', 'http://site.test/nm', 304));
    expect(requests.notModified('http://site.test/nm')?.status).toBe(304);
    expect(requests.notModified('http://site.test/other')).toBeNull();
    // Without the head's text: the debugger's fields.
    requests.requested(asked('U', 'https://site.test/h2'));
    requests.seen({ requestId: 'U', statusCode: 304, headers: { etag: '"y"' } });
    expect(requests.notModified('https://site.test/h2')).toEqual({
      status: 304,
      headers: [{ name: 'etag', value: '"y"' }],
    });
  });

  it('is nothing for a kept page brought up to date, or any other answer', () => {
    const requests = new DocumentRequests(MAIN);
    requests.requested(asked('V', 'http://site.test/kept'));
    requests.seen(seen('V', 304));
    requests.answered(announced('V', 'http://site.test/kept', 200));
    expect(requests.notModified('http://site.test/kept')).toBeNull();
    requests.requested(asked('W', 'http://site.test/fine'));
    requests.seen(seen('W', 200));
    expect(requests.notModified('http://site.test/fine')).toBeNull();
  });
});

describe('a file the main frame was answered with (F120)', () => {
  const answered = (requestId: string, headers: Record<string, string>, type = 'Document') => ({
    requestId,
    type,
    response: { url: 'http://site.test/bin', status: 200, headers },
  });

  it('is the main frame’s document answered with an attachment, whatever the spelling', () => {
    const requests = new DocumentRequests(MAIN);
    requests.requested(asked('F', 'http://site.test/bin'));
    expect(
      requests.savesFile(answered('F', { 'Content-Disposition': 'Attachment; filename="x.bin"' })),
    ).toBe(true);
    expect(requests.savesFile(answered('F', { 'content-disposition': ' attachment' }))).toBe(true);
  });

  it('is never a page shown inline, another request, or another frame’s', () => {
    const requests = new DocumentRequests(MAIN);
    requests.requested(asked('P', 'http://site.test/page'));
    expect(requests.savesFile(answered('P', { 'content-disposition': 'inline' }))).toBe(false);
    expect(requests.savesFile(answered('P', {}))).toBe(false);
    expect(requests.savesFile(answered('X', { 'content-disposition': 'attachment' }))).toBe(false);
    expect(
      requests.savesFile(answered('P', { 'content-disposition': 'attachment' }, 'Script')),
    ).toBe(false);
    requests.requested(asked('I', 'http://site.test/frame', false, 'FRAME'));
    expect(requests.savesFile(answered('I', { 'content-disposition': 'attachment' }))).toBe(false);
  });
});
