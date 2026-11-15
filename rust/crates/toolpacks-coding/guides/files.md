---
version: 1
---
# files

Which tool when
- You know the path: {{tool:read_file}}. Read before you edit.
- You do not know the path: {{tool:find_files}} matches file names, {{tool:code_search}} matches file contents.
- A project you have not seen: {{tool:repo_map}} once, then {{tool:list_dir}} for one folder.
- Change part of a file: {{tool:edit_file}}. Create a file or replace all of it: {{tool:write_file}}.

Usual order: locate, read, edit, then check the result with the shell toolset (tests or build).

Example 1: change a constant
1. {{tool:code_search}} {"pattern": "MAX_RETRIES"}
2. {{tool:read_file}} {"file_path": "src/client.rs", "offset": 30, "limit": 60}
3. {{tool:edit_file}} {"file_path": "src/client.rs", "old_string": "MAX_RETRIES: u32 = 3", "new_string": "MAX_RETRIES: u32 = 5"}

Example 2: add a new file
1. {{tool:find_files}} {"pattern": "tests/*.rs"} to see how tests are named.
2. {{tool:write_file}} {"file_path": "tests/retry.rs", "content": "..."}

Common mistakes
- Editing a file you have not read: old_string then does not match.
- An old_string that matches in two places: add surrounding text, or set replace_all to true when you mean every one.
- Using {{tool:write_file}} on a file that exists to change one line: it replaces the whole file. Use {{tool:edit_file}}.
- Reading a huge file in one call: use offset and limit, and follow the continuation notice.
- Absolute or ../ paths: paths are relative to the workspace root.
