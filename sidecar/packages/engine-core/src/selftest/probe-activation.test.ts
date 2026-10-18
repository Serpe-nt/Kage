import type { ExpectedIdentity, Finding } from '@kage/contract';
import { describe, expect, it } from 'vitest';

import {
  judgeProbe,
  parseProbeReport,
  verdict,
  type ProbeActivation,
  type ProbeReport,
  type ProbeScope,
} from '../index.js';

/**
 * User activation against the input a page received (spec §10.2, M5.1):
 * what both engines' drivers used to grant by reading a page, and what a
 * detector sees by the same comparison.
 */

const LINUX_UA =
  'Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/155.0.0.0 Safari/537.36';

const expected: ExpectedIdentity = {
  claimed_os: 'linux',
  browser_major: 155,
  timezone: 'Europe/Berlin',
  languages: ['de-DE', 'de'],
};

const scope: ProbeScope = {
  user_agent: LINUX_UA,
  platform: 'Linux x86_64',
  oscpu: null,
  app_version: null,
  language: null,
  intl_locale: null,
  fonts: null,
  screen: null,
  languages: ['de-DE', 'de'],
  hardware_concurrency: 8,
  device_memory: 16,
  timezone: 'Europe/Berlin',
  uad_platform: 'Linux',
  high_entropy: null,
  gpc: null,
  dnt: null,
  webgl: null,
  error: null,
};

type Sample = ProbeActivation['samples'][number];

const sample = (at: string, change: Partial<Sample> = {}): Sample => ({
  at,
  ms: at === 'start' ? 14 : 655,
  is_active: false,
  has_been_active: false,
  audio_state: 'suspended',
  ...change,
});

/** A page nobody touched, as an engine that reads without a gesture leaves it. */
const untouched = (change: Partial<ProbeActivation> = {}): ProbeActivation => ({
  samples: [sample('start'), sample('load+500ms')],
  reached_from_own_origin: false,
  trusted_input_events: 0,
  activating_input_events: 0,
  input_types: [],
  ...change,
});

const report = (activation: ProbeActivation | null | undefined): ProbeReport => ({
  kage_probe: 1,
  ...(activation === undefined ? {} : { activation }),
  scopes: {
    window: scope,
    iframe_same_origin: scope,
    iframe_srcdoc: scope,
    iframe_blank: scope,
    worker: scope,
    blob_worker: scope,
  },
  automation: {
    webdriver: false,
    console_stack_reads: 0,
    getters_native: true,
    illegal_invocation_throws: true,
    navigator_own_properties: [],
    driver_worker: 'throws',
    set_globals: [],
  },
  requests: {
    navigation: null,
    iframe: null,
    fetch: null,
    worker_script: null,
    worker_fetch: null,
  },
});

const judge = (activation: ProbeActivation | null | undefined) =>
  judgeProbe(report(activation), expected, 'probe', 1);
const find = (findings: readonly Finding[], check: string) =>
  findings.find((finding) => finding.check === check);

/** What the drivers did before M5.1: nothing at the start, active after the first read. */
const DRIVEN = [
  sample('start'),
  sample('load+500ms', { is_active: true, has_been_active: true, audio_state: 'running' }),
];

