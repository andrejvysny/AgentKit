import { SCHEMA_V8, SCHEMA_VERSION } from "../schema.js";
import type { SqliteDatabase } from "./driver.js";
import { DEFAULT_BUSY_TIMEOUT_MS } from "./connection.js";
import {
  assertIntegrity,
  inspectSchema,
  migrationPath,
  MIN_SUPPORTED_SCHEMA_VERSION,
  SQLITE_MIGRATIONS,
  type SqliteMigration,
} from "./migrations.js";

/** Internal registry seam also used by fault-injection tests; no public version override. */
export interface SchemaUpgradeOptions {
  targetVersion: number;
  migrations: readonly SqliteMigration[];
}

function upgrade(db: SqliteDatabase, options: SchemaUpgradeOptions): void {
  // Validate before acquiring a write lock or changing persistent pragmas.
  const before = inspectSchema(db, options.targetVersion, options.migrations);
  migrationPath(
    before || MIN_SUPPORTED_SCHEMA_VERSION,
    options.targetVersion,
    options.migrations,
  );
  db.exec("BEGIN IMMEDIATE");
  try {
    const version = inspectSchema(
      db,
      options.targetVersion,
      options.migrations,
    );
    // Supported layouts were checked above; preserve the guarded FTS repair on reopen.
    db.exec(SCHEMA_V8);
    for (const step of migrationPath(
      version || MIN_SUPPORTED_SCHEMA_VERSION,
      options.targetVersion,
      options.migrations,
    ))
      db.exec(step.sql);
    assertIntegrity(db);
    db.exec(`PRAGMA user_version = ${options.targetVersion}`);
    db.exec("COMMIT");
  } catch (error) {
    try {
      db.exec("ROLLBACK");
    } catch {
      /* Preserve the original migration error. */
    }
    throw error;
  }
}

/** Owns the handle until successful return; every refused/failed open closes it. */
export function initializeDatabase(
  db: SqliteDatabase,
  path: string,
  busyTimeoutMs = DEFAULT_BUSY_TIMEOUT_MS,
  options: SchemaUpgradeOptions = {
    targetVersion: SCHEMA_VERSION,
    migrations: SQLITE_MIGRATIONS,
  },
): SqliteDatabase {
  try {
    if (
      !Number.isSafeInteger(busyTimeoutMs) ||
      busyTimeoutMs < 0 ||
      !Number.isSafeInteger(options.targetVersion) ||
      options.targetVersion < SCHEMA_VERSION
    )
      throw new RangeError("Invalid SQLite open options");
    db.exec(`PRAGMA busy_timeout = ${busyTimeoutMs}`);
    db.exec("PRAGMA foreign_keys = ON");
    upgrade(db, options);
    if (path !== ":memory:") db.exec("PRAGMA journal_mode = WAL");
    return db;
  } catch (error) {
    try {
      db.close();
    } catch {
      /* The refusal is more useful than a close failure. */
    }
    throw error;
  }
}
