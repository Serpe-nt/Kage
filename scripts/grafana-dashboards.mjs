#!/usr/bin/env node
// The Grafana dashboards of the `observability` profile (spec §20), written
// from the definitions below into deploy/grafana/dashboards/*.json, which
// Grafana reads at start (deploy/grafana/provisioning/dashboards). The JSON
// is generated so that every panel is built alike and checked alike:
//
//   node scripts/grafana-dashboards.mjs            write the files
//   node scripts/grafana-dashboards.mjs --check    exit 1 unless the files are
//                                                  current and every metric a
//                                                  panel or an alert rule names
//                                                  is one Kage or a sidecar
//                                                  defines
//
// Run by `just lint` (with --check).

import { readFileSync, readdirSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';

const args = process.argv.slice(2);
const root = resolve(args.find((arg) => !arg.startsWith('--')) ?? '.');
const check = args.includes('--check');
const out = join(root, 'deploy/grafana/dashboards');

const PROMETHEUS = { type: 'prometheus', uid: 'prometheus' };
const BLOCKS = 'rate_limited|forbidden|captcha|challenge_failed|soft_block';

/** A time series panel: one query per line drawn. */
const series = (title, unit, ...targets) => ({ kind: 'timeseries', title, unit, targets });
/** One number, coloured past its thresholds. */
const stat = (title, unit, expr, steps = []) => ({ kind: 'stat', title, unit, targets: [[expr]], steps });
/** A gauge from 0 to `max`, coloured past its thresholds. */
const gauge = (title, unit, expr, max, steps) => ({ kind: 'gauge', title, unit, targets: [[expr]], max, steps });

/** The quantile of a histogram of seconds over five minutes, by a label. */
const quantile = (q, metric, by) =>
  `histogram_quantile(${q}, sum by (le${by ? `, ${by}` : ''}) (rate(${metric}_bucket[5m])))`;

const DASHBOARDS = [
  {
    uid: 'kage-pool',
    title: 'Kage · pool and personas',
    description: 'The proxies, the personas behind them, their self-tests, and the sidecars that run their browsers.',
    panels: [
      series('Proxies by state', 'short', ['sum by (state) (kage_proxy_state)', '{{state}}']),
      series('Personas by state', 'short', ['sum by (state) (kage_persona_state)', '{{state}}']),
      series('Self-test score by persona', 'short', ['kage_selftest_score', '{{engine}} {{persona}}']),
      series('Replacements asked of the provider, per day', 'short', ['sum(increase(kage_replacements_total[1d]))', 'replacements']),
      series('Browser contexts open', 'short', ['sum by (engine) (sidecar_contexts_open)', '{{engine}}']),
      series('Context opens by result, per hour', 'short', ['sum by (engine, result) (increase(sidecar_context_opens_total[1h]))', '{{engine}} {{result}}']),
      series('Sidecar memory', 'bytes', ['max by (engine) (process_resident_memory_bytes{engine!=""})', '{{engine}}']),
      series('Sidecar CPU', 'percentunit', ['sum by (engine) (rate(process_cpu_seconds_total{engine!=""}[5m]))', '{{engine}}']),
      series('Browser recycles, per hour', 'short', ['sum by (engine) (increase(sidecar_browser_recycles_total[1h]))', '{{engine}}']),
      series('Navigation time p95', 's', [quantile(0.95, 'sidecar_navigation_duration_seconds', 'engine'), '{{engine}}']),
    ],
  },
  {
    uid: 'kage-visits',
    title: 'Kage · pacing and visits',
    description: 'What is asked of Kage, how long it waits for a persona and its turn, and how attempts end.',
    panels: [
      series('Fetches by outcome, per minute', 'short', ['sum by (outcome) (rate(kage_requests_total[5m])) * 60', '{{outcome}}']),
      series('Attempts by render, per minute', 'short', ['sum by (render) (rate(kage_attempts_total[5m])) * 60', '{{render}}']),
      series('Attempt time p50 and p95', 's', [quantile(0.5, 'kage_attempt_duration_seconds', 'render'), 'p50 {{render}}'], [quantile(0.95, 'kage_attempt_duration_seconds', 'render'), 'p95 {{render}}']),
      series('Wait for a persona and its turn p50 and p95', 's', [quantile(0.5, 'kage_scheduler_wait_seconds', 'kind'), 'p50 {{kind}}'], [quantile(0.95, 'kage_scheduler_wait_seconds', 'kind'), 'p95 {{kind}}']),
      series('Queue depth', 'short', ['sum by (kind) (kage_queue_depth)', '{{kind}}']),
      series('Attempts in flight', 'short', ['sum by (render) (kage_scheduler_in_flight)', '{{render}}']),
      series('Refused without an attempt, per hour', 'short', ['sum by (reason) (increase(kage_scheduler_refusals_total[1h]))', '{{reason}}']),
      series('API requests by status, per minute', 'short', ['sum by (status) (rate(kage_api_requests_total[5m])) * 60', '{{status}}']),
      series('API time p95 by route', 's', [quantile(0.95, 'kage_api_request_duration_seconds', 'route'), '{{route}}']),
      series('API refusals by code, per hour', 'short', ['sum by (code) (increase(kage_api_refusals_total[1h]))', '{{code}}']),
    ],
  },
  {
    uid: 'kage-blocks',
    title: 'Kage · blocks and breakers',
    description: 'How often each site turns a persona away, against what its policy allows before it pauses the site.',
    panels: [
      series('Blocks per hour by domain, against its breaker threshold', 'short', [`sum by (domain) (increase(kage_attempts_total{outcome=~"${BLOCKS}"}[1h]))`, 'blocks {{domain}}'], ['max by (domain) (kage_policy_breaker_threshold)', 'threshold {{domain}}']),
      series('Block rate by domain', 'percentunit', [`sum by (domain) (rate(kage_attempts_total{outcome=~"${BLOCKS}"}[1h])) / sum by (domain) (rate(kage_attempts_total[1h]))`, '{{domain}}']),
      series('Breakers open', 'short', ['max by (domain) (kage_breaker_open)', '{{domain}}']),
      series('Blocks by vendor, per hour', 'short', [`sum by (vendor) (increase(kage_attempts_total{outcome=~"${BLOCKS}"}[1h]))`, '{{vendor}}']),
      series('Blocks by outcome, per hour', 'short', [`sum by (outcome) (increase(kage_attempts_total{outcome=~"${BLOCKS}"}[1h]))`, '{{outcome}}']),
      stat('Domains paused now', 'short', 'sum(kage_breaker_open) or vector(0)', [[1, 'orange']]),
    ],
  },
  {
    uid: 'kage-jobs',
    title: 'Kage · jobs and crawls',
    description: 'Batches, flows and crawls: what waits, what is worked, how they end, and how a crawl reached its pages.',
    panels: [
      series('Jobs by type and state', 'short', ['sum by (job_type, state) (kage_jobs)', '{{job_type}} {{state}}']),
      series('Items by state', 'short', ['sum by (state) (kage_job_items)', '{{state}}']),
      series('Jobs accepted, per hour', 'short', ['sum by (job_type) (increase(kage_jobs_accepted_total[1h]))', '{{job_type}}']),
      series('Jobs finished by state, per hour', 'short', ['sum by (job_type, state) (increase(kage_jobs_finished_total[1h]))', '{{job_type}} {{state}}']),
      series('Items finished by state, per hour', 'short', ['sum by (job_type, state) (increase(kage_job_items_total[1h]))', '{{job_type}} {{state}}']),
      series('Crawl pages by how they were reached, per hour', 'short', ['sum by (reached) (increase(kage_crawl_pages_total[1h]))', '{{reached}}']),
      series('Crawl addresses skipped by reason, per hour', 'short', ['sum by (reason) (increase(kage_crawl_skipped_total[1h]))', '{{reason}}']),
      series('Crawl duplicates by kind, per hour', 'short', ['sum by (kind) (increase(kage_crawl_duplicates_total[1h]))', '{{kind}}']),
      series('Flows by result, per hour', 'short', ['sum by (engine, result) (increase(sidecar_flows_total[1h]))', '{{engine}} {{result}}']),
    ],
  },
  {
    uid: 'kage-webhooks',
    title: 'Kage · webhooks',
    description: "Deliveries of a job's events to its receiver: delivered, retried, failed for good, and what waits.",
    panels: [
      series('Deliveries by result, per hour', 'short', ['sum by (result) (increase(kage_webhook_deliveries_total[1h]))', '{{result}}']),
      series('Deliveries by event, per hour', 'short', ['sum by (event) (increase(kage_webhook_deliveries_total[1h]))', '{{event}}']),
      stat('Deliveries waiting', 'short', 'sum(kage_webhook_deliveries_pending) or vector(0)', [[100, 'orange']]),
      stat('Failed for good, last day', 'short', 'sum(increase(kage_webhook_deliveries_total{result="failed"}[1d])) or vector(0)', [[1, 'red']]),
    ],
  },
  {
    uid: 'kage-bandwidth',
    title: 'Kage · bandwidth',
    description: "The bytes through the forwarder against the proxy plan's monthly cap (bandwidth.cap, 250 GB by default).",
    panels: [
      gauge("Share of the plan's cap used", 'percentunit', 'max(kage_bandwidth_budget_ratio)', 1, [[0.8, 'orange'], [1, 'red']]),
      series("Share of the plan's cap used", 'percentunit', ['max(kage_bandwidth_budget_ratio)', 'used']),
      series('Bytes through the forwarder, per hour', 'bytes', ['sum by (direction) (increase(kage_bandwidth_bytes_total[1h]))', '{{direction}}']),
      series('Bytes per fetch', 'bytes', ['sum(increase(kage_bandwidth_bytes_total[1h])) / sum(increase(kage_requests_total[1h]))', 'per fetch']),
    ],
  },
  {
    uid: 'kage-cache-robots',
    title: 'Kage · cache and robots.txt',
    description: 'The response cache, the sites whose robots.txt is asked for, and what it kept from being fetched.',
    panels: [
      series('Cache by result, per hour', 'short', ['sum by (result) (increase(kage_cache_requests_total[1h]))', '{{result}}']),
      series('Cache hit share', 'percentunit', ['sum(rate(kage_cache_requests_total{result="hit"}[1h])) / sum(rate(kage_cache_requests_total{result=~"hit|miss"}[1h]))', 'hits']),
      series('robots.txt asked for, by result, per hour', 'short', ['sum by (result) (increase(kage_robots_requests_total[1h]))', '{{result}}']),
      series('Not fetched because of robots.txt, by reason, per hour', 'short', ['sum by (reason) (increase(kage_robots_denied_total[1h]))', '{{reason}}']),
      series('Reading a document p95', 's', [quantile(0.95, 'kage_extract_duration_seconds', ''), 'p95']),
    ],
  },
];

/** The Grafana JSON of one panel, at its place in a grid two panels wide. */
function panel(definition, index) {
  const common = {
    id: index + 1,
    type: definition.kind,
    title: definition.title,
    datasource: PROMETHEUS,
    gridPos: { h: 8, w: 12, x: (index % 2) * 12, y: Math.floor(index / 2) * 8 },
    targets: definition.targets.map(([expr, legendFormat], ref) => ({
      datasource: PROMETHEUS,
      refId: String.fromCharCode(65 + ref),
      expr,
      ...(legendFormat === undefined ? { instant: true } : { legendFormat }),
    })),
  };
  const steps = [{ color: 'green', value: null }, ...(definition.steps ?? []).map(([value, color]) => ({ color, value }))];
  const defaults = { unit: definition.unit, thresholds: { mode: 'absolute', steps } };
  if (definition.kind === 'gauge') {
    defaults.min = 0;
    defaults.max = definition.max;
  }
  if (definition.kind === 'timeseries') {
    defaults.custom = { drawStyle: 'line', lineWidth: 1, fillOpacity: 10, showPoints: 'never' };
  }
  return { ...common, fieldConfig: { defaults, overrides: [] }, options: {} };
}

/** The Grafana JSON of one dashboard. */
function dashboard(definition) {
  return {
    uid: definition.uid,
    title: definition.title,
    description: definition.description,
    tags: ['kage'],
    editable: false,
    schemaVersion: 41,
    version: 1,
    time: { from: 'now-6h', to: 'now' },
    refresh: '1m',
    timezone: 'utc',
    templating: { list: [] },
    annotations: { list: [] },
    panels: definition.panels.map(panel),
  };
}

const rendered = new Map(
  DASHBOARDS.map((definition) => [`${definition.uid}.json`, `${JSON.stringify(dashboard(definition), null, 2)}\n`]),
);

/** Every metric name a PromQL expression names (not functions, labels or keywords). */
function metricsOf(expr) {
  const names = new Set();
  const stripped = expr.replace(/\{[^}]*\}/g, '').replace(/"[^"]*"/g, '');
  for (const match of stripped.matchAll(/\b([a-z_][a-z0-9_]*)\b(?!\s*\()/g)) {
    const word = match[1];
    if (/^(sum|max|min|avg|by|on|or|and|unless|le|without|ignoring|group_left|group_right|vector|bool|offset)$/.test(word)) {
      continue;
    }
    if (/^(kage|sidecar|process|nodejs|up)(_|$)/.test(word)) {
      names.add(word.replace(/_bucket$/, ''));
    }
  }
  return names;
}

/** The metric names Kage and its sidecars define, read from their sources. */
function defined() {
  const names = new Set(['up']);
  const walk = (dir) => {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) {
        if (entry.name !== 'tests' && entry.name !== 'target') walk(path);
      } else if (entry.name.endsWith('.rs')) {
        for (const match of readFileSync(path, 'utf8').matchAll(/"(kage_[a-z_]+)"|# TYPE (kage_[a-z_]+)/g)) {
          names.add(match[1] ?? match[2]);
        }
      }
    }
  };
  walk(join(root, 'crates'));
  const sidecar = readFileSync(join(root, 'sidecar/packages/server/src/metrics.ts'), 'utf8');
  for (const match of sidecar.matchAll(/name: '(sidecar_[a-z_]+)'/g)) {
    names.add(match[1]);
  }
  // The default metrics of the sidecars' Prometheus client.
  for (const name of ['process_resident_memory_bytes', 'process_cpu_seconds_total']) {
    names.add(name);
  }
  return names;
}

