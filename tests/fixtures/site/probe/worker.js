// The dedicated worker of the coherence probe: reads the worker scope with
// the same function as the page (scope.js), the headers of the request for
// this script (written by the server that hosts the probe; a static host
// leaves null) and the headers of a fetch() made from here.
importScripts('scope.js');
(async () => {
  const SCRIPT_HEADERS = /*KAGE_REQUEST_HEADERS*/null;
  const scope = await self.kageReadScope(self, () => new OffscreenCanvas(300, 150));
  let fetchHeaders = null;
  try {
    const response = await fetch('headers');
    fetchHeaders = response.ok ? (await response.json()).headers : null;
  } catch (error) {
    fetchHeaders = null;
  }
  postMessage({ scope, script_headers: SCRIPT_HEADERS, fetch_headers: fetchHeaders });
})();
