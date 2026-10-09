import type { z } from 'zod';

import { CONTRACT_VERSION, ErrorResponseSchema, type ErrorCode } from './common.js';
import { OpenContextRequestSchema, OpenContextResponseSchema } from './contexts.js';
import { FlowEventSchema, FlowRequestSchema } from './flow.js';
import { HealthResponseSchema } from './health.js';
import { NavigateRequestSchema, PageResultSchema } from './navigate.js';
import { GeneratePersonaRequestSchema, GeneratePersonaResponseSchema } from './personas.js';
import { SelftestRequestSchema, SelftestResponseSchema } from './selftest.js';
import {
  ExportStateRequestSchema,
  ExportStateResponseSchema,
  ImportStateRequestSchema,
  ImportStateResponseSchema,
} from './state.js';

/** HTTP status of every error code. One code, one status. */
export const ERROR_STATUS: Readonly<Record<ErrorCode, number>> = {
  unauthorized: 401,
  invalid_request: 400,
  not_found: 404,
  context_not_found: 404,
  context_limit: 409,
  context_conflict: 409,
  context_busy: 409,
  cross_os_claim_disabled: 422,
  browser_major_mismatch: 409,
  browser_launch_failed: 502,
  not_implemented: 501,
  payload_too_large: 413,
  internal: 500,
};

/** Media types the contract uses. */
export type MediaType = 'application/json' | 'text/plain' | 'application/x-ndjson';

/** The success answer of an endpoint. */
export interface EndpointResponse {
  readonly status: 200 | 201 | 204;
  /** `null` for an empty body (204) and for the metrics text. */
  readonly schema: z.ZodType | null;
  readonly media: MediaType | null;
  readonly description: string;
}

/** One endpoint of the v1 contract (spec §10.2). */
export interface Endpoint {
  /** `operationId` in the OpenAPI document; the Rust client's method name. */
  readonly operation: string;
  readonly method: 'GET' | 'POST' | 'DELETE';
  /** Path in OpenAPI notation, with `{id}` for the context id. */
  readonly path: string;
  readonly summary: string;
  readonly request: z.ZodType | null;
  readonly response: EndpointResponse;
  /** Error codes the endpoint itself can answer, besides `unauthorized`, `invalid_request` and `internal`. */
  readonly errors: readonly ErrorCode[];
}

const prefix = `/${CONTRACT_VERSION}`;

/** Path of the health endpoint. */
export const HEALTH_PATH = `${prefix}/health`;
/** Path of the metrics endpoint. */
export const METRICS_PATH = `${prefix}/metrics`;

const json = (status: 200 | 201, schema: z.ZodType, description: string): EndpointResponse => ({
  status,
  schema,
  media: 'application/json',
  description,
});

/** Every endpoint, in the order of the table in spec §10.2. */
export const ENDPOINTS = {
  health: {
    operation: 'health',
    method: 'GET',
    path: HEALTH_PATH,
    summary: 'Engine, browser version, contexts in use, uptime.',
    request: null,
    response: json(200, HealthResponseSchema, 'The server is up.'),
    errors: [],
  },
  metrics: {
    operation: 'metrics',
    method: 'GET',
    path: METRICS_PATH,
    summary: 'Prometheus metrics in the text exposition format.',
    request: null,
    response: { status: 200, schema: null, media: 'text/plain', description: 'The metrics.' },
    errors: [],
  },
  generatePersona: {
    operation: 'generate_persona',
    method: 'POST',
    path: `${prefix}/personas/generate`,
    summary: 'Generate a persona fingerprint from a seed and constraints.',
    request: GeneratePersonaRequestSchema,
    response: json(200, GeneratePersonaResponseSchema, 'The fingerprint and its summary.'),
    errors: ['cross_os_claim_disabled', 'browser_major_mismatch', 'not_implemented'],
  },
  openContext: {
    operation: 'open_context',
    method: 'POST',
    path: `${prefix}/contexts`,
    summary: 'Open a persona context: its own browser, profile directory, display and proxy.',
    request: OpenContextRequestSchema,
    response: json(201, OpenContextResponseSchema, 'The context is open.'),
    errors: [
      'context_limit',
      'context_conflict',
      'cross_os_claim_disabled',
      'browser_launch_failed',
      'not_implemented',
    ],
  },
  closeContext: {
    operation: 'close_context',
    method: 'DELETE',
    path: `${prefix}/contexts/{id}`,
    summary: 'Close a context and flush its profile to disk.',
    request: null,
    response: { status: 204, schema: null, media: null, description: 'The context is closed.' },
    errors: ['context_not_found'],
  },
  navigate: {
    operation: 'navigate',
    method: 'POST',
    path: `${prefix}/contexts/{id}/navigate`,
    summary: 'Load a page, wait, and capture it.',
    request: NavigateRequestSchema,
    response: json(
      200,
      PageResultSchema,
      'The page result. A navigation that did not arrive is a 200 too: `error` says why.',
    ),
    errors: ['context_not_found', 'context_busy', 'not_implemented'],
  },
  flow: {
    operation: 'flow',
    method: 'POST',
    path: `${prefix}/contexts/{id}/flow`,
    summary: 'Run a scripted flow; answers with one NDJSON line per step and a final line.',
    request: FlowRequestSchema,
    response: {
      status: 200,
      schema: FlowEventSchema,
      media: 'application/x-ndjson',
      description: 'One `FlowEvent` per line.',
    },
    errors: ['context_not_found', 'context_busy', 'not_implemented'],
  },
  exportState: {
    operation: 'export_state',
    method: 'POST',
    path: `${prefix}/contexts/{id}/state/export`,
    summary: 'Cookies and storage of one origin.',
    request: ExportStateRequestSchema,
    response: json(200, ExportStateResponseSchema, 'The state of the origin.'),
    errors: ['context_not_found', 'context_busy', 'not_implemented'],
  },
  importState: {
    operation: 'import_state',
    method: 'POST',
    path: `${prefix}/contexts/{id}/state/import`,
    summary: 'Put cookies of one origin into the context.',
    request: ImportStateRequestSchema,
    response: json(200, ImportStateResponseSchema, 'What was imported and what was refused.'),
    errors: ['context_not_found', 'context_busy', 'not_implemented'],
  },
  selftest: {
    operation: 'selftest',
    method: 'POST',
    path: `${prefix}/contexts/{id}/selftest`,
    summary: 'Load the detection page set and return findings and a score.',
    request: SelftestRequestSchema,
    response: json(200, SelftestResponseSchema, 'Findings and score.'),
    errors: ['context_not_found', 'context_busy', 'not_implemented'],
  },
} as const satisfies Record<string, Endpoint>;

/** Name of an endpoint in {@link ENDPOINTS}. */
export type EndpointName = keyof typeof ENDPOINTS;

/** Error codes any endpoint can answer. */
export const COMMON_ERRORS: readonly ErrorCode[] = ['unauthorized', 'invalid_request', 'internal'];

/** The path in the notation fastify routes use: `:id` instead of `{id}`. */
export function routePath(endpoint: Endpoint): string {
  return endpoint.path.replaceAll(/\{(\w+)\}/g, ':$1');
}

/** The path with the context id filled in. */
export function contextPath(endpoint: Endpoint, contextId: string): string {
  return endpoint.path.replace('{id}', encodeURIComponent(contextId));
}

/** The schema of every error body. */
export const ERROR_SCHEMA = ErrorResponseSchema;
