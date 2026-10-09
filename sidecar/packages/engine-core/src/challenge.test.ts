import { describe, expect, it } from 'vitest';

import {
  challengeSignals,
  cookieKey,
  cookiesForOrigin,
  findInterstitial,
  importRejection,
  toBrowserCookieParam,
  toContractCookie,
  waitForChallenge,
  type PageObservation,
} from './index.js';

const content: PageObservation = {
  title: 'Listing, page 2',
  text: 'Forty results for used cars.',
  scriptUrls: ['https://www.example.org/app.js', 'https://cdn.example.org/lib.js'],
  frameUrls: [],
};

describe('findInterstitial', () => {
  it.each<[string, Partial<PageObservation>, string]>([
    ['Cloudflare by its title', { title: 'Just a moment...' }, 'cloudflare:just-a-moment'],
    [
      'Cloudflare by its block page title',
      { title: 'Attention Required! | Cloudflare' },
      'cloudflare:attention-required',
    ],
    [
      'Cloudflare by its text',
      { text: 'Verifying you are human. This may take a few seconds.' },
      'cloudflare:checking-browser',
    ],
    [
      'Akamai by title and reference',
      {
        title: 'Access Denied',
        text: "You don't have permission. Reference #18.2d6f1602.1728480000.1a2b3c",
      },
      'akamai:access-denied',
    ],
    [
      'PerimeterX by its button',
      { text: 'Press & Hold to confirm you are a human' },
      'perimeterx:press-and-hold',
    ],
    [
      'PerimeterX by its title',
      { title: 'Access to this page has been denied' },
      'perimeterx:access-denied',
    ],
    [
      'Imperva by its incident line',
      { text: 'Request unsuccessful. Incapsula incident ID: 123-456' },
      'imperva:incident',
    ],
    [
      'DataDome by its captcha frame',
      { text: '', frameUrls: ['https://geo.captcha-delivery.com/captcha/?initialCid=x'] },
      'datadome:captcha',
    ],
  ])('recognises %s', (_label, change, marker) => {
    expect(findInterstitial({ ...content, ...change })).toBe(marker);
  });

  it('takes content for content', () => {
    expect(findInterstitial(content)).toBeNull();
    expect(findInterstitial({ title: null, text: null, scriptUrls: [], frameUrls: [] })).toBeNull();
    // "Access Denied" alone is a page title many sites use; Akamai's needs its reference line.
    expect(findInterstitial({ ...content, title: 'Access Denied' })).toBeNull();
  });

  it('does not take a long article that mentions a challenge for one', () => {
    const article = `${'word '.repeat(1000)} Press & Hold is how one vendor words its challenge.`;
    expect(findInterstitial({ ...content, text: article })).toBeNull();
    expect(
      findInterstitial({
        ...content,
        text: article,
        frameUrls: ['https://geo.captcha-delivery.com/c'],
      }),
    ).toBeNull();
    // A title rule needs no text.
    expect(findInterstitial({ ...content, text: article, title: 'Just a moment...' })).toBe(
      'cloudflare:just-a-moment',
    );
  });
});

describe('challengeSignals', () => {
  it('names vendors, hosts and the outcome of the wait', () => {
    const observation: PageObservation = {
      title: 'Shop',
      text: 'content',
      scriptUrls: [
        'https://www.example.org/app.js',
        'https://www.example.org/cdn-cgi/challenge-platform/h/b/orchestrate/chl_page/v1',
        'https://client.px-cloud.net/PXabc123/main.min.js',
        'https://www.google.com/recaptcha/api.js',
        'https://www.example.org/149e9513-01fa-4fb0-aad4-566afd725d1b/2d206a39/ips.js',
        'https://www.example.org/_Incapsula_Resource?SWJIYLWA=1',
        'https://abc.token.awswaf.com/abc/challenge.js',
        'https://www.example.org/akam/13/7a1b2c3d',
        'data:text/javascript,1',
      ],
      frameUrls: [
        'https://newassets.hcaptcha.com/captcha/v1/x/static/hcaptcha.html',
        'about:blank',
      ],
    };
    expect(challengeSignals(observation, { marker: null, resolved: null, waitedMs: 0 })).toEqual({
      vendors: [
        'akamai',
        'aws_waf',
        'cloudflare',
        'hcaptcha',
        'imperva',
        'kasada',
        'perimeterx',
        'recaptcha',
      ],
      script_hosts: [
        'abc.token.awswaf.com',
        'client.px-cloud.net',
        'www.example.org',
        'www.google.com',
      ],
      iframe_hosts: ['newassets.hcaptcha.com'],
      interstitial: false,
      marker: null,
      resolved: null,
      waited_ms: 0,
    });
  });

  it('counts the vendor of an interstitial even when no script names it', () => {
    const signals = challengeSignals(
      { ...content, scriptUrls: [] },
      { marker: 'akamai:access-denied', resolved: false, waitedMs: 15_000 },
    );
    expect(signals).toMatchObject({
      vendors: ['akamai'],
      interstitial: true,
      marker: 'akamai:access-denied',
      resolved: false,
      waited_ms: 15_000,
    });
  });
});

