import type { ClaimedOs, ExpectedIdentity, Finding } from '@kage/contract';
import { describe, expect, it } from 'vitest';

import {
  OS_FONTS,
  judgeProbe,
  osOfAppVersion,
  osOfOscpu,
  osOfWebglRenderer,
  parseProbeReport,
  type ProbeReport,
  type ProbeScope,
} from '../index.js';

/**
 * What the probe judge learned with the Firefox engine: values only
 * Firefox has, fonts and WebGL as a worker measures them, and the desktop
 * around the browser.
 */

const WINDOWS_UA =
  'Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:152.0) Gecko/20100101 Firefox/152.0';
const WINDOWS_FONTS = ['Segoe UI', 'Calibri', 'Cambria', 'Consolas', 'Tahoma', 'Arial', 'Verdana'];

const expected = (claimed_os: ClaimedOs = 'windows'): ExpectedIdentity => ({
  claimed_os,
  browser_major: 152,
  timezone: 'Europe/Berlin',
  languages: ['de-DE', 'de'],
});

const scope = (change: Partial<ProbeScope> = {}): ProbeScope => ({
  user_agent: WINDOWS_UA,
  platform: 'Win32',
  oscpu: 'Windows NT 10.0; Win64; x64',
  app_version: '5.0 (Windows)',
  language: 'de-DE',
  intl_locale: 'de',
  languages: ['de-DE', 'de'],
  hardware_concurrency: 8,
  device_memory: null,
  timezone: 'Europe/Berlin',
  uad_platform: null,
  high_entropy: null,
  webgl: {
    vendor: 'Google Inc. (NVIDIA)',
    renderer: 'ANGLE (NVIDIA, NVIDIA GeForce GTX 980 Direct3D11 vs_5_0 ps_5_0), or similar',
    plain_renderer: 'ANGLE (NVIDIA, NVIDIA GeForce GTX 980 Direct3D11 vs_5_0 ps_5_0), or similar',
    version: 'WebGL 1.0',
    shading_language: 'WebGL GLSL ES 1.0',
    max_texture_size: 16_384,
    extensions: 27,
  },
  fonts: WINDOWS_FONTS,
  screen: { width: 1920, height: 1080, avail_width: 1920, avail_height: 1040, color_depth: 24 },
  error: null,
  ...change,
});

/** A worker has no screen and none of the properties only a window's navigator has. */
const worker = (change: Partial<ProbeScope> = {}) =>
  scope({ oscpu: null, screen: null, ...change });

const report = (change: (report: ProbeReport) => void = () => undefined): ProbeReport => {
  const value: ProbeReport = {
    kage_probe: 1,
    environment: {
      window: {
        inner_width: 1920,
        inner_height: 986,
        outer_width: 1920,
        outer_height: 1037,
        avail_left: 0,
        avail_top: 0,
        device_pixel_ratio: 1,
      },
      media_devices: { audioinput: 1, audiooutput: 0, videoinput: 1 },
      audio: { sample_rate: 48_000, max_channel_count: 2 },
      voices: 0,
    },
    scopes: {
      window: scope(),
      iframe_same_origin: scope(),
      iframe_srcdoc: scope(),
      iframe_blank: scope(),
      worker: worker(),
      blob_worker: worker(),
    },
    automation: {
      webdriver: false,
      console_stack_reads: 0,
      getters_native: true,
      illegal_invocation_throws: true,
      navigator_own_properties: [],
    },
    requests: {
      navigation: null,
      iframe: null,
      fetch: null,
      worker_script: null,
      worker_fetch: null,
    },
  };
  change(value);
  return value;
};

const judge = (value: ProbeReport, claimed: ClaimedOs = 'windows') =>
  judgeProbe(value, expected(claimed), 'probe', 1);
const statusOf = (findings: readonly Finding[], check: string) =>
  findings.find((finding) => finding.check === check)?.status;
const notPassed = (findings: readonly Finding[]) =>
  findings.filter((finding) => finding.status !== 'pass').map((finding) => finding.check);

