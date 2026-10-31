import { describe, expect, it } from 'vitest';

import {
  buildOpenApiDocument,
  checkBreaking,
  findBreakingChanges,
  majorVersion,
  type JsonObject,
  type OpenApiDocument,
} from './index.js';

/** A small document with one operation that uses its schemas in both directions. */
function base(): OpenApiDocument {
  const ref = (name: string) => ({ $ref: `#/components/schemas/${name}` });
  return {
    openapi: '3.1.0',
    info: { title: 't', version: '1.2.0' },
    paths: {
      '/v1/things': {
        post: {
          operationId: 'make_thing',
          requestBody: { content: { 'application/json': { schema: ref('ThingRequest') } } },
          responses: {
            '200': { description: 'ok', content: { 'application/json': { schema: ref('Thing') } } },
            '400': {
              description: 'no',
              content: { 'application/json': { schema: ref('Problem') } },
            },
          },
        },
        get: {
          operationId: 'list_things',
          responses: {
            '200': { description: 'ok', content: { 'application/json': { schema: ref('Thing') } } },
            '204': { description: 'nothing' },
          },
        },
      },
    },
    components: {
      schemas: {
        Problem: { type: 'object', properties: { code: { type: 'string' } }, required: ['code'] },
        Color: { type: 'string', enum: ['red', 'green'] },
        Shape: {
          oneOf: [ref('Circle'), ref('Square')],
        },
        Circle: {
          type: 'object',
          properties: { kind: { type: 'string', const: 'circle' }, radius: { type: 'number' } },
          required: ['kind', 'radius'],
        },
        Square: {
          type: 'object',
          properties: { kind: { type: 'string', const: 'square' }, side: { type: 'number' } },
          required: ['kind', 'side'],
        },
        ThingRequest: {
          type: 'object',
          properties: {
            name: { type: 'string', minLength: 1, maxLength: 50, pattern: '^[a-z]+$' },
            color: ref('Color'),
            count: { type: 'integer', minimum: 0, maximum: 10 },
            note: { type: ['string', 'null'] },
            tags: { type: 'array', items: { type: 'string' }, minItems: 0, maxItems: 5 },
            shape: ref('Shape'),
            parent: { anyOf: [ref('ThingRequest'), { type: 'null' }] },
            free: { type: 'string' },
          },
          required: ['name', 'color'],
        },
        Thing: {
          type: 'object',
          properties: {
            id: { type: 'string', format: 'uuid' },
            color: ref('Color'),
            shape: ref('Shape'),
            note: { type: 'string' },
            status: { type: 'string', const: 'ok' },
            extra: { type: 'object', additionalProperties: {} },
            sizes: { type: 'array', items: { type: 'integer' } },
          },
          required: ['id', 'color', 'shape', 'note'],
        },
      },
    },
  };
}

type Edit = (schemas: Record<string, JsonObject>, document: OpenApiDocument) => void;

function changed(edit: Edit): string[] {
  const after = base();
  edit(after.components['schemas'] as Record<string, JsonObject>, after);
  return findBreakingChanges(base(), after).map((change) => `${change.where}: ${change.what}`);
}

function named(schemas: Record<string, JsonObject>, name: string): JsonObject {
  const schema = schemas[name];
  if (schema === undefined) {
    throw new Error(`the test document has no schema ${name}`);
  }
  return schema;
}

const properties = (schema: JsonObject | undefined) =>
  schema?.['properties'] as Record<string, JsonObject>;
const REQUEST = 'POST /v1/things request';
// GET is compared before POST, and a named schema is compared once.
const RESPONSE = 'GET /v1/things response 200';

