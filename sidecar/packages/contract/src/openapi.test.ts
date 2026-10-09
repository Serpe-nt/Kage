import { readFileSync } from 'node:fs';

import { describe, expect, it } from 'vitest';
import { z } from 'zod';

import {
  ENDPOINTS,
  ERROR_STATUS,
  buildOpenApiDocument,
  findDialectViolations,
  serializeOpenApi,
  type Endpoint,
  type JsonObject,
  type OpenApiDocument,
} from './index.js';

const committed = readFileSync(new URL('../openapi.json', import.meta.url), 'utf8');
const document = buildOpenApiDocument();
const schemas = document.components['schemas'] as JsonObject;

const withSchema = (name: string, schema: JsonObject): OpenApiDocument => ({
  ...document,
  components: { ...document.components, schemas: { ...schemas, [name]: schema } },
});

describe('the OpenAPI document', () => {
  it('is the committed file, byte for byte', () => {
    // A difference means the schemas changed without `just openapi`.
    expect(serializeOpenApi(document)).toBe(committed);
  });

  it('is built the same way twice', () => {
    expect(serializeOpenApi(buildOpenApiDocument())).toBe(serializeOpenApi(document));
  });

  it('is OpenAPI 3.1 with the token as its security scheme', () => {
    expect(document.openapi).toBe('3.1.0');
    expect(document['security']).toEqual([{ sidecarToken: [] }]);
    expect((document.components['securitySchemes'] as JsonObject)['sidecarToken']).toMatchObject({
      type: 'apiKey',
      in: 'header',
      name: 'x-kage-sidecar-token',
    });
  });

  it('has one operation per endpoint with its body, its answer and its errors', () => {
    for (const endpoint of Object.values<Endpoint>(ENDPOINTS)) {
      const item = document.paths[endpoint.path] as JsonObject;
      const operation = item[endpoint.method.toLowerCase()] as JsonObject;
      expect(operation['operationId']).toBe(endpoint.operation);
      expect(operation['requestBody'] !== undefined).toBe(endpoint.request !== null);
      expect(operation['parameters'] !== undefined).toBe(endpoint.path.includes('{id}'));
      const responses = operation['responses'] as JsonObject;
      expect(responses[String(endpoint.response.status)]).toBeDefined();
      for (const code of [
        'unauthorized',
        'invalid_request',
        'internal',
        ...endpoint.errors,
      ] as const) {
        const response = responses[String(ERROR_STATUS[code])] as JsonObject;
        expect(response['description']).toContain(`\`${code}\``);
      }
    }
    expect(Object.keys(document.paths)).toHaveLength(10);
  });

  it('answers metrics as text, a flow as NDJSON and a close with nothing', () => {
    const responses = (path: string, method: string) =>
      ((document.paths[path] as JsonObject)[method] as JsonObject)['responses'] as JsonObject;
    expect(responses('/v1/metrics', 'get')['200']).toMatchObject({
      content: { 'text/plain': { schema: { type: 'string' } } },
    });
    expect(responses('/v1/contexts/{id}/flow', 'post')['200']).toMatchObject({
      content: { 'application/x-ndjson': { schema: { $ref: '#/components/schemas/FlowEvent' } } },
    });
    expect(
      (responses('/v1/contexts/{id}', 'delete')['204'] as JsonObject)['content'],
    ).toBeUndefined();
  });

  it('carries the examples and no dialect keywords of its source', () => {
    expect((schemas['PageResult'] as JsonObject)['examples']).toHaveLength(2);
    for (const schema of Object.values(schemas)) {
      expect(schema).not.toHaveProperty('$schema');
      expect(schema).not.toHaveProperty('$id');
    }
    expect(JSON.stringify(schemas)).not.toContain('"pattern":"^([0-9a-fA-F]{8}');
  });

  it('lists its schemas in a stable order', () => {
    const names = Object.keys(schemas);
    expect(names).toEqual([...names].sort());
    expect(names).toContain('PageResult');
    expect(names).toContain('FlowAction');
  });
});

