import type { AiPromptContextBlock, SkillSelection } from "@agentkit/contracts";

export interface SkillMetadata {
  name: string;
  description: string;
  license?: string;
  compatibility?: string;
  allowedTools?: string;
  metadata: Record<string, string>;
}
export interface SkillDocument {
  metadata: SkillMetadata;
  instructions: string;
}
export interface SkillSource {
  id: string;
  /** Explicit application bundles or configured directory roots only. */
  list(signal?: AbortSignal): Promise<string[]>;
  /** Paths are relative to a skill root; implementations must enforce containment. */
  read(skill: string, path: string, signal?: AbortSignal): Promise<string>;
}
export interface SkillCatalogEntry extends SkillSelection {
  sourceId: string;
  path: string;
  name: string;
  description: string;
  connectorRequirements: string[];
}
export interface ActiveSkill {
  selection: SkillSelection;
  block: AiPromptContextBlock;
  /** Supporting text only. No installer or script execution. */
  readReference(
    path: string,
    signal?: AbortSignal,
  ): Promise<{ text: string; revision: string; source: string }>;
}

/** Safe frontmatter subset: scalars, literal/folded blocks and string metadata.
 * Complex YAML tags/aliases/collections fail explicitly rather than executing code. */
export function parseSkillDocument(text: string): SkillDocument {
  if (new TextEncoder().encode(text).length > 1_000_000)
    throw new Error("Skill exceeds byte limit.");
  const normalized = text.replace(/^\uFEFF/, "").replaceAll("\r\n", "\n");
  const match = /^---\n([\s\S]*?)\n---(?:\n|$)([\s\S]*)$/.exec(normalized);
  if (!match) throw new Error("SKILL.md requires YAML frontmatter.");
  const fields: Record<string, string> = Object.create(null);
  const metadata: Record<string, string> = Object.create(null);
  const lines = match[1]!.split("\n");
  const scalar = (value: string) => {
    const trimmed = value.trim();
    if (trimmed.startsWith('"')) {
      const parsed: unknown = JSON.parse(trimmed);
      if (typeof parsed !== "string")
        throw new Error("Expected string metadata.");
      return parsed;
    }
    if (trimmed.startsWith("'")) {
      if (!trimmed.endsWith("'")) throw new Error("Unterminated YAML string.");
      return trimmed.slice(1, -1).replaceAll("''", "'");
    }
    if (/^[&*![\]{}>|]/.test(trimmed))
      throw new Error("Unsupported YAML frontmatter value.");
    return trimmed.replace(/\s+#.*$/, "");
  };
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i]!;
    if (!line.trim() || line.trimStart().startsWith("#")) continue;
    const pair = /^([a-z][a-z0-9-]*):(?:\s+(.*))?$/.exec(line);
    if (!pair) throw new Error("Unsupported YAML frontmatter syntax.");
    const key = pair[1]!;
    if (Object.hasOwn(fields, key))
      throw new Error("Duplicate frontmatter key.");
    const value = pair[2] ?? "";
    if (key === "metadata" && !value) {
      fields[key] = "";
      while (
        i + 1 < lines.length &&
        (/^\s/.test(lines[i + 1]!) || !lines[i + 1]!.trim())
      ) {
        const nested = lines[++i]!;
        if (!nested.trim() || nested.trimStart().startsWith("#")) continue;
        const item = /^ {2}([A-Za-z0-9_-]+):\s*(.*)$/.exec(nested);
        if (!item || Object.hasOwn(metadata, item[1]!))
          throw new Error("Invalid skill metadata.");
        metadata[item[1]!] = scalar(item[2]!);
      }
    } else if (/^[|>][-+]?$/.test(value)) {
      const block: string[] = [];
      while (
        i + 1 < lines.length &&
        (/^\s/.test(lines[i + 1]!) || !lines[i + 1]!.trim())
      )
        block.push(lines[++i]!.replace(/^ {2}/, ""));
      fields[key] = (
        value.startsWith(">") ? block.join(" ") : block.join("\n")
      ).trim();
    } else fields[key] = scalar(value);
  }
  if (
    !fields.name ||
    !/^[a-z0-9]+(?:-[a-z0-9]+)*$/.test(fields.name) ||
    fields.name.length > 64 ||
    !fields.description ||
    fields.description.length > 1024
  )
    throw new Error("Invalid skill name or description.");
  return {
    metadata: {
      name: fields.name,
      description: fields.description,
      metadata,
      license: fields.license,
      compatibility: fields.compatibility,
      allowedTools: fields["allowed-tools"],
    },
    instructions: match[2]!.trim(),
  };
}
export async function skillContentHash(text: string): Promise<string> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    new TextEncoder().encode(text),
  );
  return [...new Uint8Array(digest)]
    .map((value) => value.toString(16).padStart(2, "0"))
    .join("");
}
export class SkillService {
  private readonly sources: Map<string, SkillSource>;
  constructor(
    sources: readonly SkillSource[],
    private readonly recordResource?: (resource: {
      skill: SkillSelection;
      path: string;
      revision: string;
    }) => Promise<void>,
  ) {
    this.sources = new Map(sources.map((source) => [source.id, source]));
    if (this.sources.size !== sources.length)
      throw new Error("Duplicate skill source.");
  }
  async catalog(signal?: AbortSignal): Promise<SkillCatalogEntry[]> {
    const result: SkillCatalogEntry[] = [];
    for (const source of this.sources.values()) {
      signal?.throwIfAborted();
      for (const path of await source.list(signal)) {
        if (result.length >= 1000)
          throw new Error("Skill catalog exceeds limit.");
        const text = await source.read(path, "SKILL.md", signal);
        const document = parseSkillDocument(text);
        result.push({
          id: `${source.id}:${path}`,
          revision: await skillContentHash(text),
          sourceId: source.id,
          path,
          name: document.metadata.name,
          description: document.metadata.description,
          connectorRequirements: (
            document.metadata.metadata["requires-connectors"] ?? ""
          )
            .split(",")
            .map((id) => id.trim())
            .filter(Boolean),
        });
      }
    }
    if (new Set(result.map((entry) => entry.id)).size !== result.length)
      throw new Error("Duplicate skill identity.");
    return result;
  }
  async activate(
    selection: SkillSelection,
    authorizedConnectors: readonly string[],
    signal?: AbortSignal,
  ): Promise<ActiveSkill> {
    const entry = (await this.catalog(signal)).find(
      (item) => item.id === selection.id,
    );
    if (!entry || entry.revision !== selection.revision)
      throw new Error("Selected skill revision changed or disappeared.");
    if (
      entry.connectorRequirements.some(
        (id) => !authorizedConnectors.includes(id),
      )
    )
      throw new Error("Skill requires a selected authorized connector.");
    const source = this.sources.get(entry.sourceId)!;
    const text = await source.read(entry.path, "SKILL.md", signal);
    if ((await skillContentHash(text)) !== selection.revision)
      throw new Error("Skill changed during activation.");
    const document = parseSkillDocument(text);
    return {
      selection: structuredClone(selection),
      block: {
        id: `skill:${entry.id}@${entry.revision}`,
        title: `Skill: ${entry.name}`,
        content: document.instructions,
        priority: 100,
      },
      readReference: async (path, signal) => {
        signal?.throwIfAborted();
        if (
          !path ||
          path.includes("\\") ||
          path.startsWith("/") ||
          path
            .split("/")
            .some((segment) => segment === ".." || segment === "." || !segment)
        )
          throw new Error("Skill reference must remain inside its root.");
        const text = await source.read(entry.path, path, signal);
        if (new TextEncoder().encode(text).length > 1_000_000)
          throw new Error("Skill reference exceeds byte limit.");
        const revision = await skillContentHash(text);
        await this.recordResource?.({ skill: selection, path, revision });
        return { text, revision, source: `${entry.id}/${path}` };
      },
    };
  }
}
/** Application-owned bundles provide portable sources without filesystem access. */
export function bundledSkillSource(
  id: string,
  skills: Record<string, Record<string, string>>,
): SkillSource {
  const bundle = structuredClone(skills);
  return {
    id,
    list: async () => Object.keys(bundle),
    read: async (skill, path) => {
      if (!Object.hasOwn(bundle, skill) || !Object.hasOwn(bundle[skill]!, path))
        throw new Error("Skill resource not found.");
      return bundle[skill]![path]!;
    },
  };
}
