import { z } from 'zod';

import { CONTRACT_SEMVER, TOKEN_HEADER } from './common.js';
import {
  COMMON_ERRORS,
  ENDPOINTS,
  ERROR_SCHEMA,
  ERROR_STATUS,
  type Endpoint,
} from './endpoints.js';
import { EXAMPLES } from './examples.js';

/** Any JSON value. */
export type Json = null | boolean | number | string | Json[] | JsonObject;
/** A JSON object. */
export interface JsonObject {
  [key: string]: Json;
}

/** The OpenAPI document of the sidecar contract. */
export interface OpenApiDocument extends JsonObject {
  openapi: string;
  info: JsonObject;
  paths: JsonObject;
  components: JsonObject;
}

const SCHEMA_PREFIX = '#/components/schemas/';

/** The registered name of a contract schema. */
export function schemaId(schema: z.ZodType): string {
  const id = z.globalRegistry.get(schema)?.id;
  if (id === undefined) {
    throw new Error('a schema used by an endpoint has no id: give it one with .meta({ id })');
  }
  return id;
}

const ref = (schema: z.ZodType): JsonObject => ({ $ref: `${SCHEMA_PREFIX}${schemaId(schema)}` });

function componentSchemas(): JsonObject {
  const { schemas } = z.toJSONSchema(z.globalRegistry, {
    target: 'draft-2020-12',
    uri: (id) => `${SCHEMA_PREFIX}${id}`,
    override: (context) => {
      // The format names the type; zod's own validation patterns for these
      // two are long and add nothing a reader of the document needs.
      const { jsonSchema } = context;
      if (jsonSchema.format === 'uuid' || jsonSchema.format === 'date-time') {
        delete jsonSchema.pattern;
      }
    },
  });
  const sorted: JsonObject = {};
  for (const name of Object.keys(schemas).sort()) {
    // What says "this is a JSON Schema document" belongs to a document, not
    // to a component of one.
    const schema = { ...(schemas[name] as JsonObject) };
    delete schema['$schema'];
    delete schema['$id'];
    const examples = EXAMPLES[name];
    sorted[name] = examples === undefined ? schema : { ...schema, examples: examples as Json[] };
  }
  return sorted;
}

