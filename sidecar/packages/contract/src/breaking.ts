import type { Json, JsonObject, OpenApiDocument } from './openapi.js';

/**
 * Breaking-change check for the committed OpenAPI document.
 *
 * Kage and its sidecars are deployed together, but not at the same instant:
 * during an upgrade an old Kage talks to a new sidecar or the reverse. A
 * change is breaking when a client built from the old document can send
 * something the new server refuses, or receive something it cannot read.
 * What that means depends on the direction a schema travels in:
 *
 * | Change | request (Kage sends) | response (Kage reads) |
 * |---|---|---|
 * | operation, status or property removed | breaking | breaking |
 * | type or format changed | breaking | breaking |
 * | property became required, or a new required one | breaking | fine |
 * | property no longer required | fine | breaking |
 * | enum value or union variant removed | breaking | fine |
 * | enum value or union variant added | fine | breaking |
 * | null no longer accepted | breaking | fine |
 * | null newly possible | fine | breaking |
 * | a bound tightened, a pattern changed | breaking | fine |
 *
 * A breaking change is allowed only together with a new major version of
 * the document (`info.version`), which goes with a new path prefix.
 */

/** One incompatible difference between two versions of the document. */
export interface BreakingChange {
  /** Where: an operation, then the path into its schema. */
  readonly where: string;
  /** What changed, as a sentence. */
  readonly what: string;
}

type Direction = 'request' | 'response';

const isObject = (value: Json | undefined): value is JsonObject =>
  typeof value === 'object' && value !== null && !Array.isArray(value);

const METHODS = ['get', 'put', 'post', 'delete', 'patch'] as const;
const SCHEMA_PREFIX = '#/components/schemas/';

interface Side {
  readonly document: OpenApiDocument;
}

interface Resolved {
  readonly schema: JsonObject;
  /** The name of the component, when the schema is one. */
  readonly name: string | null;
  readonly nullable: boolean;
}

function resolve(side: Side, schema: Json | undefined): Resolved {
  let current: Json | undefined = schema;
  let name: string | null = null;
  let nullable = false;
  for (let depth = 0; depth < 32; depth += 1) {
    if (!isObject(current)) {
      break;
    }
    const reference = current['$ref'];
    if (typeof reference === 'string' && reference.startsWith(SCHEMA_PREFIX)) {
      name = reference.slice(SCHEMA_PREFIX.length);
      const schemas = side.document.components['schemas'];
      current = isObject(schemas) ? schemas[name] : undefined;
      continue;
    }
    if (Array.isArray(current['anyOf'])) {
      const options = current['anyOf'];
      const others = options.filter((option) => !(isObject(option) && option['type'] === 'null'));
      if (others.length === 1 && options.length === 2) {
        nullable = true;
        current = others[0];
        continue;
      }
    }
    if (Array.isArray(current['type'])) {
      const named = current['type'].filter((entry) => entry !== 'null');
      if (named.length !== current['type'].length) {
        nullable = true;
      }
      return { schema: { ...current, type: named[0] ?? 'null' }, name, nullable };
    }
    return { schema: current, name, nullable };
  }
  return { schema: {}, name, nullable };
}

function kindOf(schema: JsonObject): string {
  if (Array.isArray(schema['oneOf'])) {
    return 'oneOf';
  }
  const type = schema['type'];
  return typeof type === 'string' ? type : 'any';
}

function stringSet(value: Json | undefined): Set<string> {
  return new Set(Array.isArray(value) ? value.filter((entry) => typeof entry === 'string') : []);
}

function allowedValues(schema: JsonObject): Set<string> | null {
  if (typeof schema['const'] === 'string') {
    return new Set([schema['const']]);
  }
  return Array.isArray(schema['enum']) ? stringSet(schema['enum']) : null;
}

/** Bounds where a larger value is stricter, and bounds where a smaller one is. */
const LOWER_BOUNDS = ['minimum', 'minLength', 'minItems'] as const;
const UPPER_BOUNDS = ['maximum', 'maxLength', 'maxItems'] as const;

class Comparison {
  readonly changes: BreakingChange[] = [];
  private readonly seen = new Set<string>();

  constructor(
    private readonly before: Side,
    private readonly after: Side,
  ) {}

  private report(where: string, what: string): void {
    this.changes.push({ where, what });
  }