describe('the dialect the Rust generator reads', () => {
  it('holds for the whole contract', () => {
    expect(findDialectViolations(document)).toEqual([]);
  });

  it('has one shape for what is sent and what is read: no default, no transform', () => {
    const emit = (io: 'input' | 'output') =>
      JSON.stringify(z.toJSONSchema(z.globalRegistry, { target: 'draft-2020-12', io }));
    expect(emit('input')).toBe(emit('output'));
  });

  it.each([
    ['a schema without a description', { type: 'string', enum: ['a'] }, 'Odd has no description'],
    [
      'a property without a description',
      {
        description: 'x',
        type: 'object',
        additionalProperties: false,
        properties: { a: { type: 'string' } },
      },
      'Odd.a has no description',
    ],
    [
      'an object that accepts unknown fields',
      { description: 'x', type: 'object', properties: {} },
      'Odd is not a strict object',
    ],
    [
      'a property that is not snake_case',
      {
        description: 'x',
        type: 'object',
        additionalProperties: false,
        properties: { camelCase: { type: 'string', description: 'y' } },
      },
      'Odd.camelCase is not snake_case',
    ],
    [
      'a default',
      {
        description: 'x',
        type: 'object',
        additionalProperties: false,
        properties: { a: { type: 'integer', description: 'y', default: 1 } },
      },
      'Odd.a has a default: apply defaults in code',
    ],
    [
      'an inline object',
      {
        description: 'x',
        type: 'object',
        additionalProperties: false,
        properties: { a: { type: 'object', description: 'y', properties: {} } },
      },
      'Odd.a is an inline object: give it an id so that it becomes a named schema',
    ],
    [
      'a union that is not "or null"',
      {
        description: 'x',
        type: 'object',
        additionalProperties: false,
        properties: { a: { description: 'y', anyOf: [{ type: 'string' }, { type: 'integer' }] } },
      },
      'Odd.a anyOf is only supported as "a schema or null"',
    ],
    [
      'a list of types',
      {
        description: 'x',
        type: 'object',
        additionalProperties: false,
        properties: { a: { description: 'y', type: ['string', 'integer'] } },
      },
      'Odd.a a type list is only supported as [type, "null"]',
    ],
    [
      'a reference outside the document',
      {
        description: 'x',
        type: 'object',
        additionalProperties: false,
        properties: { a: { description: 'y', $ref: 'https://example.org/x.json' } },
      },
      'Odd.a refers outside components.schemas',
    ],
    [
      'an array of something unsupported',
      {
        description: 'x',
        type: 'object',
        additionalProperties: false,
        properties: { a: { description: 'y', type: 'array', items: { type: 'null' } } },
      },
      'Odd.a has the unsupported type "null"',
    ],
    [
      'a shapeless object',
      {
        description: 'x',
        type: 'object',
        additionalProperties: false,
        properties: { a: { description: 'y', type: 'object' } },
      },
      'Odd.a is an object without a shape',
    ],
    [
      'a keyword without a type',
      {
        description: 'x',
        type: 'object',
        additionalProperties: false,
        properties: { a: { description: 'y', minimum: 1 } },
      },
      'Odd.a has no type',
    ],
    [
      'a property that is not a schema',
      { description: 'x', type: 'object', additionalProperties: false, properties: { a: 1 } },
      'Odd.a is not a schema object',
    ],
    [
      'a named primitive',
      { description: 'x', type: 'integer' },
      'Odd is neither an object, a string enum nor a oneOf',
    ],
    [
      'a union of inline options',
      { description: 'x', oneOf: [{ type: 'string' }] },
      'Odd: every oneOf option must be a $ref to a named object',
    ],
    [
      'a union without a discriminator',
      {
        description: 'x',
        oneOf: [{ $ref: '#/components/schemas/Screen' }, { $ref: '#/components/schemas/Header' }],
      },
      'Odd: the oneOf options share no property with a constant value',
    ],
  ])('names %s', (_label, schema, expected) => {
    expect(findDialectViolations(withSchema('Odd', schema as JsonObject))).toContain(expected);
  });

  it('accepts the forms the generator knows', () => {
    const fine: JsonObject = {
      description: 'x',
      type: 'object',
      additionalProperties: false,
      properties: {
        any_value: { description: 'y' },
        text_or_null: { description: 'y', type: ['string', 'null'] },
        list_of_refs: {
          description: 'y',
          type: 'array',
          items: { $ref: '#/components/schemas/Header' },
        },
        map: { description: 'y', type: 'object', additionalProperties: {} },
        ref_or_null: {
          description: 'y',
          anyOf: [{ $ref: '#/components/schemas/Header' }, { type: 'null' }],
        },
      },
    };
    expect(findDialectViolations(withSchema('Fine', fine))).toEqual([]);
  });

  it('reports a document without schemas and a schema that is not an object', () => {
    expect(findDialectViolations({ ...document, components: {} })).toEqual([
      'components.schemas is missing',
    ]);
    expect(findDialectViolations(withSchema('Odd', 1 as unknown as JsonObject))).toContain(
      'Odd is not a schema object',
    );
  });
});
