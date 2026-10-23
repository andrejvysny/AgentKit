import type { PromptTemplate } from "@agentkit/contracts";
import type { PromptTemplateStore } from "@agentkit/host";
import type { SqliteDatabase } from "./sqlite/driver.js";
import { writeGateFor } from "./shared-assistant-store.js";
export class SharedSqlitePromptTemplateStore implements PromptTemplateStore {
  constructor(private readonly db: SqliteDatabase) {
    db.exec(
      "CREATE TABLE IF NOT EXISTS prompt_templates (id TEXT NOT NULL, revision TEXT NOT NULL, template TEXT NOT NULL, PRIMARY KEY(id, revision))",
    );
  }
  async put(template: PromptTemplate) {
    await writeGateFor(this.db).whenFree(() => {
      this.db.run(
        "INSERT INTO prompt_templates (id, revision, template) VALUES ($id, $revision, $template)",
        {
          $id: template.id,
          $revision: template.revision,
          $template: JSON.stringify(template),
        },
      );
    });
  }
  async get(id: string, revision: string): Promise<PromptTemplate | null> {
    const row = this.db
      .query(
        "SELECT template FROM prompt_templates WHERE id = $id AND revision = $revision",
      )
      .get({ $id: id, $revision: revision }) as { template: string } | null;
    return row ? JSON.parse(row.template) : null;
  }
  async list(): Promise<PromptTemplate[]> {
    return (
      this.db
        .query("SELECT template FROM prompt_templates ORDER BY id, revision")
        .all() as { template: string }[]
    )
      .map((row) => JSON.parse(row.template) as PromptTemplate | null)
      .filter((template): template is PromptTemplate => template !== null);
  }
  async delete(id: string, revision: string) {
    await writeGateFor(this.db).whenFree(() => {
      this.db.run(
        // Keep the primary key reserved while erasing the deleted body.
        "UPDATE prompt_templates SET template = 'null' WHERE id = $id AND revision = $revision",
        { $id: id, $revision: revision },
      );
    });
  }
}