  private variants(side: Side, schema: JsonObject): Map<string, Json> {
    const byTag = new Map<string, Json>();
    const options = Array.isArray(schema['oneOf']) ? schema['oneOf'] : [];
    for (const option of options) {
      const { schema: variant, name } = resolve(side, option);
      const properties = isObject(variant['properties']) ? variant['properties'] : {};
      const tag = Object.values(properties)
        .map((property) => (isObject(property) ? property['const'] : undefined))
        .find((value) => typeof value === 'string');
      byTag.set(typeof tag === 'string' ? tag : (name ?? JSON.stringify(option)), option);
    }
    return byTag;
  }

  compare(
    where: string,
    direction: Direction,
    before: Json | undefined,
    after: Json | undefined,
  ): void {
    const old = resolve(this.before, before);
    const now = resolve(this.after, after);
    // Whether null is allowed belongs to the place a schema is used in.
    if (direction === 'request' && old.nullable && !now.nullable) {
      this.report(where, 'null is no longer accepted');
    }
    if (direction === 'response' && !old.nullable && now.nullable) {
      this.report(where, 'may now be null');
    }
    if (old.name !== null) {
      // A named schema is compared once per direction, however often it is used.
      const key = `${direction}:${old.name}:${now.name ?? ''}`;
      if (this.seen.has(key)) {
        return;
      }
      this.seen.add(key);
    }
    const kind = kindOf(old.schema);
    if (kind !== kindOf(now.schema)) {
      this.report(where, `type changed from ${kind} to ${kindOf(now.schema)}`);
      return;
    }
    if (old.schema['format'] !== now.schema['format']) {
      this.report(
        where,
        `format changed from ${JSON.stringify(old.schema['format'] ?? null)} to ${JSON.stringify(now.schema['format'] ?? null)}`,
      );
    }
    this.compareValues(where, direction, old.schema, now.schema);
    if (direction === 'request') {
      this.compareBounds(where, old.schema, now.schema);
    }
    if (kind === 'array') {
      this.compare(`${where}[]`, direction, old.schema['items'], now.schema['items']);
    } else if (kind === 'oneOf') {
      this.compareVariants(where, direction, old.schema, now.schema);
    } else if (kind === 'object') {
      this.compareProperties(where, direction, old.schema, now.schema);
    }
  }

  private compareValues(
    where: string,
    direction: Direction,
    old: JsonObject,
    now: JsonObject,
  ): void {
    const before = allowedValues(old);
    const after = allowedValues(now);
    if (before === null) {
      if (after !== null && direction === 'request') {
        this.report(where, 'is now limited to a fixed set of values');
      }
      return;
    }
    if (after === null) {
      if (direction === 'response') {
        this.report(where, 'is no longer limited to a fixed set of values');
      }
      return;
    }
    const [gone, added] = [
      [...before].filter((value) => !after.has(value)),
      [...after].filter((value) => !before.has(value)),
    ];
    if (direction === 'request' && gone.length > 0) {
      this.report(where, `no longer accepts ${gone.map((value) => `"${value}"`).join(', ')}`);
    }
    if (direction === 'response' && added.length > 0) {
      this.report(where, `may now be ${added.map((value) => `"${value}"`).join(', ')}`);
    }
  }

  private compareBounds(where: string, old: JsonObject, now: JsonObject): void {
    const number = (schema: JsonObject, key: string): number | null => {
      const value = schema[key];
      return typeof value === 'number' ? value : null;
    };
    for (const key of LOWER_BOUNDS) {
      const [before, after] = [number(old, key), number(now, key)];
      if (after !== null && (before === null || after > before)) {
        this.report(where, `${key} tightened from ${String(before)} to ${after}`);
      }
    }
    for (const key of UPPER_BOUNDS) {
      const [before, after] = [number(old, key), number(now, key)];
      if (after !== null && (before === null || after < before)) {
        this.report(where, `${key} tightened from ${String(before)} to ${after}`);
      }
    }
    if (typeof now['pattern'] === 'string' && now['pattern'] !== old['pattern']) {
      this.report(where, 'pattern changed');
    }
  }

  private compareVariants(
    where: string,
    direction: Direction,
    old: JsonObject,
    now: JsonObject,
  ): void {
    const before = this.variants(this.before, old);
    const after = this.variants(this.after, now);
    for (const [tag, variant] of before) {
      if (!after.has(tag)) {
        if (direction === 'request') {
          this.report(where, `variant "${tag}" is no longer accepted`);
        }
        continue;
      }
      this.compare(`${where}<${tag}>`, direction, variant, after.get(tag));
    }
    if (direction === 'response') {
      for (const tag of after.keys()) {
        if (!before.has(tag)) {
          this.report(where, `variant "${tag}" may now be sent`);
        }
      }
    }
  }