describe('user activation on the probe', () => {
  it('passes a page nobody touched', () => {
    const findings = judge(untouched());
    expect(find(findings, 'automation.user_activation')).toEqual({
      page: 'probe',
      check: 'automation.user_activation',
      status: 'pass',
      hard: true,
      weight: 1,
      observed:
        'start: is_active=false has_been_active=false | load+500ms: is_active=false has_been_active=false | trusted input events: 0, of them granting activation: 0',
      expected: 'no user activation before an input event that grants it',
    });
    expect(find(findings, 'automation.audio_autoplay')).toMatchObject({
      status: 'pass',
      hard: false,
      observed:
        'start: suspended | load+500ms: suspended | trusted input events: 0, of them granting activation: 0',
    });
    expect(verdict(findings, 90, true).passed).toBe(true);
  });

  it('fails hard when the page is active and no input granted it', () => {
    const findings = judge(untouched({ samples: DRIVEN }));
    expect(find(findings, 'automation.user_activation')).toMatchObject({
      status: 'fail',
      hard: true,
    });
    expect(find(findings, 'automation.user_activation')?.observed).toContain(
      'load+500ms: is_active=true has_been_active=true',
    );
    // One hard failure fails the self-test whatever the other fifty checks say.
    expect(verdict(findings, 90, true).passed).toBe(false);
    // The sound that started by itself is said too, as a warning.
    expect(find(findings, 'automation.audio_autoplay')).toMatchObject({
      status: 'warn',
      hard: false,
    });
  });

  it('takes a mouse that only moved for no reason to be active', () => {
    // Chrome hands a page a mousemove for a pointer that rests in the
    // window: an input event, and none that grants activation.
    const findings = judge(
      untouched({ samples: DRIVEN, trusted_input_events: 1, input_types: ['mousemove'] }),
    );
    expect(find(findings, 'automation.user_activation')).toMatchObject({
      status: 'fail',
      hard: true,
    });
    expect(find(findings, 'automation.user_activation')?.observed).toContain(
      'trusted input events: 1, of them granting activation: 0',
    );
  });

  it('fails for the sticky value alone, long after the transient one ran out', () => {
    const findings = judge(
      untouched({ samples: [sample('start'), sample('load+500ms', { has_been_active: true })] }),
    );
    expect(find(findings, 'automation.user_activation')?.status).toBe('fail');
    // Nothing plays: the sound check has nothing to say against it.
    expect(find(findings, 'automation.audio_autoplay')?.status).toBe('pass');
  });

  it('passes a page a person clicked on', () => {
    const findings = judge(
      untouched({
        samples: DRIVEN,
        trusted_input_events: 46,
        activating_input_events: 3,
        input_types: ['mousemove', 'pointerdown', 'mousedown', 'pointerup', 'mouseup', 'click'],
      }),
    );
    expect(find(findings, 'automation.user_activation')?.status).toBe('pass');
    expect(find(findings, 'automation.audio_autoplay')?.status).toBe('pass');
  });

  it('does not judge a page reached from its own origin, where Chrome carries activation over', () => {
    const findings = judge(
      untouched({
        samples: [
          sample('start', { has_been_active: true, audio_state: 'running' }),
          sample('load+500ms', { has_been_active: true, audio_state: 'running' }),
        ],
        reached_from_own_origin: true,
      }),
    );
    expect(find(findings, 'automation.user_activation')?.status).toBe('pass');
    expect(find(findings, 'automation.user_activation')?.observed).toContain(
      'reached from its own origin',
    );
    expect(find(findings, 'automation.audio_autoplay')?.status).toBe('pass');
  });

  it('warns, never fails, for a sound that runs by itself on an inactive page', () => {
    // An autoplay setting somebody chose.
    const findings = judge(
      untouched({
        samples: [sample('start'), sample('load+500ms', { audio_state: 'running' })],
      }),
    );
    expect(find(findings, 'automation.user_activation')?.status).toBe('pass');
    expect(find(findings, 'automation.audio_autoplay')).toMatchObject({
      status: 'warn',
      hard: false,
      expected: 'sound that waits for the user',
    });
    expect(verdict(findings, 90, true).passed).toBe(true);
  });

  it('warns when the browser says that sound may start by itself, also without a sound device', () => {
    // The Camoufox build as it ships: allowed, where a Firefox nobody
    // configured says allowed-muted and disallowed. No context ran: the
    // machine has no sound card.
    const asBuilt = judge(
      untouched({ autoplay_policy: { mediaelement: 'allowed', audiocontext: 'allowed' } }),
    );
    expect(find(asBuilt, 'automation.audio_autoplay')).toMatchObject({
      status: 'warn',
      hard: false,
      observed:
        'start: suspended | load+500ms: suspended | autoplay policy: mediaelement=allowed audiocontext=allowed | trusted input events: 0, of them granting activation: 0',
      expected: 'sound that waits for the user',
    });
    expect(find(asBuilt, 'automation.user_activation')?.status).toBe('pass');

    const stock = judge(
      untouched({
        autoplay_policy: { mediaelement: 'allowed-muted', audiocontext: 'disallowed' },
      }),
    );
    expect(find(stock, 'automation.audio_autoplay')?.status).toBe('pass');
    // Either answer alone is enough to say so.
    for (const autoplay_policy of [
      { mediaelement: 'allowed', audiocontext: 'disallowed' },
      { mediaelement: 'allowed-muted', audiocontext: 'allowed' },
    ]) {
      expect(find(judge(untouched({ autoplay_policy })), 'automation.audio_autoplay')?.status).toBe(
        'warn',
      );
    }
    // After a click a browser says allowed, and is right to.
    const clicked = judge(
      untouched({
        samples: DRIVEN,
        activating_input_events: 3,
        trusted_input_events: 40,
        autoplay_policy: { mediaelement: 'allowed', audiocontext: 'allowed' },
      }),
    );
    expect(find(clicked, 'automation.audio_autoplay')?.status).toBe('pass');
    // A browser that answers the call and has no AudioContext is judged by the call.
    const silent = judge(
      untouched({
        samples: [
          sample('start', { audio_state: null }),
          sample('load+500ms', { audio_state: null }),
        ],
        autoplay_policy: { mediaelement: 'allowed', audiocontext: null },
      }),
    );
    expect(find(silent, 'automation.audio_autoplay')).toMatchObject({
      status: 'warn',
      observed:
        'autoplay policy: mediaelement=allowed audiocontext=null | trusted input events: 0, of them granting activation: 0',
    });
  });

  it('says nothing about sound in a browser without an AudioContext', () => {
    const findings = judge(
      untouched({
        samples: [
          sample('start', { audio_state: null }),
          sample('load+500ms', { audio_state: null }),
        ],
      }),
    );
    expect(find(findings, 'automation.audio_autoplay')).toBeUndefined();
    expect(find(findings, 'automation.user_activation')?.status).toBe('pass');
  });

  it('warns when the browser has no navigator.userActivation to read', () => {
    const findings = judge(
      untouched({
        samples: [
          sample('start', { is_active: null, has_been_active: null }),
          sample('load+500ms', { is_active: null, has_been_active: null }),
        ],
      }),
    );
    expect(find(findings, 'automation.user_activation')).toMatchObject({
      status: 'warn',
      expected: 'a browser with navigator.userActivation',
    });
  });

  it('warns when the probe page is older than the check, so that nobody trusts a test that did not run', () => {
    for (const activation of [undefined, null, untouched({ samples: [] })]) {
      const findings = judge(activation);
      expect(find(findings, 'automation.user_activation')).toMatchObject({
        status: 'warn',
        hard: false,
        observed: 'the probe page reported no user activation: it is older than this check',
      });
      expect(find(findings, 'automation.audio_autoplay')).toBeUndefined();
      expect(verdict(findings, 90, true).passed).toBe(true);
    }
  });

  it('reads the activation block of a published report, and survives one that is not as expected', () => {
    const published = JSON.stringify(report(untouched({ samples: DRIVEN })));
    expect(parseProbeReport(published)?.activation?.samples).toHaveLength(2);
    // A value of another type in a sample becomes null and is judged as unreadable.
    const odd = JSON.parse(published) as { activation: { samples: { is_active: unknown }[] } };
    for (const entry of odd.activation.samples) {
      entry.is_active = 'yes';
    }
    const parsed = parseProbeReport(JSON.stringify(odd));
    expect(parsed?.activation?.samples[0]?.is_active).toBeNull();
    // A block that is no block at all is dropped, not the whole report.
    const broken = JSON.parse(published) as Record<string, unknown>;
    broken['activation'] = { samples: 'none' };
    expect(parseProbeReport(JSON.stringify(broken))).toMatchObject({
      kage_probe: 1,
      activation: null,
    });
  });
});