describe('findBreakingChanges', () => {
  it('finds nothing between a document and itself', () => {
    expect(findBreakingChanges(base(), base())).toEqual([]);
    const real = buildOpenApiDocument();
    expect(findBreakingChanges(real, structuredClone(real))).toEqual([]);
  });

  it.each<[string, Edit, string[]]>([
    [
      'a removed operation',
      (_schemas, document) => {
        delete (document.paths['/v1/things'] as JsonObject)['get'];
      },
      ['GET /v1/things: the operation was removed'],
    ],
    [
      'a removed path',
      (_schemas, document) => {
        document.paths = {};
      },
      ['POST /v1/things: the operation was removed', 'GET /v1/things: the operation was removed'],
    ],
    [
      'a removed success status',
      (_schemas, document) => {
        delete (
          ((document.paths['/v1/things'] as JsonObject)['get'] as JsonObject)[
            'responses'
          ] as JsonObject
        )['204'];
      },
      ['GET /v1/things: no longer answers 204'],
    ],
    [
      'a request body that is gone',
      (_schemas, document) => {
        delete ((document.paths['/v1/things'] as JsonObject)['post'] as JsonObject)['requestBody'];
      },
      ['POST /v1/things: no longer takes a request body'],
    ],
    [
      'a new request body',
      (_schemas, document) => {
        ((document.paths['/v1/things'] as JsonObject)['get'] as JsonObject)['requestBody'] = {
          content: { 'application/json': { schema: { type: 'string' } } },
        };
      },
      ['GET /v1/things: now needs a request body'],
    ],
    [
      'a request property that is gone',
      (schemas) => {
        delete properties(schemas['ThingRequest'])['free'];
      },
      [`${REQUEST}.free: is no longer accepted`],
    ],
    [
      'a response property that is gone',
      (schemas) => {
        delete properties(schemas['Thing'])['sizes'];
      },
      [`${RESPONSE}.sizes: is no longer returned`],
    ],
    [
      'an optional request property that became required',
      (schemas) => {
        named(schemas, 'ThingRequest')['required'] = ['name', 'color', 'count'];
      },
      [`${REQUEST}.count: became required`],
    ],
    [
      'a new required request property',
      (schemas) => {
        properties(schemas['ThingRequest'])['must'] = { type: 'string' };
        named(schemas, 'ThingRequest')['required'] = ['name', 'color', 'must'];
      },
      [`${REQUEST}.must: is new and required`],
    ],
    [
      'a response property that may now be absent',
      (schemas) => {
        named(schemas, 'Thing')['required'] = ['id', 'color', 'shape'];
      },
      [`${RESPONSE}.note: may now be absent`],
    ],
    [
      'a changed type',
      (schemas) => {
        properties(schemas['ThingRequest'])['count'] = { type: 'string' };
      },
      [`${REQUEST}.count: type changed from integer to string`],
    ],
    [
      'a changed format',
      (schemas) => {
        properties(schemas['Thing'])['id'] = { type: 'string', format: 'date-time' };
      },
      [`${RESPONSE}.id: format changed from "uuid" to "date-time"`],
    ],
    [
      'an enum value removed from a request and added to a response',
      (schemas) => {
        named(schemas, 'Color')['enum'] = ['red', 'blue'];
      },
      [`${REQUEST}.color: no longer accepts "green"`, `${RESPONSE}.color: may now be "blue"`],
    ],
    [
      'a request value that is now limited',
      (schemas) => {
        properties(schemas['ThingRequest'])['free'] = { type: 'string', enum: ['a'] };
      },
      [`${REQUEST}.free: is now limited to a fixed set of values`],
    ],
    [
      'a response value that is no longer limited',
      (schemas) => {
        properties(schemas['Thing'])['status'] = { type: 'string' };
      },
      [`${RESPONSE}.status: is no longer limited to a fixed set of values`],
    ],
    [
      'a request field that no longer takes null',
      (schemas) => {
        properties(schemas['ThingRequest'])['note'] = { type: 'string' };
        properties(schemas['ThingRequest'])['parent'] = {
          $ref: '#/components/schemas/ThingRequest',
        };
      },
      [
        `${REQUEST}.note: null is no longer accepted`,
        `${REQUEST}.parent: null is no longer accepted`,
      ],
    ],
    [
      'a response field that may now be null',
      (schemas) => {
        properties(schemas['Thing'])['note'] = { type: ['string', 'null'] };
      },
      [`${RESPONSE}.note: may now be null`],
    ],
    [
      'tightened bounds and a changed pattern in a request',
      (schemas) => {
        properties(schemas['ThingRequest'])['name'] = {
          type: 'string',
          minLength: 2,
          maxLength: 40,
          pattern: '^[a-z0-9]+$',
        };
        properties(schemas['ThingRequest'])['count'] = { type: 'integer', minimum: 1, maximum: 9 };
        properties(schemas['ThingRequest'])['tags'] = {
          type: 'array',
          items: { type: 'string' },
          minItems: 1,
          maxItems: 4,
        };
        properties(schemas['ThingRequest'])['free'] = { type: 'string', maxLength: 10 };
      },
      [
        `${REQUEST}.name: minLength tightened from 1 to 2`,
        `${REQUEST}.name: maxLength tightened from 50 to 40`,
        `${REQUEST}.name: pattern changed`,
        `${REQUEST}.count: minimum tightened from 0 to 1`,
        `${REQUEST}.count: maximum tightened from 10 to 9`,
        `${REQUEST}.tags: minItems tightened from 0 to 1`,
        `${REQUEST}.tags: maxItems tightened from 5 to 4`,
        `${REQUEST}.free: maxLength tightened from null to 10`,
      ],
    ],
    [
      'changed array items',
      (schemas) => {
        properties(schemas['Thing'])['sizes'] = { type: 'array', items: { type: 'string' } };
      },
      [`${RESPONSE}.sizes[]: type changed from integer to string`],
    ],
    [
      'a union variant removed from a request and added to a response',
      (schemas) => {
        schemas['Triangle'] = {
          type: 'object',
          properties: { kind: { type: 'string', const: 'triangle' } },
          required: ['kind'],
        };
        named(schemas, 'Shape')['oneOf'] = [
          { $ref: '#/components/schemas/Circle' },
          { $ref: '#/components/schemas/Triangle' },
        ];
      },
      [
        `${REQUEST}.shape: variant "square" is no longer accepted`,
        `${RESPONSE}.shape: variant "triangle" may now be sent`,
      ],
    ],
    [
      'a change inside a union variant',
      (schemas) => {
        delete properties(schemas['Circle'])['radius'];
      },
      [
        `${REQUEST}.shape<circle>.radius: is no longer accepted`,
        `${RESPONSE}.shape<circle>.radius: is no longer returned`,
      ],
    ],
  ])('reports %s', (_label, edit, expected) => {
    const found = changed(edit);
    for (const line of expected) {
      expect(found).toContain(line);
    }
  });

  it.each<[string, Edit]>([
    [
      'a new optional request property',
      (schemas) => {
        properties(schemas['ThingRequest'])['more'] = { type: 'string' };
      },
    ],
    [
      'a new response property',
      (schemas) => {
        properties(schemas['Thing'])['more'] = { type: 'string' };
      },
    ],
    [
      'a request property that is no longer required',
      (schemas) => {
        named(schemas, 'ThingRequest')['required'] = ['name'];
      },
    ],
    [
      'a response property that became required',
      (schemas) => {
        named(schemas, 'Thing')['required'] = ['id', 'color', 'shape', 'note', 'status'];
      },
    ],
    [
      'loosened bounds',
      (schemas) => {
        properties(schemas['ThingRequest'])['count'] = { type: 'integer' };
        properties(schemas['ThingRequest'])['name'] = {
          type: 'string',
          minLength: 0,
          maxLength: 99,
        };
      },
    ],
    [
      'a request field that newly takes null',
      (schemas) => {
        properties(schemas['ThingRequest'])['free'] = { type: ['string', 'null'] };
      },
    ],
    [
      'a changed error body',
      (schemas) => {
        schemas['Problem'] = { type: 'string' };
      },
    ],
    [
      'a new operation',
      (_schemas, document) => {
        document.paths['/v1/other'] = { get: { operationId: 'other', responses: {} } };
      },
    ],
  ])('accepts %s', (_label, edit) => {
    expect(changed(edit)).toEqual([]);
  });

  it('takes the pace of a flow (1.2.0) as an addition, and its removal as a break', () => {
    const now = buildOpenApiDocument();
    // The document as it was at 1.1.0: no `pace` in the request, no `paced_ms` in a step.
    const before = structuredClone(now);
    const schemas = before.components['schemas'] as Record<string, JsonObject>;
    delete schemas['FlowPace'];
    delete properties(schemas['FlowBehavior'])['pace'];
    const step = named(schemas, 'FlowStepEvent');
    delete properties(step)['paced_ms'];
    step['required'] = (step['required'] as string[]).filter((name) => name !== 'paced_ms');
    before.info['version'] = '1.1.0';
    expect(JSON.stringify(now)).toContain('"paced_ms"');
    expect(
      properties(named(now.components['schemas'] as Record<string, JsonObject>, 'FlowPace')),
    ).toMatchObject({
      dwell_ms: { type: 'integer', minimum: 0, maximum: 120_000 },
      min_dwell_ms: { type: 'integer', minimum: 0, maximum: 45_000 },
    });
    expect(checkBreaking(before, now)).toEqual({ changes: [], majorBumped: false, ok: true });
    // The other way round it is what the check is for.
    expect(
      findBreakingChanges(now, before).map((change) => `${change.where}: ${change.what}`),
    ).toEqual([
      'POST /v1/contexts/{id}/flow request.behavior.pace: is no longer accepted',
      'POST /v1/contexts/{id}/flow response 200<step>.paced_ms: is no longer returned',
    ]);
  });

  it('takes what a crawl needs (1.3.0) as additions, and their removal as a break', () => {
    const now = buildOpenApiDocument();
    // The document as it was at 1.2.0: no patience, no click on the open
    // page, no links, and nothing of them in an answer.
    const before = structuredClone(now);
    const schemas = before.components['schemas'] as Record<string, JsonObject>;
    const without = (name: string, fields: string[]) => {
      const schema = named(schemas, name);
      schema['properties'] = Object.fromEntries(
        Object.entries(properties(schema)).filter(([field]) => !fields.includes(field)),
      );
      if (Array.isArray(schema['required'])) {
        schema['required'] = (schema['required'] as string[]).filter(
          (field) => !fields.includes(field),
        );
      }
    };
    without('CaptureOptions', ['links']);
    without('Entry', ['from_open_page', 'max_backs']);
    without('BehaviorOptions', ['patience_ms']);
    without('EntryReport', ['backs']);
    without('BehaviorReport', ['patience_ms', 'scroll_reached', 'scroll_ended_by']);
    without('PageResult', ['links', 'links_truncated']);
    delete schemas['PageLink'];
    delete schemas['ScrollEnd'];
    before.info['version'] = '1.2.0';
    expect(now.info['version']).toBe('1.7.0');
    const current = now.components['schemas'] as Record<string, JsonObject>;
    // Asked for with optional fields only, so that a caller of 1.2.0 is still served.
    for (const [name, fields] of [
      ['CaptureOptions', ['links']],
      ['Entry', ['from_open_page', 'max_backs']],
      ['BehaviorOptions', ['patience_ms']],
    ] as const) {
      const schema = named(current, name);
      for (const field of fields) {
        expect(properties(schema)[field], `${name}.${field}`).toBeDefined();
        expect(schema['required'] ?? [], `${name}.${field}`).not.toContain(field);
      }
    }
    expect(properties(named(current, 'BehaviorOptions'))['patience_ms']).toMatchObject({
      type: 'integer',
      minimum: 1000,
      maximum: 600_000,
    });
    expect(properties(named(current, 'Entry'))['max_backs']).toMatchObject({
      type: 'integer',
      minimum: 0,
      maximum: 10,
    });
    expect(named(current, 'ScrollEnd')['enum']).toEqual(['depth', 'patience', 'budget', 'page']);
    expect(properties(named(current, 'PageResult'))['links']).toMatchObject({
      anyOf: [{ type: 'array', maxItems: 5000 }, { type: 'null' }],
    });
    expect(checkBreaking(before, now)).toEqual({ changes: [], majorBumped: false, ok: true });
    // The other way round it is what the check is for.
    const lost = findBreakingChanges(now, before).map(
      (change) => `${change.where}: ${change.what}`,
    );
    const navigate = 'POST /v1/contexts/{id}/navigate';
    for (const line of [
      `${navigate} request.capture.links: is no longer accepted`,
      `${navigate} request.behavior.entry.from_open_page: is no longer accepted`,
      `${navigate} request.behavior.entry.max_backs: is no longer accepted`,
      `${navigate} request.behavior.patience_ms: is no longer accepted`,
      `${navigate} response 200.behavior.entry.backs: is no longer returned`,
      `${navigate} response 200.behavior.patience_ms: is no longer returned`,
      `${navigate} response 200.behavior.scroll_reached: is no longer returned`,
      `${navigate} response 200.behavior.scroll_ended_by: is no longer returned`,
      `${navigate} response 200.links: is no longer returned`,
      `${navigate} response 200.links_truncated: is no longer returned`,
    ]) {
      expect(lost, line).toContain(line);
    }
  });

  it('takes the move of a fingerprint to a new major (1.4.0) as an addition, and its removal as a break', () => {
    const now = buildOpenApiDocument();
    // The document as it was at 1.3.0: no upgrade endpoint, none of its schemas.
    const before = structuredClone(now);
    delete (before.paths as Record<string, unknown>)['/v1/personas/upgrade'];
    const schemas = before.components['schemas'] as Record<string, JsonObject>;
    delete schemas['UpgradePersonaRequest'];
    delete schemas['UpgradePersonaResponse'];
    before.info['version'] = '1.3.0';
    expect(checkBreaking(before, now)).toEqual({ changes: [], majorBumped: false, ok: true });
    expect(
      findBreakingChanges(now, before).map((change) => `${change.where}: ${change.what}`),
    ).toContain('POST /v1/personas/upgrade: the operation was removed');
    // A sidecar of 1.4.0 still answers everything a caller of 1.3.0 asks.
    const current = now.components['schemas'] as Record<string, JsonObject>;
    expect(named(current, 'UpgradePersonaResponse')['required']).toEqual(
      expect.arrayContaining(['fingerprint', 'summary', 'upgraded']),
    );
  });

  it('takes the still visit and the resting hand of a paced flow (1.5.0) as additions, and their removal as a break', () => {
    const now = buildOpenApiDocument();
    // The document as it was at 1.4.0: no mode in the behavior of a
    // navigation, no wander in the pace of a flow.
    const before = structuredClone(now);
    const schemas = before.components['schemas'] as Record<string, JsonObject>;
    delete properties(schemas['BehaviorOptions'])['mode'];
    delete schemas['BehaviorMode'];
    delete properties(schemas['FlowPace'])['wander'];
    before.info['version'] = '1.4.0';
    const current = now.components['schemas'] as Record<string, JsonObject>;
    // Asked for with an optional field, so that a caller of 1.4.0 is still served.
    const behavior = named(current, 'BehaviorOptions');
    expect(properties(behavior)['mode']).toMatchObject({
      $ref: '#/components/schemas/BehaviorMode',
    });
    expect(behavior['required'] ?? []).not.toContain('mode');
    expect(named(current, 'BehaviorMode')['enum']).toEqual(['human', 'still']);
    // A flow has no mode; its pace has an optional wander.
    expect(properties(named(current, 'FlowBehavior'))['mode']).toBeUndefined();
    const pace = named(current, 'FlowPace');
    expect(properties(pace)['wander']).toMatchObject({ type: 'boolean' });
    expect(pace['required'] ?? []).not.toContain('wander');
    expect(checkBreaking(before, now)).toEqual({ changes: [], majorBumped: false, ok: true });
    // The other way round it is what the check is for.
    expect(
      findBreakingChanges(now, before).map((change) => `${change.where}: ${change.what}`),
    ).toEqual([
      'POST /v1/contexts/{id}/navigate request.behavior.mode: is no longer accepted',
      'POST /v1/contexts/{id}/flow request.behavior.pace.wander: is no longer accepted',
    ]);
  });

  it('takes the attempts of a self-test page (1.6.0) as an addition, and their removal as a break', () => {
    const now = buildOpenApiDocument();
    // The document as it was at 1.5.0: a page report without `attempts`.
    const before = structuredClone(now);
    const schemas = before.components['schemas'] as Record<string, JsonObject>;
    delete properties(schemas['SelftestPageReport'])['attempts'];
    before.info['version'] = '1.5.0';
    const current = now.components['schemas'] as Record<string, JsonObject>;
    // Reported, not required: a reader of a 1.5.0 sidecar's answer takes an absent
    // count as one load.
    const page = named(current, 'SelftestPageReport');
    expect(properties(page)['attempts']).toMatchObject({ type: 'integer', minimum: 1, maximum: 2 });
    expect(page['required'] ?? []).not.toContain('attempts');
    expect(checkBreaking(before, now)).toEqual({ changes: [], majorBumped: false, ok: true });
    // The other way round Kage would lose a field it reads.
    expect(
      findBreakingChanges(now, before).map((change) => `${change.where}: ${change.what}`),
    ).toEqual([
      'POST /v1/contexts/{id}/selftest response 200.pages[].attempts: is no longer returned',
    ]);
  });

  it('takes the tab of a navigation (1.7.0) as an addition, and its removal as a break', () => {
    const now = buildOpenApiDocument();
    // The document as it was at 1.6.0: a navigation that names no tab.
    const before = structuredClone(now);
    const schemas = before.components['schemas'] as Record<string, JsonObject>;
    delete properties(schemas['NavigateRequest'])['tab'];
    before.info['version'] = '1.6.0';
    const current = now.components['schemas'] as Record<string, JsonObject>;
    // Asked for with an optional field only: a sidecar of 1.6.0 is asked for
    // nothing it does not know unless Kage names a tab, and a request that
    // names none starts in a fresh tab (F96).
    const request = named(current, 'NavigateRequest');
    expect(properties(request)['tab']).toMatchObject({
      type: 'string',
      minLength: 1,
      maxLength: 128,
    });
    expect(request['required'] ?? []).not.toContain('tab');
    expect(checkBreaking(before, now)).toEqual({ changes: [], majorBumped: false, ok: true });
    // The other way round a crawl could no longer name its tab.
    expect(
      findBreakingChanges(now, before).map((change) => `${change.where}: ${change.what}`),
    ).toEqual(['POST /v1/contexts/{id}/navigate request.tab: is no longer accepted']);
  });

  it('compares a schema used twice only once per direction', () => {
    const found = changed((schemas) => {
      delete properties(schemas['Thing'])['sizes'];
    });
    expect(found.filter((line) => line.endsWith('.sizes: is no longer returned'))).toHaveLength(1);
  });
});

describe('checkBreaking', () => {
  it('passes without changes and fails on a breaking one', () => {
    expect(checkBreaking(base(), base())).toEqual({ changes: [], majorBumped: false, ok: true });
    const broken = base();
    broken.paths = {};
    expect(checkBreaking(base(), broken)).toMatchObject({ majorBumped: false, ok: false });
  });

  it('lets a new major version carry breaking changes', () => {
    const next = base();
    next.paths = {};
    next.info['version'] = '2.0.0';
    const report = checkBreaking(base(), next);
    expect(report.ok).toBe(true);
    expect(report.majorBumped).toBe(true);
    expect(report.changes).toHaveLength(2);
  });

  it('does not accept a minor version as an excuse', () => {
    const next = base();
    next.paths = {};
    next.info['version'] = '1.9.0';
    expect(checkBreaking(base(), next).ok).toBe(false);
  });

  it('reads the major version and refuses a version that is none', () => {
    expect(majorVersion(base())).toBe(1);
    const odd = base();
    odd.info['version'] = 'latest';
    expect(() => majorVersion(odd)).toThrow(/not a semantic version/);
    delete odd.info['version'];
    expect(() => majorVersion(odd)).toThrow(/not a semantic version/);
  });
});