describe('waitForChallenge', () => {
  const challenge: PageObservation = { ...content, title: 'Just a moment...' };

  function script(pages: PageObservation[]) {
    const slept: number[] = [];
    let looks = 0;
    return {
      slept,
      looks: () => looks,
      look: () => {
        const page = pages[Math.min(looks, pages.length - 1)] ?? content;
        looks += 1;
        return Promise.resolve(page);
      },
      sleep: (ms: number) => {
        slept.push(ms);
        return Promise.resolve();
      },
    };
  }

  it('looks once at content and does not wait', async () => {
    const run = script([content]);
    const { wait, observation } = await waitForChallenge(run.look, 15_000, run.sleep);
    expect(wait).toEqual({ marker: null, resolved: null, waitedMs: 0 });
    expect(observation).toBe(content);
    expect(run.looks()).toBe(1);
    expect(run.slept).toEqual([]);
  });

  it('waits until the interstitial is gone and returns the page behind it', async () => {
    const run = script([challenge, challenge, challenge, content]);
    const { wait, observation } = await waitForChallenge(run.look, 15_000, run.sleep);
    expect(wait).toEqual({ marker: 'cloudflare:just-a-moment', resolved: true, waitedMs: 1500 });
    expect(observation).toBe(content);
    expect(run.slept).toEqual([500, 500, 500]);
  });

  it('gives up when the budget is spent, never waiting longer than the budget', async () => {
    const run = script([challenge]);
    const { wait } = await waitForChallenge(run.look, 1200, run.sleep);
    expect(wait).toEqual({ marker: 'cloudflare:just-a-moment', resolved: false, waitedMs: 1200 });
    expect(run.slept).toEqual([500, 500, 200]);
  });

  it('reports an interstitial without waiting when the budget is zero', async () => {
    const run = script([challenge]);
    const { wait } = await waitForChallenge(run.look, 0, run.sleep);
    expect(wait).toEqual({ marker: 'cloudflare:just-a-moment', resolved: false, waitedMs: 0 });
    expect(run.looks()).toBe(1);
  });
});

