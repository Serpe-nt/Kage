import { checkBreaking } from './breaking.js';
import {
  buildOpenApiDocument,
  findDialectViolations,
  serializeOpenApi,
  type OpenApiDocument,
} from './openapi.js';

/** What the command line needs from the outside world; tests pass fakes. */
export interface CliIo {
  /** Reads a text file; `null` when it does not exist. */
  read(path: string): string | null;
  write(path: string, text: string): void;
  out(line: string): void;
  err(line: string): void;
}

const USAGE = [
  'usage: contract <command>',
  '  emit <file>               write the OpenAPI document',
  '  check <file>              fail unless <file> is what the schemas emit',
  '  breaking <old> <file>     fail when <file> breaks clients of <old>',
].join('\n');

function parse(text: string, path: string): OpenApiDocument {
  try {
    return JSON.parse(text) as OpenApiDocument;
  } catch (error) {
    throw new Error(
      `${path} is not JSON: ${error instanceof Error ? error.message : String(error)}`,
      { cause: error },
    );
  }
}

/**
 * Runs one contract command and returns the process exit code.
 *
 * `emit` and `check` first make sure the document stays inside the dialect
 * the Rust generator reads, so a schema it would not understand fails here,
 * next to the schema, and not later in a Rust build.
 */
export function runCli(argv: readonly string[], io: CliIo): number {
  const [command, first, second] = argv;
  if (command === 'emit' || command === 'check') {
    if (first === undefined) {
      io.err(USAGE);
      return 2;
    }
    const document = buildOpenApiDocument();
    const violations = findDialectViolations(document);
    if (violations.length > 0) {
      io.err('the contract schemas leave the dialect the Rust generator reads:');
      for (const violation of violations) {
        io.err(`  ${violation}`);
      }
      return 1;
    }
    const text = serializeOpenApi(document);
    if (command === 'emit') {
      io.write(first, text);
      io.out(`wrote ${first}`);
      return 0;
    }
    if (io.read(first) === text) {
      io.out(`${first} matches the schemas`);
      return 0;
    }
    io.err(
      `${first} is not what the zod schemas emit. Run \`just openapi\` and commit the result.`,
    );
    return 1;
  }
  if (command === 'breaking') {
    if (first === undefined || second === undefined) {
      io.err(USAGE);
      return 2;
    }
    const [oldText, newText] = [io.read(first), io.read(second)];
    if (newText === null) {
      io.err(`${second} does not exist`);
      return 1;
    }
    if (oldText === null) {
      io.out(`${first} does not exist: nothing to compare with, the contract is new`);
      return 0;
    }
    const report = checkBreaking(parse(oldText, first), parse(newText, second));
    if (report.changes.length === 0) {
      io.out('no breaking change in the sidecar contract');
      return 0;
    }
    const print = (line: string) => {
      if (report.ok) {
        io.out(line);
      } else {
        io.err(line);
      }
    };
    print(
      report.majorBumped
        ? 'breaking changes, covered by the new major version of the contract:'
        : 'breaking changes in the sidecar contract without a new major version (CONTRACT_SEMVER):',
    );
    for (const change of report.changes) {
      print(`  ${change.where}: ${change.what}`);
    }
    return report.ok ? 0 : 1;
  }
  io.err(USAGE);
  return 2;
}
