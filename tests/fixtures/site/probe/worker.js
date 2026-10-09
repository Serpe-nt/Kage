// The dedicated worker of the coherence probe: reads the worker scope, the
// headers of the request for this script (written by the server that hosts
// the probe; a static host leaves null) and the headers of a fetch() made
// from here.
(async () => {
  const SCRIPT_HEADERS = /*KAGE_REQUEST_HEADERS*/null;
  const scope = {
    user_agent: null, platform: null, languages: null, hardware_concurrency: null,
    device_memory: null, timezone: null, uad_platform: null, high_entropy: null,
    webgl: null, error: null,
  };
  try {
    const nav = self.navigator;
    scope.user_agent = nav.userAgent;
    scope.platform = nav.platform;
    scope.languages = Array.from(nav.languages || []);
    scope.hardware_concurrency = nav.hardwareConcurrency ?? null;
    scope.device_memory = nav.deviceMemory ?? null;
    scope.timezone = new Intl.DateTimeFormat().resolvedOptions().timeZone;
    if (nav.userAgentData) {
      scope.uad_platform = nav.userAgentData.platform;
      const high = await nav.userAgentData.getHighEntropyValues(
        ['platform', 'platformVersion', 'architecture', 'bitness', 'uaFullVersion'],
      );
      scope.high_entropy = {
        platform: high.platform ?? null,
        platform_version: high.platformVersion ?? null,
        architecture: high.architecture ?? null,
        bitness: high.bitness ?? null,
        ua_full_version: high.uaFullVersion ?? null,
      };
    }
    const gl = new OffscreenCanvas(8, 8).getContext('webgl');
    const info = gl && gl.getExtension('WEBGL_debug_renderer_info');
    if (gl && info) {
      scope.webgl = {
        vendor: gl.getParameter(info.UNMASKED_VENDOR_WEBGL),
        renderer: gl.getParameter(info.UNMASKED_RENDERER_WEBGL),
      };
    }
  } catch (error) {
    scope.error = String(error);
  }
  let fetchHeaders = null;
  try {
    const response = await fetch('headers');
    fetchHeaders = response.ok ? (await response.json()).headers : null;
  } catch (error) {
    fetchHeaders = null;
  }
  postMessage({ scope, script_headers: SCRIPT_HEADERS, fetch_headers: fetchHeaders });
})();