describe('cookies', () => {
  const now = new Date('2026-10-09T12:00:00Z');
  const row = {
    domain: 'example.org',
    host_only: false,
    path: '/',
    name: 'sid',
    value: 'abc',
    secure: true,
    http_only: true,
    same_site: 'lax' as const,
    expires_at: '2027-01-01T00:00:00.000Z',
  };

  it('turns a browser cookie into a row of http_cookies', () => {
    expect(
      toContractCookie({
        name: 'sid',
        value: 'abc',
        domain: '.Example.org',
        path: '/',
        expires: 1_798_761_600,
        httpOnly: true,
        secure: true,
        session: false,
        sameSite: 'Lax',
      }),
    ).toEqual(row);
  });

  it('keeps host-only, session and attribute-less cookies as what they are', () => {
    expect(
      toContractCookie({
        name: 't',
        value: '',
        domain: 'www.example.org',
        path: '',
        expires: -1,
        httpOnly: false,
        secure: false,
      }),
    ).toEqual({
      domain: 'www.example.org',
      host_only: true,
      path: '/',
      name: 't',
      value: '',
      secure: false,
      http_only: false,
      same_site: null,
      expires_at: null,
    });
    const base = {
      name: 'a',
      value: 'b',
      domain: 'x.test',
      path: '/',
      httpOnly: false,
      secure: false,
    };
    expect(toContractCookie({ ...base, session: true, expires: 99 }).expires_at).toBeNull();
    expect(toContractCookie(base).expires_at).toBeNull();
    expect(toContractCookie({ ...base, sameSite: 'None' }).same_site).toBe('none');
    expect(toContractCookie({ ...base, sameSite: 'Strict' }).same_site).toBe('strict');
    expect(toContractCookie({ ...base, sameSite: 'Unspecified' }).same_site).toBeNull();
  });

  it('writes a row the way a browser protocol takes it', () => {
    expect(toBrowserCookieParam(row)).toEqual({
      name: 'sid',
      value: 'abc',
      domain: '.example.org',
      path: '/',
      secure: true,
      httpOnly: true,
      sameSite: 'Lax',
      expires: 1_798_761_600,
    });
    // A host-only cookie is given by URL, so the browser keeps it host-only.
    expect(
      toBrowserCookieParam({
        ...row,
        host_only: true,
        secure: false,
        same_site: null,
        expires_at: null,
        path: '/shop',
      }),
    ).toEqual({
      name: 'sid',
      value: 'abc',
      url: 'http://example.org/shop',
      path: '/shop',
      secure: false,
      httpOnly: true,
    });
    expect(toBrowserCookieParam({ ...row, host_only: true }).url).toBe('https://example.org/');
  });

  it('round-trips through both forms', () => {
    const param = toBrowserCookieParam(row);
    expect(
      toContractCookie({ ...param, domain: param.domain ?? '', expires: param.expires ?? -1 }),
    ).toEqual(row);
  });

  it('selects the cookies a browser would send to an origin', () => {
    const jar = [
      row,
      { ...row, name: 'host', domain: 'www.example.org', host_only: true },
      { ...row, name: 'other-host', domain: 'shop.example.org', host_only: true },
      { ...row, name: 'plain', secure: false },
      { ...row, name: 'old', expires_at: '2026-10-09T12:00:00.000Z' },
      { ...row, name: 'session', expires_at: null },
      { ...row, name: 'foreign', domain: 'example.com' },
      { ...row, name: 'suffix-trap', domain: 'ample.org' },
    ];
    const names = (origin: string) =>
      cookiesForOrigin(jar, origin, now).map((cookie) => cookie.name);
    expect(names('https://www.example.org')).toEqual(['sid', 'host', 'plain', 'session']);
    expect(names('https://WWW.example.org:8443')).toEqual(['sid', 'host', 'plain', 'session']);
    expect(names('https://example.org')).toEqual(['sid', 'plain', 'session']);
    // A Secure cookie does not go to a plain-http origin.
    expect(names('http://www.example.org')).toEqual(['plain']);
    expect(names('https://example.com')).toEqual(['foreign']);
  });

  it('says why a cookie may not be imported for an origin', () => {
    expect(importRejection(row, 'https://shop.example.org', now)).toBeNull();
    expect(importRejection({ ...row, domain: 'example.com' }, 'https://example.org', now)).toBe(
      'foreign_domain',
    );
    expect(
      importRejection(
        { ...row, domain: 'a.example.org', host_only: true },
        'https://b.example.org',
        now,
      ),
    ).toBe('foreign_domain');
    expect(
      importRejection({ ...row, domain: 'shop.example.org' }, 'https://example.org', now),
    ).toBe('foreign_domain');
    expect(
      importRejection({ ...row, expires_at: '2020-01-01T00:00:00Z' }, 'https://example.org', now),
    ).toBe('expired');
    expect(importRejection({ ...row, expires_at: null }, 'https://example.org', now)).toBeNull();
  });

  it('gives two cookies one key exactly when they are the same cookie', () => {
    expect(cookieKey(row)).toBe(cookieKey({ ...row, domain: 'EXAMPLE.org' }));
    for (const change of [
      { host_only: true },
      { path: '/a' },
      { name: 'x' },
      { domain: 'example.com' },
    ]) {
      expect(cookieKey({ ...row, ...change })).not.toBe(cookieKey(row));
    }
  });
});
