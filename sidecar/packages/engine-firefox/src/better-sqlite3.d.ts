/**
 * The part of better-sqlite3 the engine uses: reading the launcher's WebGL
 * table. The package ships no type declarations of its own.
 */
declare module 'better-sqlite3' {
  export default class Database {
    constructor(file: string, options?: { readonly?: boolean; fileMustExist?: boolean });
    prepare(sql: string): { all(...parameters: unknown[]): unknown[] };
    close(): void;
  }
}
