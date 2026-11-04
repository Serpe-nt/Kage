import { describe, expect, it } from 'vitest';

import { DocumentRequests } from './document-requests.js';

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
    requests.answered({ requestId: 'C', type: 'Document', response: { status: 200 } });
    expect(requests.unanswered()).toEqual([]);

    // An iframe's document, a script, and a new navigation of the tab.
    requests.requested(asked('D', 'http://ads.test/frame', false, 'FRAME'));
    requests.seen(seen('D', 302, 'data:text/html,x'));
    requests.requested({ ...asked('E', 'http://site.test/app.js'), type: 'Script' });
    expect(requests.unanswered()).toEqual([]);
    requests.requested(asked('F', 'http://site.test/next'));
    requests.seen(seen('F', 302));
    requests.seen(seen('other', 302, 'file:///x'));
    requests.answered({ requestId: 'other', type: 'Document', response: { status: 200 } });
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
