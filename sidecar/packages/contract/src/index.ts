/**
 * The sidecar HTTP contract (spec §10.2): zod schemas, the endpoint table,
 * the OpenAPI document built from both, and the breaking-change check.
 *
 * The schemas are the source of truth. `openapi.json` next to this package
 * is emitted from them and committed; `kage-browser` generates its Rust
 * types from that file.
 */
export * from './breaking.js';
export * from './cli.js';
export * from './common.js';
export * from './contexts.js';
export * from './endpoints.js';
export * from './examples.js';
export * from './flow.js';
export * from './health.js';
export * from './navigate.js';
export * from './openapi.js';
export * from './personas.js';
export * from './selftest.js';
export * from './state.js';
