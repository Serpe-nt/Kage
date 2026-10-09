// Reads one global scope for the coherence probe: a window, a frame's
// window or a worker. The page, its dedicated worker (importScripts) and its
// blob worker (the function's source) all run this one function, so the
// scopes cannot differ because their readers do.
//
// It is self-contained on purpose: a blob worker gets nothing but its text.
self.kageReadScope = async (scope, makeCanvas) => {
  const out = {
    user_agent: null, platform: null, oscpu: null, app_version: null, build_id: null,
    language: null, languages: null, intl_locale: null, hardware_concurrency: null,
    device_memory: null, timezone: null, uad_platform: null, high_entropy: null,
    webgl: null, fonts: null, screen: null, error: null,
  };
  // Families whose presence says which system the fonts come from, and a few
  // every system has under one name or another. Detection is the usual one:
  // a family that exists changes the width of a line set in it.
  const FONTS = [
    'Segoe UI', 'Calibri', 'Cambria', 'Consolas', 'Tahoma', 'Microsoft Sans Serif', 'MS Gothic',
    'Franklin Gothic Medium', 'Gabriola', 'Sylfaen',
    'Helvetica Neue', 'Menlo', 'Monaco', 'Lucida Grande', 'Geneva', 'Avenir', 'Optima',
    'PingFang SC', 'Hiragino Sans', 'Apple SD Gothic Neo',
    'DejaVu Sans', 'DejaVu Sans Mono', 'Liberation Sans', 'Liberation Mono', 'Cantarell',
    'Noto Sans', 'Noto Mono', 'Ubuntu', 'Droid Sans Fallback', 'Nimbus Sans', 'FreeSans',
    'Arial', 'Times New Roman', 'Courier New', 'Verdana', 'Georgia', 'Helvetica',
    'Comic Sans MS', 'Impact', 'Trebuchet MS',
  ];
  try {
    const nav = scope.navigator;
    out.user_agent = nav.userAgent;
    out.platform = nav.platform;
    out.oscpu = nav.oscpu ?? null;
    out.app_version = nav.appVersion ?? null;
    out.build_id = nav.buildID ?? null;
    out.language = nav.language ?? null;
    out.languages = Array.from(nav.languages || []);
    out.hardware_concurrency = nav.hardwareConcurrency ?? null;
    out.device_memory = nav.deviceMemory ?? null;
    const resolved = new scope.Intl.DateTimeFormat().resolvedOptions();
    out.timezone = resolved.timeZone;
    out.intl_locale = resolved.locale;
    if (nav.userAgentData) {
      out.uad_platform = nav.userAgentData.platform;
      const high = await nav.userAgentData.getHighEntropyValues(
        ['platform', 'platformVersion', 'architecture', 'bitness', 'uaFullVersion'],
      );
      out.high_entropy = {
        platform: high.platform ?? null,
        platform_version: high.platformVersion ?? null,
        architecture: high.architecture ?? null,
        bitness: high.bitness ?? null,
        ua_full_version: high.uaFullVersion ?? null,
      };
    }
    if (scope.screen) {
      const s = scope.screen;
      out.screen = {
        width: s.width, height: s.height, avail_width: s.availWidth, avail_height: s.availHeight,
        color_depth: s.colorDepth, pixel_depth: s.pixelDepth,
      };
    }
    try {
      const gl = makeCanvas().getContext('webgl');
      if (gl) {
        const info = gl.getExtension('WEBGL_debug_renderer_info');
        out.webgl = {
          vendor: info ? gl.getParameter(info.UNMASKED_VENDOR_WEBGL) : null,
          renderer: info ? gl.getParameter(info.UNMASKED_RENDERER_WEBGL) : null,
          plain_vendor: gl.getParameter(gl.VENDOR),
          plain_renderer: gl.getParameter(gl.RENDERER),
          version: gl.getParameter(gl.VERSION),
          shading_language: gl.getParameter(gl.SHADING_LANGUAGE_VERSION),
          max_texture_size: gl.getParameter(gl.MAX_TEXTURE_SIZE),
          extensions: (gl.getSupportedExtensions() || []).length,
        };
      }
    } catch (error) {
      out.webgl = null;
    }
    try {
      const context = makeCanvas().getContext('2d');
      if (context) {
        const text = 'mmMwWLliI0O&1 The quick brown fox';
        const bases = ['monospace', 'sans-serif', 'serif'];
        const width = (font) => { context.font = font; return context.measureText(text).width; };
        const baseline = bases.map((base) => width(`72px ${base}`));
        out.fonts = FONTS.filter((family) =>
          bases.some((base, index) => width(`72px "${family}", ${base}`) !== baseline[index]),
        );
      }
    } catch (error) {
      out.fonts = null;
    }
  } catch (error) {
    out.error = String(error);
  }
  return out;
};
