import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { describe, it } from "node:test";
import { initializeDatabase } from "../packages/agentkit/dist/adapters-sqlite/sqlite/open.js";
import { SqliteConnection } from "../packages/agentkit/dist/adapters-sqlite/sqlite/connection.js";
import {
  bun,
  Store,
  raw,
  scratch,
  seed,
  rows,
  scalar,
  snapshot,
  verifyPopulated,
  SCHEMA_VERSION,
  migration,
  upgradeOptions,
} from "./qualification/sqlite-fixture.mjs";
if (process.argv[2] === "--seed") {
  await seed(process.argv[3]);
  process.exit(0);
}

describe(`SQLite durability: ${bun ? "Bun" : "Node"}`, () => {
  it("reopens populated data and preserves ordering", () =>
    scratch(async (path) => {
      await seed(path);
      const store = new Store(path);
      try {
        assert.equal(
          (await store.conversations.listMessages("chat"))[0].content,
          "persistent",
        );
        await verifyPopulated(store);
        const next = await store.conversations.appendMessage({
          chatId: "chat",
          role: "assistant",
          content: "second",
        });
        assert.equal(next.orderKey, 3);
      } finally {
        store.close();
      }
    }));
  for (const version of [0, 7, SCHEMA_VERSION + 1])
    it(`refuses foreign/version ${version} without altering bytes`, () =>
      scratch(async (path) => {
        const db = raw(path);
        db.exec(
          `CREATE TABLE application_data (value TEXT); INSERT INTO application_data VALUES ('keep'); PRAGMA user_version = ${version};`,
        );
        db.close();
        const before = readFileSync(path);
        assert.throws(
          () => new Store(path),
          /unsupported|Unrecognized SQLite table/,
        );
        assert.deepEqual(readFileSync(path), before);
      }));
  it("refuses a foreign database spoofing schema v8", () =>
    scratch(async (path) => {
      const db = raw(path);
      db.exec(
        "CREATE TABLE application_data(value TEXT); PRAGMA user_version = 8",
      );
      db.close();
      const before = readFileSync(path);
      assert.throws(() => new Store(path), /Unrecognized SQLite table/);
      assert.deepEqual(readFileSync(path), before);
    }));
  for (const populated of [false, true])
    it(`refuses nonzero application_id on ${populated ? "AgentKit" : "empty"} database without writes`, () =>
      scratch(async (path) => {
        if (populated) await seed(path, true);
        const db = raw(path);
        db.exec("PRAGMA application_id = 123456");
        db.close();
        const before = readFileSync(path);
        assert.throws(() => new Store(path), /Foreign SQLite application_id/);
        assert.deepEqual(readFileSync(path), before);
      }));
  for (const table of ["application_data", "message_search_foreign"])
    it(`refuses extra foreign table ${table} in valid v8`, () =>
      scratch(async (path) => {
        await seed(path, true);
        const db = raw(path);
        db.exec(
          `CREATE TABLE ${table} (value TEXT); INSERT INTO ${table} VALUES ('keep')`,
        );
        db.close();
        const before = readFileSync(path);
        assert.throws(() => new Store(path), /Unrecognized SQLite table/);
        assert.deepEqual(readFileSync(path), before);
      }));
  it("opens original v8 and backfills empty FTS index without losing parts", () =>
    scratch(async (path) => {
      await seed(path, true);
      const db = raw(path);
      db.exec(
        "INSERT INTO message_search(message_search) VALUES ('delete-all')",
      );
      db.close();
      const store = new Store(path);
      try {
        await verifyPopulated(store);
      } finally {
        store.close();
      }
    }));
  it(`reads ${bun ? "Node" : "Bun"} file, then appends in order`, () =>
    scratch(async (path) => {
      const child = spawnSync(
        bun ? "node" : "bun",
        [fileURLToPath(import.meta.url), "--seed", path],
        {
          encoding: "utf8",
          env: { ...process.env, AGENTKIT_SQLITE_DRIVER: bun ? "node" : "bun" },
          timeout: 30000,
        },
      );
      assert.equal(child.status, 0, `${child.error ?? ""}\n${child.stderr}`);
      const store = new Store(path);
      try {
        await verifyPopulated(store);
        const message = await store.conversations.appendMessage({
          chatId: "chat",
          role: "assistant",
          content: "cross runtime",
        });
        assert.equal(message.orderKey, 3);
      } finally {
        store.close();
      }
    }));
  it("normalizes booleans, null, bigint, blobs and named bindings", () => {
    const db = raw(":memory:");
    const connection = new SqliteConnection(db, 100);
    try {
      const row = connection.get(
        "SELECT $yes AS yes, :no AS no, @empty AS empty, $integer AS integer, $blob AS blob",
        {
          $yes: true,
          ":no": false,
          "@empty": null,
          $integer: 42n,
          $blob: new Uint8Array([0, 127, 255]),
        },
      );
      assert.deepEqual(
        { ...row, blob: Array.from(row.blob) },
        { yes: 1, no: 0, empty: null, integer: 42, blob: [0, 127, 255] },
      );
      assert.equal(connection.get("SELECT 1 WHERE 0"), null);
      assert.deepEqual(
        connection.all("SELECT $integer AS integer", { $integer: 42n }),
        [{ integer: 42 }],
      );
      assert.throws(
        () => connection.get("SELECT 9007199254740992 AS unsafe"),
        /safe integer/,
      );
      connection.exec("CREATE TABLE bindings(value BLOB)");
      assert.equal(
        connection.run("INSERT INTO bindings VALUES ($value)", { $value: null })
          .changes,
        1,
      );
      assert.equal(
        connection.run("UPDATE bindings SET value = $value", { $value: true })
          .changes,
        1,
      );
      assert.equal(connection.get("SELECT value FROM bindings").value, 1);
      if (!bun)
        assert.throws(
          () =>
            connection.get("SELECT $same + :same", { $same: 1, ":same": 2 }),
          /Ambiguous SQLite parameter same/,
        );
    } finally {
      db.close();
    }
  });
  it("creates fresh baseline and applies migration in one transaction", () =>
    scratch(async (path) => {
      const db = initializeDatabase(raw(path), path, 100, upgradeOptions());
      try {
        assert.equal(
          scalar(db, "PRAGMA user_version", "user_version"),
          SCHEMA_VERSION + 1,
        );
        assert.equal(rows(db, "SELECT * FROM migration_receipts").length, 1);
        assert.equal(rows(db, "SELECT * FROM messages").length, 0);
      } finally {
        db.close();
      }
    }));
  it("rolls back fresh baseline when migration fails", () =>
    scratch(async (path) => {
      const broken = {
        ...migration,
        sql: `${migration.sql} INSERT INTO missing_table VALUES (1)`,
      };
      assert.throws(() =>
        initializeDatabase(raw(path), path, 100, upgradeOptions(broken)),
      );
      const db = raw(path);
      try {
        assert.equal(scalar(db, "PRAGMA user_version", "user_version"), 0);
        assert.equal(rows(db, "SELECT * FROM sqlite_master").length, 0);
      } finally {
        db.close();
      }
    }));
  it("refuses a version stamp whose migrated table is missing", () =>
    scratch(async (path) => {
      await seed(path);
      const db = raw(path);
      db.exec(`PRAGMA user_version = ${SCHEMA_VERSION + 1}`);
      db.close();
      const before = readFileSync(path);
      assert.throws(
        () => initializeDatabase(raw(path), path, 100, upgradeOptions()),
        /Missing migrated AgentKit table/,
      );
      assert.deepEqual(readFileSync(path), before);
    }));
  it("refuses broken baseline layout and foreign keys without writes", () =>
    scratch(async (path) => {
      await seed(path, true);
      let db = raw(path);
      db.exec("PRAGMA foreign_keys = OFF; DELETE FROM chats WHERE id = 'chat'");
      db.close();
      let before = readFileSync(path);
      assert.throws(() => new Store(path), /integrity check failed/);
      assert.deepEqual(readFileSync(path), before);
      db = raw(path);
      db.exec("ALTER TABLE tasks ADD COLUMN foreign_data TEXT");
      db.close();
      before = readFileSync(path);
      assert.throws(() => new Store(path), /Unrecognized AgentKit table tasks/);
      assert.deepEqual(readFileSync(path), before);
    }));
  it("closes refused handles", () => {
    const db = raw(":memory:");
    let closed = false;
    const close = db.close.bind(db);
    db.close = () => {
      closed = true;
      close();
    };
    db.exec("PRAGMA application_id = 123456");
    assert.throws(
      () => initializeDatabase(db, ":memory:"),
      /Foreign SQLite application_id/,
    );
    assert.equal(closed, true);
  });
  it("rolls back failed migration DDL, data and version; retry commits once", () =>
    scratch(async (path) => {
      await seed(path);
      let db = raw(path);
      const before = snapshot(db);
      db.close();
      const broken = {
        ...migration,
        sql: `${migration.sql} UPDATE messages SET content = 'lost'; DELETE FROM task_events; DELETE FROM proposal_outcomes; UPDATE task_attempts SET status = 'failed'; INSERT INTO missing_table VALUES (1);`,
      };
      assert.throws(() =>
        initializeDatabase(raw(path), path, 100, upgradeOptions(broken)),
      );
      db = raw(path);
      assert.equal(
        scalar(db, "PRAGMA user_version", "user_version"),
        SCHEMA_VERSION,
      );
      assert.equal(
        rows(
          db,
          "SELECT name FROM sqlite_master WHERE name = 'migration_receipts'",
        ).length,
        0,
      );
      assert.deepEqual(snapshot(db), before);
      assert.equal(
        rows(
          db,
          "SELECT rowid FROM message_search WHERE message_search MATCH 'searchable'",
        ).length,
        1,
      );
      db.close();
      db = initializeDatabase(raw(path), path, 100, upgradeOptions());
      assert.equal(
        scalar(db, "PRAGMA user_version", "user_version"),
        SCHEMA_VERSION + 1,
      );
      assert.equal(rows(db, "SELECT * FROM migration_receipts").length, 1);
      assert.deepEqual(snapshot(db), before);
      db.close();
      db = initializeDatabase(raw(path), path, 100, upgradeOptions());
      assert.equal(rows(db, "SELECT * FROM migration_receipts").length, 1);
      db.close();
    }));
  it("refuses missing migration before writes", () =>
    scratch(async (path) => {
      await seed(path, true);
      const before = readFileSync(path);
      assert.throws(
        () =>
          initializeDatabase(
            raw(path),
            path,
            100,
            upgradeOptions(migration, SCHEMA_VERSION + 2),
          ),
        /No unique SQLite migration/,
      );
      assert.deepEqual(readFileSync(path), before);
    }));
  it("serializes a foreign write behind rollback", async () => {
    const store = new Store(":memory:");
    let unblock;
    const wait = new Promise((resolve) => {
      unblock = resolve;
    });
    let entered;
    const ready = new Promise((resolve) => {
      entered = resolve;
    });
    const tx = store.transaction(async (scoped) => {
      await scoped.conversations.createChat({ id: "discard" });
      entered();
      await wait;
      throw new Error("rollback");
    });
    const rejected = assert.rejects(tx, /rollback/);
    await ready;
    const committed = store.conversations.createChat({ id: "keep" });
    unblock();
    await rejected;
    await committed;
    assert.equal(await store.conversations.getChat("discard"), null);
    assert.equal((await store.conversations.getChat("keep")).id, "keep");
    store.close();
  });
  it("rejects unsafe integer row conversion", async () => {
    const store = new Store(":memory:");
    store.database.exec("UPDATE fencing_counter SET value = 9007199254740992");
    await store.tasks.createTask({
      taskId: "task",
      scopeId: "scope",
      kind: "work",
      payload: {},
    });
    await assert.rejects(
      store.tasks.claimNext({
        ownerId: "worker",
        now: new Date(),
        scopesBusy: [],
      }),
      /safe integer/,
    );
    store.close();
  });
});
