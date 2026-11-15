---
version: 1
---
# code_intel

Language-server tools: exact answers about code, read-only. Use them instead of text search when the question is about meaning (what is this symbol, who uses it).

Which tool when
- Where is this symbol defined: {{tool:lsp_definition}} at a file position.
- Who uses it: {{tool:lsp_references}} (include_declaration true to list the declaration too).
- Its type or documentation: {{tool:lsp_hover}}.
- Which concrete types implement a trait or interface: {{tool:lsp_implementation}}.
- You only know a name: {{tool:lsp_symbols}} with query, then use the position it returns.
- Does the code compile cleanly: {{tool:lsp_diagnostics}}, for one file or the whole workspace.

Positions are 1-based line and character in file_path.

Usual order: {{tool:lsp_symbols}} to find the symbol, {{tool:lsp_definition}} or {{tool:lsp_references}} from its position, edit with the files toolset, {{tool:lsp_diagnostics}} to check.

Example 1: who calls this function
1. {{tool:lsp_symbols}} {"query": "ConnectionPool"}
2. {{tool:lsp_references}} {"file_path": "src/pool.rs", "line": 12, "character": 8}

Example 2: check an edit
{{tool:lsp_diagnostics}} {"file_path": "src/lib.rs"}

Common mistakes
- Guessing line and character: take them from a search or read result.
- Using text search to find usages of a common name: references are exact, search is not.
- Expecting results at once after startup: a server may need a moment; try again once.
- Skipping diagnostics after an edit.
