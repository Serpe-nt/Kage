/**
 * The public API's OpenAPI document (`crates/kage-api/openapi.json`), seen
 * from outside the server that emits it: checked against the OpenAPI
 * specification's own schema, and turned into the TypeScript types the
 * client is typed with.
 */
import { Validator } from '@seriousme/openapi-schema-validator';
import openapiTS, { astToString } from 'openapi-typescript';

/** What stands at the top of the generated types. */
export const GENERATED_HEADER = [
  '/**',
  ' * The types of the Kage API, generated from crates/kage-api/openapi.json',
  ' * by openapi-typescript. Do not edit: run `just openapi`.',
  ' */',
  '',
  '',
].join('\n');

/** A document that is not an OpenAPI 3.1 document, and why. */
export class InvalidDocumentError extends Error {
  constructor(readonly problems: readonly string[]) {
    super(`the document is not a valid OpenAPI document:\n  ${problems.join('\n  ')}`);
    this.name = 'InvalidDocumentError';
  }
}

function parse(text: string): Record<string, unknown> {
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (error) {
    throw new InvalidDocumentError([
      `not JSON: ${error instanceof Error ? error.message : String(error)}`,
    ]);
  }
  if (typeof parsed !== 'object' || parsed === null || Array.isArray(parsed)) {
    throw new InvalidDocumentError(['not a JSON object']);
  }
  return parsed as Record<string, unknown>;
}

/**
 * Validates a document against the schema of the OpenAPI specification it
 * names, and returns the version it was validated as.
 *
 * @throws InvalidDocumentError with every violation; a document of any
 * version but 3.1 is one.
 */
export async function validateDocument(text: string): Promise<string> {
  const document = parse(text);
  const validator = new Validator();
  const result = await validator.validate(document);
  if (!result.valid) {
    const errors = result.errors ?? 'no reason given';
    const problems =
      typeof errors === 'string'
        ? [errors]
        : errors.map((error) => `${error.instancePath || '/'}: ${error.message ?? error.keyword}`);
    throw new InvalidDocumentError(problems);
  }
  if (validator.version !== '3.1') {
    throw new InvalidDocumentError([
      `it is an OpenAPI ${validator.version} document; the API's is 3.1`,
    ]);
  }
  return validator.version;
}

/** The TypeScript types of a document, as `src/schema.ts` holds them. */
export async function generateTypes(text: string): Promise<string> {
  parse(text);
  // A buffer is the document itself to the generator; a string could be
  // taken for the name of a file.
  const ast = await openapiTS(Buffer.from(text, 'utf8'));
  return GENERATED_HEADER + astToString(ast);
}