describe('a Firefox persona on the probe', () => {
  it('passes when every scope tells the claimed system, fonts and WebGL included', () => {
    const findings = judge(report());
    expect(notPassed(findings)).toEqual([]);
    for (const check of [
      'os.window',
      'os.worker',
      'coherence.fonts',
      'coherence.webgl',
      'coherence.screen',
      'coherence.app_version',
      'coherence.language',
      'coherence.intl_locale',
      'webgl.os',
      'fonts.foreign',
      'fonts.own',
      'environment.work_area',
      'environment.media_devices',
      'environment.audio',
    ]) {
      expect(statusOf(findings, check), check).toBe('pass');
    }
    // What a worker cannot have is not held against it.
    expect(findings.find((finding) => finding.check === 'coherence.screen')?.observed).toBe(
      '1920x1080x1920x1040x24',
    );
  });

  it('fails a window whose oscpu or appVersion names another system than its User-Agent', () => {
    const oscpu = judge(
      report((value) => (value.scopes.window = scope({ oscpu: 'Linux x86_64' }))),
    );
    expect(statusOf(oscpu, 'os.window')).toBe('fail');
    expect(oscpu.find((finding) => finding.check === 'os.window')?.hard).toBe(true);
    const appVersion = judge(
      report((value) => (value.scopes.iframe_blank = scope({ app_version: '5.0 (X11)' }))),
    );
    expect(statusOf(appVersion, 'os.iframe_blank')).toBe('fail');
    expect(statusOf(appVersion, 'coherence.app_version')).toBe('fail');
  });

  it('fails a worker that measures other fonts or reads another graphics card than the window', () => {
    const fonts = judge(
      report(
        (value) => (value.scopes.worker = worker({ fonts: ['DejaVu Sans', 'Liberation Sans'] })),
      ),
    );
    expect(statusOf(fonts, 'coherence.fonts')).toBe('fail');
    expect(fonts.find((finding) => finding.check === 'coherence.fonts')?.observed).toContain(
      'worker=DejaVu Sans,Liberation Sans',
    );
    const webgl = judge(
      report((value) => {
        const base = worker();
        value.scopes.blob_worker = worker({
          webgl: base.webgl === null ? null : { ...base.webgl, max_texture_size: 8192 },
        });
      }),
    );
    expect(statusOf(webgl, 'coherence.webgl')).toBe('fail');
  });

  it('fails fonts of another system, and a Windows or macOS claim without its own', () => {
    const every = (fonts: string[]) =>
      report((value) => {
        for (const name of Object.keys(value.scopes) as (keyof ProbeReport['scopes'])[]) {
          const current = value.scopes[name];
          if (current !== null) {
            value.scopes[name] = { ...current, fonts };
          }
        }
      });
    const foreign = judge(every([...WINDOWS_FONTS, 'DejaVu Sans']));
    expect(statusOf(foreign, 'fonts.foreign')).toBe('fail');
    expect(foreign.find((finding) => finding.check === 'fonts.foreign')?.observed).toBe(
      'DejaVu Sans',
    );
    expect(statusOf(foreign, 'fonts.own')).toBe('pass');
    const bare = judge(every(['Arial', 'Times New Roman']));
    expect(statusOf(bare, 'fonts.own')).toBe('fail');
    expect(statusOf(bare, 'fonts.foreign')).toBe('pass');
    // A Linux machine may have next to no fonts: that is a warning, not a contradiction.
    const linux = report((value) => {
      for (const name of Object.keys(value.scopes) as (keyof ProbeReport['scopes'])[]) {
        value.scopes[name] = scope({
          user_agent: 'Mozilla/5.0 (X11; Linux x86_64; rv:152.0) Gecko/20100101 Firefox/152.0',
          platform: 'Linux x86_64',
          oscpu: name.includes('worker') ? null : 'Linux x86_64',
          app_version: '5.0 (X11)',
          fonts: ['Times New Roman'],
          screen: name.includes('worker') ? null : scope().screen,
          webgl: null,
        });
      }
    });
    const tor = judge(linux, 'linux');
    expect(statusOf(tor, 'fonts.own')).toBe('warn');
    expect(statusOf(tor, 'webgl.available')).toBe('warn');
    expect(statusOf(tor, 'webgl.os')).toBeUndefined();
    expect(tor.filter((finding) => finding.status === 'fail')).toEqual([]);
    expect(OS_FONTS.linux).toContain('DejaVu Sans');
  });

  it("fails a renderer that can only be another system's", () => {
    const mesa = judge(
      report((value) => {
        for (const name of Object.keys(value.scopes) as (keyof ProbeReport['scopes'])[]) {
          const current = value.scopes[name];
          if (current?.webgl != null) {
            value.scopes[name] = {
              ...current,
              webgl: { ...current.webgl, renderer: 'llvmpipe (LLVM 19.1.7, 256 bits)' },
            };
          }
        }
      }),
    );
    expect(statusOf(mesa, 'webgl.os')).toBe('fail');
    expect(mesa.find((finding) => finding.check === 'webgl.os')?.hard).toBe(true);
    expect(statusOf(mesa, 'coherence.webgl')).toBe('pass');
  });

  it('warns about a browser with no desktop around it, and fails nothing for it', () => {
    const bare = judge(
      report((value) => {
        value.environment = {
          window: null,
          media_devices: { audioinput: 0, audiooutput: 0, videoinput: 0 },
          audio: { sample_rate: 44_100, max_channel_count: 0 },
          voices: 0,
        };
        for (const name of [
          'window',
          'iframe_same_origin',
          'iframe_srcdoc',
          'iframe_blank',
        ] as const) {
          value.scopes[name] = scope({
            screen: {
              width: 1920,
              height: 1080,
              avail_width: 1920,
              avail_height: 1080,
              color_depth: 24,
            },
          });
        }
      }),
    );
    expect(notPassed(bare).sort()).toEqual([
      'environment.audio',
      'environment.media_devices',
      'environment.work_area',
    ]);
    expect(bare.filter((finding) => finding.status === 'fail')).toEqual([]);
    expect(
      bare
        .filter((finding) => finding.check.startsWith('environment.'))
        .every((finding) => !finding.hard),
    ).toBe(true);
    const unreadable = judge(
      report((value) => {
        value.environment = { window: null, media_devices: null, audio: null, voices: null };
      }),
    );
    expect(
      unreadable.find((finding) => finding.check === 'environment.media_devices')?.observed,
    ).toBe('not readable');
    expect(unreadable.find((finding) => finding.check === 'environment.audio')?.observed).toBe(
      'no AudioContext',
    );
    // A probe page of before M5 says nothing about the environment: nothing is judged.
    const old = judge(report((value) => delete value.environment));
    expect(old.some((finding) => finding.check.startsWith('environment.'))).toBe(false);
  });

  it('reads the new values from JSON and treats what is missing as absent', () => {
    const parsed = parseProbeReport(JSON.stringify(report()));
    expect(parsed).toEqual(report());
    const stripped = JSON.parse(JSON.stringify(report())) as {
      environment?: unknown;
      scopes: { window: Record<string, unknown> };
    };
    delete stripped.environment;
    delete stripped.scopes.window['fonts'];
    stripped.scopes.window['screen'] = 'wide';
    const old = parseProbeReport(JSON.stringify(stripped));
    expect(old?.environment).toBeUndefined();
    expect(old?.scopes.window.fonts).toBeNull();
    expect(old?.scopes.window.screen).toBeNull();
  });
});