  private compareProperties(
    where: string,
    direction: Direction,
    old: JsonObject,
    now: JsonObject,
  ): void {
    const before = isObject(old['properties']) ? old['properties'] : {};
    const after = isObject(now['properties']) ? now['properties'] : {};
    const [wasRequired, isRequired] = [stringSet(old['required']), stringSet(now['required'])];
    for (const [name, property] of Object.entries(before)) {
      const here = `${where}.${name}`;
      if (!(name in after)) {
        this.report(
          here,
          direction === 'request' ? 'is no longer accepted' : 'is no longer returned',
        );
        continue;
      }
      if (direction === 'request' && !wasRequired.has(name) && isRequired.has(name)) {
        this.report(here, 'became required');
      }
      if (direction === 'response' && wasRequired.has(name) && !isRequired.has(name)) {
        this.report(here, 'may now be absent');
      }
      this.compare(here, direction, property, after[name]);
    }
    if (direction === 'request') {
      for (const name of Object.keys(after)) {
        if (!(name in before) && isRequired.has(name)) {
          this.report(`${where}.${name}`, 'is new and required');
        }
      }
    }
  }
}

function bodySchema(container: Json | undefined): Json | undefined {
  if (!isObject(container) || !isObject(container['content'])) {
    return undefined;
  }
  const [media] = Object.values(container['content']);
  return isObject(media) ? media['schema'] : undefined;
}

/** Every change from `before` to `after` that an old client cannot live with. */
export function findBreakingChanges(
  before: OpenApiDocument,
  after: OpenApiDocument,
): BreakingChange[] {
  const comparison = new Comparison({ document: before }, { document: after });
  const { changes } = comparison;
  for (const [path, item] of Object.entries(before.paths)) {
    const newItem = after.paths[path];
    for (const method of METHODS) {
      const old = isObject(item) ? item[method] : undefined;
      if (!isObject(old)) {
        continue;
      }
      const where = `${method.toUpperCase()} ${path}`;
      const now = isObject(newItem) ? newItem[method] : undefined;
      if (!isObject(now)) {
        changes.push({ where, what: 'the operation was removed' });
        continue;
      }
      const [oldBody, newBody] = [bodySchema(old['requestBody']), bodySchema(now['requestBody'])];
      if (oldBody !== undefined && newBody === undefined) {
        changes.push({ where, what: 'no longer takes a request body' });
      } else if (oldBody === undefined && newBody !== undefined) {
        changes.push({ where, what: 'now needs a request body' });
      } else if (oldBody !== undefined) {
        comparison.compare(`${where} request`, 'request', oldBody, newBody);
      }
      const oldResponses = isObject(old['responses']) ? old['responses'] : {};
      const newResponses = isObject(now['responses']) ? now['responses'] : {};
      for (const [status, response] of Object.entries(oldResponses)) {
        if (!status.startsWith('2')) {
          continue;
        }
        if (!(status in newResponses)) {
          changes.push({ where, what: `no longer answers ${status}` });
          continue;
        }
        const [oldSchema, newSchema] = [bodySchema(response), bodySchema(newResponses[status])];
        if (oldSchema !== undefined) {
          comparison.compare(`${where} response ${status}`, 'response', oldSchema, newSchema);
        }
      }
    }
  }
  return changes;
}

/** The major part of the document's `info.version`. */
export function majorVersion(document: OpenApiDocument): number {
  const version = document.info['version'];
  const major =
    typeof version === 'string' ? Number.parseInt(version.split('.')[0] ?? '', 10) : Number.NaN;
  if (!Number.isInteger(major)) {
    throw new Error(`info.version is not a semantic version: ${JSON.stringify(version ?? null)}`);
  }
  return major;
}

/** Outcome of comparing the committed document with an older one. */
export interface BreakingReport {
  readonly changes: readonly BreakingChange[];
  /** True when the major version moved, which is what allows breaking changes. */
  readonly majorBumped: boolean;
  /** True when the check passes: nothing breaks, or the major version says so. */
  readonly ok: boolean;
}

/** Compares two versions of the document. */
export function checkBreaking(before: OpenApiDocument, after: OpenApiDocument): BreakingReport {
  const changes = findBreakingChanges(before, after);
  const majorBumped = majorVersion(after) > majorVersion(before);
  return { changes, majorBumped, ok: changes.length === 0 || majorBumped };
}
