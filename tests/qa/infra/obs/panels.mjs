// Run inside the stack's `world` container: node panels.mjs  (dashboards JSON on stdin as an array of {file, json})
// For every panel target: the expression with Grafana's variables replaced, the
// number of series an instant query returns, and their values.
let input = '';
process.stdin.on('data', (c) => (input += c)).on('end', async () => {
  const boards = JSON.parse(input);
  for (const { file, json } of boards) {
    for (const panel of json.panels ?? []) {
      for (const t of panel.targets ?? []) {
        if (!t.expr) continue;
        const expr = t.expr.replaceAll('$__rate_interval', '1m').replaceAll('$__interval', '15s').replaceAll('$__range', '1h');
        let out;
        try {
          const r = await fetch('http://prometheus:9090/api/v1/query?query=' + encodeURIComponent(expr));
          const j = await r.json();
          if (j.status !== 'success') out = `ERROR ${j.error}`;
          else out = `${j.data.result.length} series ` + j.data.result.slice(0, 4).map((s) => `${JSON.stringify(s.metric)}=${s.value[1]}`).join(' ');
        } catch (e) { out = `FETCH ${e}`; }
        console.log(`${file} | ${panel.title} | ${t.refId} | ${out}`);
      }
    }
  }
});