describe('identity marks of Firefox and of graphics cards', () => {
  it('reads the system out of oscpu, appVersion and a WebGL renderer', () => {
    expect(
      ['Windows NT 10.0; Win64; x64', 'Intel Mac OS X 10.15', 'Linux x86_64', 'OS/2'].map(
        osOfOscpu,
      ),
    ).toEqual(['windows', 'macos', 'linux', null]);
    expect(
      [
        '5.0 (Windows)',
        '5.0 (Macintosh)',
        '5.0 (X11)',
        '5.0 (X11; Linux x86_64) AppleWebKit/537.36',
        '5.0',
      ].map(osOfAppVersion),
    ).toEqual(['windows', 'macos', 'linux', 'linux', null]);
    expect(
      [
        'ANGLE (Intel, Intel(R) HD Graphics 400 Direct3D11 vs_5_0 ps_5_0), or similar',
        'Apple M1, or similar',
        'Intel(R) Iris(TM) Plus Graphics OpenGL Engine',
        'ANGLE (Mesa, llvmpipe (LLVM 19.1.7 256 bits), OpenGL 4.5)',
        'NVIDIA GeForce GTX 980, or similar',
      ].map(osOfWebglRenderer),
    ).toEqual(['windows', 'macos', 'macos', 'linux', null]);
  });
});
