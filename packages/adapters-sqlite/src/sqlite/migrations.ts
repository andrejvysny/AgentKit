import { AgentKitHostError } from "@agentkit/host";
import { SCHEMA_V8, SCHEMA_VERSION } from "../schema.js";
import { SCHEMA_V9_PROVIDER_CONTINUATIONS } from "../provider-continuation-schema.js";
import { normalizeRow, type SqliteDatabase } from "./driver.js";

export interface SqliteMigration {
  readonly from: number;
  readonly to: number;
  /** Transactional SQL only. No transaction control or external side effects. */
  readonly sql: string;
}

export const SQLITE_MIGRATIONS: readonly SqliteMigration[] = Object.freeze([
  Object.freeze({ from: 8, to: 9, sql: SCHEMA_V9_PROVIDER_CONTINUATIONS }),
]);
export const MIN_SUPPORTED_SCHEMA_VERSION = 8;

function versionOf(db: SqliteDatabase): number {
  const row = normalizeRow(db.query("PRAGMA user_version").get()) as {
    user_version?: number;
  } | null;
  if (!Number.isSafeInteger(row?.user_version))
    throw new AgentKitHostError(
      "sqlite_schema_version",
      "Cannot read SQLite schema version.",
    );
  return row?.user_version as number;
}

function canonical(sql: string): string {
  return sql
    .replace(/--[^\n]*/g, "")
    .replace(/IF NOT EXISTS/gi, "")
    .replace(/\s+/g, " ")
    .trim()
    .replace(/;$/, "");
}

function declaredTables(sql: string): string[] {
  return [
    ...sql.matchAll(
      /CREATE\s+(?:VIRTUAL\s+)?TABLE\s+(?:IF\s+NOT\s+EXISTS\s+)?(\w+)/gi,
    ),
  ].map((match) => match[1] as string);
}

function assertOwnership(
  db: SqliteDatabase,
  version: number,
  migrations: readonly SqliteMigration[],
): void {
  const application = normalizeRow(db.query("PRAGMA application_id").get()) as {
    application_id: number;
  };
  if (application.application_id !== 0) {
    throw new AgentKitHostError(
      "sqlite_schema_integrity",
      "Foreign SQLite application_id; refusing to change this database.",
    );
  }
  const allowed = new Set(declaredTables(SCHEMA_V8));
  const migrated = new Set<string>();
  for (const step of migrations.filter(
    (migration) => migration.to <= version,
  )) {
    for (const name of declaredTables(step.sql)) {
      allowed.add(name);
      migrated.add(name);
    }
  }
  // FTS5 creates these shadow tables itself; a prefix match could admit foreign tables.
  for (const suffix of ["data", "idx", "docsize", "config"])
    allowed.add(`message_search_${suffix}`);
  const tables = db
    .query(
      "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
    )
    .all() as { name: string }[];
  for (const { name } of tables) {
    if (!allowed.has(name)) {
      throw new AgentKitHostError(
        "sqlite_schema_integrity",
        `Unrecognized SQLite table ${name}; refusing to change this database.`,
      );
    }
  }
  for (const name of migrated) {
    if (!tables.some((table) => table.name === name)) {
      throw new AgentKitHostError(
        "sqlite_schema_integrity",
        `Missing migrated AgentKit table ${name}; refusing to change this database.`,
      );
    }
  }
}

/** v8 predates an application-id marker; adopt only its complete table layout. */
function assertBaseline(db: SqliteDatabase): void {
  const statements =
    SCHEMA_V8.match(/CREATE TABLE IF NOT EXISTS \w+ \([\s\S]*?\n\);/g) ?? [];
  for (const sql of statements) {
    const name = sql.match(/EXISTS (\w+)/)?.[1];
    const row = db
      .query(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = $name",
      )
      .get({ $name: name ?? "" }) as { sql: string } | null;
    if (!row || canonical(row.sql) !== canonical(sql)) {
      throw new AgentKitHostError(
        "sqlite_schema_integrity",
        `Unrecognized AgentKit table ${name}; refusing to change this database.`,
      );
    }
  }
}

/** Version markers cannot make a table with foreign columns authoritative. */
function assertMigratedTables(
  db: SqliteDatabase,
  version: number,
  migrations: readonly SqliteMigration[],
): void {
  for (const migration of migrations.filter((step) => step.to <= version)) {
    const statements =
      migration.sql.match(
        /CREATE TABLE(?: IF NOT EXISTS)? \w+ \([\s\S]*?\n\);/g,
      ) ?? [];
    for (const sql of statements) {
      const name = sql.match(/TABLE(?: IF NOT EXISTS)? (\w+)/)?.[1];
      const row = db
        .query(
          "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = $name",
        )
        .get({ $name: name ?? "" }) as { sql: string } | null;
      if (!row || canonical(row.sql) !== canonical(sql)) {
        throw new AgentKitHostError(
          "sqlite_schema_integrity",
          `Unrecognized migrated AgentKit table ${name}; refusing to change this database.`,
        );
      }
    }
  }
}

export function assertIntegrity(db: SqliteDatabase): void {
  const check = db.query("PRAGMA quick_check").all() as {
    quick_check: string;
  }[];
  if (
    check.length !== 1 ||
    check[0]?.quick_check !== "ok" ||
    db.query("PRAGMA foreign_key_check").all().length !== 0
  ) {
    throw new AgentKitHostError(
      "sqlite_schema_integrity",
      "SQLite integrity check failed; restore a verified backup before opening.",
    );
  }
}

export function inspectSchema(
  db: SqliteDatabase,
  target = SCHEMA_VERSION,
  migrations = SQLITE_MIGRATIONS,
): number {
  const version = versionOf(db);
  if (
    version !== 0 &&
    (version < MIN_SUPPORTED_SCHEMA_VERSION || version > target)
  ) {
    unsupportedVersion(version, target);
  }
  assertOwnership(db, version, migrations);
  const objects = db
    .query("SELECT name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'")
    .all();
  if (version === 0 && objects.length === 0) return 0;
  if (version < MIN_SUPPORTED_SCHEMA_VERSION || version > target) {
    unsupportedVersion(version, target);
  }
  assertBaseline(db);
  assertMigratedTables(db, version, migrations);
  assertIntegrity(db);
  return version;
}

function unsupportedVersion(version: number, target: number): never {
  throw new AgentKitHostError(
    "sqlite_schema_version",
    `SQLite schema ${version} is unsupported (supported ${MIN_SUPPORTED_SCHEMA_VERSION}..${target}); no migrations exist for older development schemas. Use a compatible AgentKit release or a verified backup.`,
    { found: version, expected: target },
  );
}

export function migrationPath(
  from: number,
  target: number,
  migrations: readonly SqliteMigration[],
): readonly SqliteMigration[] {
  const path: SqliteMigration[] = [];
  for (let current = from; current < target; ) {
    const matches = migrations.filter((step) => step.from === current);
    const step = matches[0];
    if (
      matches.length !== 1 ||
      !step ||
      step.to !== current + 1 ||
      step.to > target
    ) {
      throw new AgentKitHostError(
        "sqlite_schema_version",
        `No unique SQLite migration from ${current} to ${target}.`,
      );
    }
    if (
      /\b(BEGIN|COMMIT|ROLLBACK|VACUUM|ATTACH|DETACH|PRAGMA)\b/i.test(step.sql)
    ) {
      throw new AgentKitHostError(
        "sqlite_schema_version",
        "Migration must contain transactional schema/data SQL only.",
      );
    }
    path.push(step);
    current = step.to;
  }
  return path;
}
