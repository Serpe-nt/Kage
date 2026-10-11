import { generateTypes, InvalidDocumentError, validateDocument } from './document.js';

/** What the command line needs from the outside world; tests pass fakes. */
export interface CliIo {
  /** Reads a text file; `null` when it does not exist. */
  read(path: string): string | null;
  write(path: string, text: string): void;
  out(line: string): void;
  err(line: string): void;
}

const USAGE = [
  'usage: api-client <command>',
  '  validate <openapi.json>              fail unless the document is valid OpenAPI 3.1',
  '  generate <openapi.json> <schema.ts>  write the TypeScript types of the document',
  '  check <openapi.json> <schema.ts>     validate, and fail unless <schema.ts> is current',
].join('\n');

/** Line endings as git stores them, whatever the checkout made of them. */
const normalized = (text: string): string => text.replaceAll('\r\n', '\n');

/** Runs one command and returns the process exit code. */
export async function runCli(argv: readonly string[], io: CliIo): Promise<number> {
  const [command, documentPath, typesPath] = argv;
  const known = command === 'validate' || command === 'generate' || command === 'check';
  if (!known || documentPath === undefined || (command !== 'validate' && typesPath === undefined)) {
    io.err(USAGE);
    return 2;
  }
  const text = io.read(documentPath);
  if (text === null) {
    io.err(`${documentPath} does not exist. Run \`just openapi\`.`);
    return 1;
  }
  try {
    const version = await validateDocument(text);
    if (command === 'validate' || typesPath === undefined) {
      io.out(`${documentPath} is a valid OpenAPI ${version} document`);
      return 0;
    }
    const types = await generateTypes(text);
    if (command === 'generate') {
      io.write(typesPath, types);
      io.out(`wrote ${typesPath}`);
      return 0;
    }
    const committed = io.read(typesPath);
    if (committed !== null && normalized(committed) === normalized(types)) {
      io.out(`${documentPath} is a valid OpenAPI ${version} document and ${typesPath} is current`);
      return 0;
    }
    io.err(
      `${typesPath} is not what ${documentPath} generates. Run \`just openapi\` and commit the result.`,
    );
    return 1;
  } catch (error) {
    if (error instanceof InvalidDocumentError) {
      io.err(`${documentPath}: ${error.message}`);
      return 1;
    }
    throw error;
  }
}
