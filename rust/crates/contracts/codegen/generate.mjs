// Run with Bun from the repository root. TypeBox remains the wire source of truth.
import * as schemas from "../../../../packages/contracts/src/schemas.ts";
import {
  REST_ROUTES,
  REST_API_VERSION,
} from "../../../../packages/contracts/src/rest.ts";
import { CONTRACT_VERSION } from "../../../../packages/contracts/src/version.ts";
import { cpSync, mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const output = join(import.meta.dir, "../src");
mkdirSync(join(output, "generated"), { recursive: true });
const pascal = (value) =>
  value.replace(/(^|[._-])(\w)/g, (_, _sep, letter) => letter.toUpperCase());
const snake = (value) =>
  value.replace(/([a-z0-9])([A-Z])/g, "$1_$2").toLowerCase();
const clean = (value) => {
  if (Array.isArray(value)) return value.map(clean);
  if (value && typeof value === "object") {
    return Object.fromEntries(
      Object.entries(value)
        .filter(([key]) => !["description", "title", "$id"].includes(key))
        .sort(([a], [b]) => a.localeCompare(b))
        .map(([key, entry]) => [key, clean(entry)]),
    );
  }
  return value;
};
const signature = (schema) => JSON.stringify(clean(schema));
const entries = Object.entries(schemas).filter(([key]) =>
  key.endsWith("Schema"),
);
const names = new Map();
for (const [name, schema] of entries) {
  const key = signature(schema);
  if (!names.has(key)) names.set(key, name.slice(0, -6));
}
const files = [];
const derives = "#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]";
const serializeDerives = "#[derive(Clone, Debug, PartialEq, Serialize)]";
const reserved = new Set([
  "type",
  "ref",
  "enum",
  "const",
  "default",
  "match",
  "loop",
  "self",
  "super",
  "mod",
  "use",
  "in",
  "as",
]);
function generate(rootName, rootSchema) {
  const definitions = [];
  const local = new Map();
  function fieldType(schema, hint, force = false) {
    const key = signature(schema);
    if (!force && names.has(key)) return names.get(key);
    if (local.has(key)) return local.get(key);
    if (Object.hasOwn(schema, "const")) {
      local.set(key, hint);
      if (typeof schema.const === "number") {
        definitions.push(`crate::number_literal!(${hint}, ${schema.const});`);
      } else {
        definitions.push(
          `${derives}\npub enum ${hint} {\n    #[serde(rename = ${JSON.stringify(schema.const)})]\n    ${pascal(schema.const)},\n}\ncrate::impl_json_via_serde!(${hint});`,
        );
      }
      return hint;
    }
    if (schema.anyOf) {
      const arms = schema.anyOf;
      if (arms.length === 2 && arms.some((arm) => arm.type === "null")) {
        return `Nullable<${fieldType(
          arms.find((arm) => arm.type !== "null"),
          `${hint}Value`,
        )}>`;
      }
      local.set(key, hint);
      const literals = arms.every((arm) => typeof arm.const === "string");
      const unionTypes = [];
      const variants = arms.map((arm, index) => {
        if (literals)
          return `    #[serde(rename = ${JSON.stringify(arm.const)})]\n    ${pascal(arm.const)},`;
        const tag = arm.properties?.type?.const ?? arm.properties?.kind?.const;
        const label = tag
          ? pascal(tag.replace(/^run\./, ""))
          : arm.type === "string"
            ? "Text"
            : arm.type === "array"
              ? "Parts"
              : `Variant${index + 1}`;
        const memberType = fieldType(arm, hint + label);
        unionTypes.push({ label, memberType });
        return `    ${label}(${memberType}),`;
      });
      definitions.push(
        `${literals ? derives : serializeDerives}\n${literals ? "" : "#[serde(untagged)]\n"}pub enum ${hint} {\n${variants.join("\n")}\n}\n${literals ? `crate::impl_json_via_serde!(${hint});` : `crate::impl_json_deserialize!(${hint}, |value: Value| {\n${unionTypes.map(({ label, memberType }) => `    if let Ok(member) = <${memberType} as crate::JsonDeserialize>::from_json(value.clone()) { return Ok(Self::${label}(member)); }`).join("\n")}\n    Err(crate::json_error("invalid ${hint}"))\n});`}`,
      );
      return hint;
    }
    if (schema.type === "object") {
      if (!schema.properties)
        return schema.patternProperties
          ? `BTreeMap<String, ${fieldType(Object.values(schema.patternProperties)[0], `${hint}Entry`)}>`
          : "BTreeMap<String, Value>";
      local.set(key, hint);
      const required = new Set(schema.required ?? []);
      const decodedFields = [];
      const fields = Object.entries(schema.properties).map(([name, child]) => {
        const raw = snake(name);
        const rustName = reserved.has(raw) ? `${raw}_` : raw;
        const childType = fieldType(child, hint + pascal(name));
        const optional = !required.has(name);
        const numeric =
          ["number", "integer"].includes(child.type) &&
          !Object.hasOwn(child, "const");
        decodedFields.push(
          `        ${rustName}: crate::${optional ? "optional_field" : "required_field"}(&mut object, ${JSON.stringify(name)})?,`,
        );
        const serializer = numeric
          ? `, serialize_with = "crate::serialize_${optional ? "optional_" : ""}number"`
          : "";
        return `    #[serde(rename = ${JSON.stringify(name)}${optional ? ', default, skip_serializing_if = "Optional::is_absent"' : ""}${serializer})]\n    pub ${rustName}: ${optional ? `Optional<${childType}>` : childType},`;
      });
      if (schema.additionalProperties !== false)
        fields.push(
          "    #[serde(flatten)]\n    pub extra: BTreeMap<String, Value>,",
        );
      if (schema.additionalProperties !== false)
        decodedFields.push("        extra: object.into_iter().collect(),");
      definitions.push(
        `${serializeDerives}\n${schema.additionalProperties === false ? "#[serde(deny_unknown_fields)]\n" : ""}pub struct ${hint} {\n${fields.join("\n")}\n}\ncrate::impl_json_deserialize!(${hint}, |value: Value| {\n    let mut object = crate::json_object(value)?;\n    let output = Self {\n${decodedFields.join("\n")}\n    };\n${schema.additionalProperties === false ? "    crate::reject_extra_fields(&object)?;\n" : ""}    Ok(output)\n});`,
      );
      return hint;
    }
    if (schema.type === "array")
      return `Vec<${fieldType(schema.items, `${hint}Item`)}>`;
    return (
      {
        string: "String",
        number: "f64",
        integer: "f64",
        boolean: "bool",
        null: "()",
      }[schema.type] ?? "Value"
    );
  }
  const target = names.get(signature(rootSchema));
  if (target !== rootName)
    definitions.push(`pub type ${rootName} = ${target};`);
  else {
    const rendered = fieldType(rootSchema, rootName, true);
    if (rendered !== rootName)
      definitions.push(`pub type ${rootName} = ${rendered};`);
  }
  const filename = snake(rootName);
  const imports =
    "#![allow(unused_imports)]\nuse crate::*;\nuse serde::{Deserialize, Serialize};\nuse serde_json::Value;\nuse std::collections::BTreeMap;";
  writeFileSync(
    join(output, "generated", `${filename}.rs`),
    `// Generated from packages/contracts/src via codegen/generate.mjs.\n${imports}\n\n${definitions.join("\n\n")}\n`,
  );
  files.push(filename);
}
for (const [name, schema] of entries) generate(name.slice(0, -6), schema);
writeFileSync(
  join(output, "generated/mod.rs"),
  `${files.map((file) => `mod ${file};\npub use ${file}::*;`).join("\n")}\n`,
);
writeFileSync(
  join(output, "schema_bundle.json"),
  `${JSON.stringify(Object.fromEntries(entries))}\n`,
);
const routes = Object.entries(REST_ROUTES);
const bodies = Object.fromEntries(
  routes.map(([operation]) => [
    operation,
    {
      submitMessage: "SubmitMessageRequest",
      regenerateMessage: "RegenerateMessageRequest",
      approveProposal: "ProposalDecisionRequest",
      rejectProposal: "ProposalDecisionRequest",
      applyProposal: "ApplyProposalRequest",
      grantAllowance: "GrantAllowanceRequest",
    }[operation] ??
      (entries.some(([name]) => name === `${pascal(operation)}RequestSchema`)
        ? `${pascal(operation)}Request`
        : null),
  ]),
);
const routeLines = routes.map(
  ([operation, route]) =>
    `    RouteDef { operation: RestOperation::${pascal(operation)}, method: RestMethod::${pascal(route.method.toLowerCase())}, path: ${JSON.stringify(route.path)}, request_schema: ${bodies[operation] ? `Some("${bodies[operation]}Schema")` : "None"} },`,
);
const routeSource = `// Generated from packages/contracts/src via codegen/generate.mjs.\nuse serde::{Deserialize, Serialize};\n\npub const CONTRACT_VERSION: &str = ${JSON.stringify(CONTRACT_VERSION)};\npub const REST_API_VERSION: &str = ${JSON.stringify(REST_API_VERSION)};\n\n${derives}\npub enum RestMethod { Get, Post, Patch, Delete }\n\n#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]\n#[serde(rename_all = "camelCase")]\npub enum RestOperation {\n${routes.map(([op]) => `    ${pascal(op)},`).join("\n")}\n}\n\n#[derive(Clone, Debug, PartialEq)]\npub struct RouteDef {\n    pub operation: RestOperation,\n    pub method: RestMethod,\n    pub path: &'static str,\n    pub request_schema: Option<&'static str>,\n}\n\npub static REST_ROUTES: &[RouteDef] = &[\n${routeLines.join("\n")}\n];\n\nimpl RestOperation {\n    pub fn route(&self) -> &'static RouteDef {\n        REST_ROUTES.iter().find(|route| &route.operation == self).expect("all operations have a route")\n    }\n    pub fn as_str(&self) -> &'static str {\n        match self {\n${routes.map(([op]) => `            Self::${pascal(op)} => "${op}",`).join("\n")}\n        }\n    }\n    pub fn request_schema(&self) -> Option<&'static str> { self.route().request_schema }\n}\n\nimpl RestMethod {\n    pub fn as_str(&self) -> &'static str {\n        match self { Self::Get => "GET", Self::Post => "POST", Self::Patch => "PATCH", Self::Delete => "DELETE" }\n    }\n}\n\nimpl std::fmt::Display for RestOperation {\n    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.as_str()) }\n}\n\nimpl std::str::FromStr for RestOperation {\n    type Err = serde_json::Error;\n    fn from_str(value: &str) -> Result<Self, Self::Err> {\n        serde_json::from_value(serde_json::Value::String(value.to_owned()))\n    }\n}\n`;
writeFileSync(join(output, "routes.rs"), routeSource);
console.log(
  `Generated ${entries.length} schemas and ${routes.length} REST operations.`,
);
const roundtrips = `// Generated coverage for every exported TypeBox schema.\n#[test]\nfn all_generated_dtos_roundtrip() {\n${entries.map(([key]) => `    support::roundtrip::<${key.slice(0, -6)}>("${key}");`).join("\n")}\n}\n`;
writeFileSync(
  join(output, "../tests/support/generated_roundtrips.rs"),
  roundtrips,
);

const { spawnSync } = await import("node:child_process");
const format = spawnSync(
  "rustfmt",
  [
    "+1.97.0",
    "--edition",
    "2024",
    ...files.map((file) => join(output, "generated", `${file}.rs`)),
    join(output, "generated/mod.rs"),
    join(output, "routes.rs"),
    join(output, "../tests/support/generated_roundtrips.rs"),
  ],
  { stdio: "inherit" },
);
if (format.status !== 0) process.exit(format.status ?? 1);

cpSync(
  join(import.meta.dir, "../../../fixtures"),
  join(output, "../tests/fixtures"),
  { recursive: true },
);