function errorResponses(endpoint: Endpoint): JsonObject {
  const byStatus = new Map<number, string[]>();
  for (const code of [...COMMON_ERRORS, ...endpoint.errors]) {
    const status = ERROR_STATUS[code];
    byStatus.set(status, [...(byStatus.get(status) ?? []), code]);
  }
  const responses: JsonObject = {};
  for (const status of [...byStatus.keys()].sort((a, b) => a - b)) {
    const codes = byStatus.get(status) ?? [];
    responses[String(status)] = {
      description: `Error codes: ${codes.map((code) => `\`${code}\``).join(', ')}.`,
      content: { 'application/json': { schema: ref(ERROR_SCHEMA) } },
    };
  }
  return responses;
}

function operation(endpoint: Endpoint): JsonObject {
  const { response } = endpoint;
  const success: JsonObject = { description: response.description };
  if (response.media !== null) {
    success['content'] = {
      [response.media]: {
        schema: response.schema === null ? { type: 'string' } : ref(response.schema),
      },
    };
  }
  const result: JsonObject = { operationId: endpoint.operation, summary: endpoint.summary };
  if (endpoint.path.includes('{id}')) {
    result['parameters'] = [
      {
        name: 'id',
        in: 'path',
        required: true,
        description: 'The context id returned by `POST /v1/contexts`.',
        schema: { type: 'string', format: 'uuid' },
      },
    ];
  }
  if (endpoint.request !== null) {
    result['requestBody'] = {
      required: true,
      content: { 'application/json': { schema: ref(endpoint.request) } },
    };
  }
  result['responses'] = { [String(response.status)]: success, ...errorResponses(endpoint) };
  return result;
}

/** Builds the OpenAPI 3.1 document from the zod schemas and the endpoint table. */
export function buildOpenApiDocument(): OpenApiDocument {
  const paths: JsonObject = {};
  for (const endpoint of Object.values<Endpoint>(ENDPOINTS)) {
    const item = (paths[endpoint.path] ?? {}) as JsonObject;
    item[endpoint.method.toLowerCase()] = operation(endpoint);
    paths[endpoint.path] = item;
  }
  return {
    openapi: '3.1.0',
    info: {
      title: 'Kage browser sidecar',
      version: CONTRACT_SEMVER,
      description:
        'Internal contract between Kage and its browser sidecars (spec §10.2). Generated from the zod schemas in sidecar/packages/contract; do not edit by hand. Regenerate with `just openapi`.',
      license: { name: 'MIT OR Apache-2.0', identifier: 'MIT OR Apache-2.0' },
    },
    security: [{ sidecarToken: [] }],
    paths,
    components: {
      securitySchemes: {
        sidecarToken: {
          type: 'apiKey',
          in: 'header',
          name: TOKEN_HEADER,
          description: 'The shared secret of Kage and its sidecars.',
        },
      },
      schemas: componentSchemas(),
    },
  };
}

/** The document as it is committed: two-space JSON with a final newline. */
export function serializeOpenApi(document: OpenApiDocument): string {
  return `${JSON.stringify(document, null, 2)}\n`;
}

const isObject = (value: Json | undefined): value is JsonObject =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

const PRIMITIVES = new Set(['string', 'integer', 'number', 'boolean']);

/**
 * Whether a schema below a named one is of a form the Rust type generator
 * (`crates/kage-browser/build.rs`) understands. Returns what is wrong, or
 * `null`.
 */
function valueProblem(schema: Json | undefined): string | null {
  if (!isObject(schema)) {
    return 'is not a schema object';
  }
  if (typeof schema['$ref'] === 'string') {
    return schema['$ref'].startsWith(SCHEMA_PREFIX) ? null : 'refers outside components.schemas';
  }
  if (Array.isArray(schema['anyOf'])) {
    const options = schema['anyOf'];
    const nulls = options.filter((option) => isObject(option) && option['type'] === 'null');
    const others = options.filter((option) => !(isObject(option) && option['type'] === 'null'));
    if (options.length !== 2 || nulls.length !== 1 || others.length !== 1) {
      return 'anyOf is only supported as "a schema or null"';
    }
    return valueProblem(others[0]);
  }
  let type = schema['type'];
  if (Array.isArray(type)) {
    const named = type.filter((entry) => entry !== 'null');
    if (type.length !== 2 || named.length !== 1) {
      return 'a type list is only supported as [type, "null"]';
    }
    type = named[0];
  }
  if (type === undefined) {
    // `{}`: any JSON value.
    return Object.keys(schema).every((key) => key === 'description') ? null : 'has no type';
  }
  if (typeof type === 'string' && PRIMITIVES.has(type)) {
    return null;
  }
  if (type === 'array') {
    return valueProblem(schema['items']);
  }
  if (type === 'object') {
    if (schema['properties'] !== undefined) {
      return 'is an inline object: give it an id so that it becomes a named schema';
    }
    return schema['additionalProperties'] === undefined ? 'is an object without a shape' : null;
  }
  return `has the unsupported type ${JSON.stringify(type)}`;
}

function discriminator(variants: JsonObject[]): string | null {
  const [first] = variants;
  if (first === undefined || !isObject(first['properties'])) {
    return null;
  }
  for (const name of Object.keys(first['properties'])) {
    const everywhere = variants.every((variant) => {
      const properties = variant['properties'];
      const property = isObject(properties) ? properties[name] : undefined;
      return isObject(property) && typeof property['const'] === 'string';
    });
    if (everywhere) {
      return name;
    }
  }
  return null;
}

/**
 * Checks that the document stays inside the dialect the Rust generator
 * reads, and that everything carries a description. An empty list means the
 * document is fine.
 */
export function findDialectViolations(document: OpenApiDocument): string[] {
  const problems: string[] = [];
  const schemas = document.components['schemas'];
  if (!isObject(schemas)) {
    return ['components.schemas is missing'];
  }
  for (const [name, schema] of Object.entries(schemas)) {
    if (!isObject(schema)) {
      problems.push(`${name} is not a schema object`);
      continue;
    }
    if (typeof schema['description'] !== 'string' || schema['description'] === '') {
      problems.push(`${name} has no description`);
    }
    if (Array.isArray(schema['oneOf'])) {
      const variants = schema['oneOf'].map((option) => {
        const target = isObject(option) && typeof option['$ref'] === 'string' ? option['$ref'] : '';
        return schemas[target.slice(SCHEMA_PREFIX.length)];
      });
      if (!variants.every(isObject)) {
        problems.push(`${name}: every oneOf option must be a $ref to a named object`);
      } else if (discriminator(variants) === null) {
        problems.push(`${name}: the oneOf options share no property with a constant value`);
      }
      continue;
    }
    if (schema['type'] === 'string' && Array.isArray(schema['enum'])) {
      continue;
    }
    if (schema['type'] !== 'object' || !isObject(schema['properties'])) {
      problems.push(`${name} is neither an object, a string enum nor a oneOf`);
      continue;
    }
    if (schema['additionalProperties'] !== false) {
      problems.push(`${name} is not a strict object`);
    }
    for (const [property, value] of Object.entries(schema['properties'])) {
      if (!/^[a-z][a-z0-9_]*$/.test(property)) {
        problems.push(`${name}.${property} is not snake_case`);
      }
      if (
        !isObject(value) ||
        typeof value['description'] !== 'string' ||
        value['description'] === ''
      ) {
        problems.push(`${name}.${property} has no description`);
      }
      if (isObject(value) && value['default'] !== undefined) {
        problems.push(`${name}.${property} has a default: apply defaults in code`);
      }
      const problem = valueProblem(value);
      if (problem !== null) {
        problems.push(`${name}.${property} ${problem}`);
      }
    }
  }
  return problems;
}
