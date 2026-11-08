// QA round three addendum (crawl and classifier): helpers on top of round
// one's /scripts/qa/crawl/h.mjs. S=<stack> in the environment.
export * from '/scripts/qa/crawl/h.mjs';
import * as h from '/scripts/qa/crawl/h.mjs';

// One fetch, summarised: outcome, status, reason, block, attempts.
export async function fetch1(req, token) {
  const r = await h.api('POST', '/v1/fetch', req, token ?? (await h.key()));
  const j = r.json ?? {};
  return {
    http: r.status,
    code: j.code,
    outcome: j.outcome,
    status: j.status,
    reason: j.reason,
    block: j.block ? `${j.block.vendor}/${j.block.rule_id ?? j.block.rule ?? ''}` : undefined,
    engine: j.persona?.engine,
    attempts: (j.attempts ?? []).map((a) => [a.engine, a.outcome, a.reason, a.status]),
    dom: j.dom_html ? j.dom_html.length : 0,
    raw: j.raw_body ? j.raw_body.bytes ?? j.raw_body.data?.length : 0,
    referer: j.behavior?.referer_strategy,
    full: j,
  };
}
export const brief1 = (x) => {
  const { full, ...rest } = x;
  return JSON.stringify(rest);
};
export function metric(name, filter = '') {
  return fetch(h.base + '/metrics', { headers: { authorization: `Bearer ${h.admin}` } })
    .then((r) => r.text())
    .then((t) => t.split('\n').filter((l) => l.startsWith(name) && l.includes(filter)));
}