if (!check) {
  for (const [file, text] of rendered) {
    writeFileSync(join(out, file), text);
  }
  console.log(`wrote ${rendered.size} dashboards to ${out}`);
  process.exit(0);
}

const problems = [];
const present = new Set(readdirSync(out).filter((file) => file.endsWith('.json')));
for (const [file, text] of rendered) {
  let committed = null;
  try {
    committed = readFileSync(join(out, file), 'utf8');
  } catch {
    // Reported below.
  }
  if (committed !== text) {
    problems.push(`${file} is not what the definitions make: run node scripts/grafana-dashboards.mjs`);
  }
  present.delete(file);
}
for (const file of present) {
  problems.push(`${file} is no dashboard of the definitions`);
}

const known = defined();
const used = new Map();
for (const definition of DASHBOARDS) {
  for (const item of definition.panels) {
    for (const [expr] of item.targets) {
      for (const name of metricsOf(expr)) used.set(name, `panel "${item.title}"`);
    }
  }
}
const alerts = readFileSync(join(root, 'deploy/prometheus/alerts.yml'), 'utf8');
for (const match of alerts.matchAll(/expr:\s*(?:>-\s*)?([\s\S]*?)\n\s*(?:for|labels):/g)) {
  for (const name of metricsOf(match[1])) used.set(name, 'an alert rule');
}
for (const match of alerts.matchAll(/runbook: (\S+)/g)) {
  try {
    readFileSync(join(root, match[1]));
  } catch {
    problems.push(`an alert rule names the runbook ${match[1]}, which does not exist`);
  }
}
for (const [name, where] of used) {
  if (!known.has(name)) {
    problems.push(`${where} names ${name}, which neither Kage nor a sidecar defines`);
  }
}

if (problems.length > 0) {
  for (const problem of problems) console.error(`grafana-dashboards: ${problem}`);
  process.exit(1);
}
console.log(
  `grafana-dashboards: ${rendered.size} dashboards current; ${used.size} metrics named, all defined`,
);
