use super::*;
use std::io::Read;

struct StartupInputs {
    files: BindState,
    interpreters: Vec<Executable>,
    seen: HashSet<String>,
}

pub(super) fn check_script_environment(words: &[String]) -> FpResult<()> {
    for word in words {
        let Some((name, _)) = word.split_once('=') else {
            continue;
        };
        if matches!(
            name.to_ascii_lowercase().as_str(),
            "home"
                | "bash_env"
                | "env"
                | "bun_options"
                | "ld_preload"
                | "dyld_insert_libraries"
                | "npm_config_node_options"
                | "npm_config_script_shell"
                | "npm_config_userconfig"
                | "npm_config_globalconfig"
                | "npm_config_prefix"
        ) {
            return fail("package script startup environment override is unsupported");
        }
    }
    Ok(())
}

/// The launcher and its shebang interpreter have independent identities.
pub(super) fn bind(
    cwd: &str,
    dirs: &[String],
    env: &HashMap<String, String>,
    executables: &[(Option<String>, Executable)],
    package_runner: bool,
    previous: &BindState,
) -> FpResult<String> {
    let mut inputs = StartupInputs {
        files: BindState::new(),
        interpreters: Vec::new(),
        seen: HashSet::new(),
    };
    for (index, (script, executable)) in executables.iter().enumerate() {
        inputs.bind_launcher(
            env,
            executable,
            script.is_some() || (package_runner && index > 0),
            0,
        )?;
    }
    let mut files = npm_configuration_files(cwd, dirs, env, executables, &inputs.interpreters);
    let mut checked = HashSet::new();
    while let Some(path) = files.pop() {
        if !checked.insert(path.clone()) {
            continue;
        }
        bind_file(&path, &mut inputs.files, false)?;
        files.extend(check_npm_configuration(&path)?);
    }
    check_other_manager_configuration(dirs, env, executables, &mut inputs.files)?;
    if inputs.files.count + previous.count > MAX_FILES
        || inputs.files.bytes + previous.bytes > MAX_TOTAL_BYTES
    {
        return fail("standing approval startup inputs exceed bounds");
    }
    Ok(format!(
        "{{\"interpreters\":[{}],\"files\":[{}],\"environment\":[{}]}}",
        inputs
            .interpreters
            .iter()
            .map(|found| found.json(None))
            .collect::<Vec<_>>()
            .join(","),
        inputs.files.entries.join(","),
        launcher_environment(env)?
    ))
}

impl StartupInputs {
    fn bind_launcher(
        &mut self,
        env: &HashMap<String, String>,
        launcher: &Executable,
        package_script: bool,
        depth: usize,
    ) -> FpResult<()> {
        if depth > 4 {
            return fail("launcher interpreter chain exceeds bounds");
        }
        if !self.seen.insert(launcher.canonical.clone()) {
            return Ok(());
        }
        let mut prefix = Vec::new();
        std::fs::File::open(&launcher.canonical)
            .map_err(FingerprintError::Io)?
            .take(4096)
            .read_to_end(&mut prefix)
            .map_err(FingerprintError::Io)?;
        if !prefix.starts_with(b"#!") {
            return Ok(());
        }
        bind_file(&launcher.path, &mut self.files, true)?;
        let interpreter = shebang_interpreter(
            &prefix,
            &launcher.cwd,
            env,
            package_script,
            &mut self.interpreters,
        )?;
        self.interpreters.push(interpreter.clone());
        self.bind_launcher(env, &interpreter, package_script, depth + 1)
    }
}

fn launcher_environment(env: &HashMap<String, String>) -> FpResult<String> {
    let mut environment: Vec<_> = env.iter().collect();
    environment.sort_by(|left, right| left.0.cmp(right.0));
    let mut entries = Vec::new();
    for (name, value) in environment {
        let lower = name.to_ascii_lowercase().replace('-', "_");
        if matches!(
            lower.as_str(),
            "npm_config_node_options"
                | "npm_config_script_shell"
                | "bun_options"
                | "ld_preload"
                | "dyld_insert_libraries"
        ) && !value.is_empty()
        {
            return fail("package-manager startup configuration is unsupported");
        }
        entries.push(format!("[{},{}]", json_quote(name), json_quote(value)));
    }
    let serialized = entries.join(",");
    if serialized.len() > MAX_ENV_BYTES {
        return fail("package-manager environment exceeds bounds");
    }
    Ok(serialized)
}

fn shebang_interpreter(
    prefix: &[u8],
    cwd: &str,
    env: &HashMap<String, String>,
    package_script: bool,
    interpreters: &mut Vec<Executable>,
) -> FpResult<Executable> {
    let words = shebang_words(prefix)?;
    let Some(first) = words.first() else {
        return fail("launcher interpreter is missing");
    };
    let name = if executable_name(first) == "env" {
        if *first != "/usr/bin/env"
            || words.len() != 2
            || words[1].starts_with('-')
            || words[1].contains('=')
        {
            return fail("launcher env indirection is ambiguous");
        }
        interpreters.push(executable(first, cwd, env)?.ok_or_else(|| {
            FingerprintError::Message("launcher env interpreter is unresolved".into())
        })?);
        words[1]
    } else {
        if !first.starts_with('/') || words.len() != 1 {
            return fail("launcher interpreter flags are unsupported");
        }
        first
    };
    if package_script && !name.contains('/') {
        return script_runner(cwd, name, env);
    }
    executable(name, cwd, env)?
        .ok_or_else(|| FingerprintError::Message("launcher interpreter is unresolved".into()))
}

fn shebang_words(prefix: &[u8]) -> FpResult<Vec<&str>> {
    let end = prefix
        .iter()
        .position(|byte| *byte == b'\n')
        .ok_or_else(|| FingerprintError::Message("launcher shebang is incomplete".into()))?;
    let line = std::str::from_utf8(&prefix[2..end])
        .map_err(|_| FingerprintError::Message("launcher shebang is not UTF-8".into()))?;
    Ok(line.split_ascii_whitespace().collect())
}

fn npm_configuration_files(
    cwd: &str,
    dirs: &[String],
    env: &HashMap<String, String>,
    executables: &[(Option<String>, Executable)],
    interpreters: &[Executable],
) -> Vec<String> {
    let mut files: Vec<String> = dirs.iter().map(|dir| js_join(&[dir, ".npmrc"])).collect();
    if let Some(home) = env.get("HOME") {
        files.push(js_join(&[home, ".npmrc"]));
    }
    for (name, value) in env {
        match name.to_ascii_lowercase().as_str() {
            "npm_config_userconfig" | "npm_config_globalconfig" => {
                files.push(js_resolve(cwd, &[value]))
            }
            "npm_config_prefix" => files.push(js_resolve(cwd, &[value, "etc/npmrc"])),
            _ => {}
        }
    }
    for launcher in executables
        .iter()
        .map(|(_, found)| found)
        .chain(interpreters)
    {
        let name = executable_name(&launcher.path);
        if matches!(name.as_str(), "npm" | "npx") {
            files.push(js_resolve(&js_dirname(&launcher.canonical), &["../npmrc"]));
        }
        if matches!(name.as_str(), "node" | "nodejs") {
            files.push(js_resolve(
                &js_dirname(&launcher.canonical),
                &["../etc/npmrc"],
            ));
        }
    }
    files.sort();
    files.dedup();
    files
}

fn check_npm_configuration(path: &str) -> FpResult<Vec<String>> {
    let Some(contents) = configuration_text(path)? else {
        return Ok(Vec::new());
    };
    let mut selected = Vec::new();
    for line in contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with(['#', ';']))
    {
        if let Some(path) = npm_configuration_entry(line)? {
            selected.push(path);
        }
    }
    Ok(selected)
}

fn configuration_text(path: &str) -> FpResult<Option<String>> {
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if is_absent(&error) => return Ok(None),
        Err(error) => return Err(FingerprintError::Io(error)),
    };
    let mut contents = String::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_string(&mut contents)
        .map_err(FingerprintError::Io)?;
    if contents.len() as u64 > MAX_FILE_BYTES {
        return fail("package-manager configuration exceeds bounds");
    }
    Ok(Some(contents))
}

fn npm_configuration_entry(line: &str) -> FpResult<Option<String>> {
    let (key, value) = line.split_once('=').unwrap_or((line, ""));
    let key = key.trim().to_ascii_lowercase();
    if !key
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return fail("package-manager configuration keys are unresolved");
    }
    if key == "prefix" {
        let value = value.trim();
        if !value.starts_with('/')
            || value
                .chars()
                .any(|c| c.is_ascii_whitespace() || matches!(c, '$' | '\'' | '"'))
        {
            return fail("package-manager configuration indirection is ambiguous");
        }
        return Ok(Some(js_join(&[value, "etc/npmrc"])));
    }
    if matches!(
        key.as_str(),
        "node-options"
            | "script-shell"
            | "userconfig"
            | "globalconfig"
            | "onload-script"
            | "pnpmfile"
            | "global-pnpmfile"
            | "use-node-version"
    ) {
        return fail("package-manager startup configuration is unsupported");
    }
    Ok(None)
}

fn check_other_manager_configuration(
    dirs: &[String],
    env: &HashMap<String, String>,
    executables: &[(Option<String>, Executable)],
    state: &mut BindState,
) -> FpResult<()> {
    let managers: HashSet<_> = executables
        .iter()
        .map(|(_, found)| executable_name(&found.path))
        .collect();
    if managers
        .iter()
        .any(|name| matches!(name.as_str(), "yarn" | "pnpm" | "corepack"))
    {
        return fail("package-manager configuration resolution is unsupported");
    }
    if !managers.contains("bun") && !managers.contains("bunx") {
        return Ok(());
    }
    let mut directories = dirs.to_vec();
    if let Some(home) = env.get("HOME")
        && !directories.contains(home)
    {
        directories.push(home.clone());
    }
    for directory in directories {
        for name in ["bunfig.toml", ".bunfig.toml"] {
            let path = js_join(&[&directory, name]);
            bind_file(&path, state, false)?;
            let Some(contents) = configuration_text(&path)? else {
                continue;
            };
            if !safe_bun_configuration(&contents) {
                return fail("package-manager startup configuration is unsupported");
            }
        }
    }
    Ok(())
}

fn safe_bun_configuration(contents: &str) -> bool {
    let mut section = "";
    for line in contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
    {
        if matches!(line, "[install]" | "[test]") {
            section = line;
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return false;
        };
        match (section, key.trim()) {
            ("[install]", "linker") if matches!(value.trim(), "\"hoisted\"" | "\"isolated\"") => {}
            ("[test]", "timeout") if value.trim().parse::<u64>().is_ok() => {}
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::safe_bun_configuration;

    #[test]
    fn unknown_and_escaped_startup_configuration_requires_fresh_approval() {
        assert!(safe_bun_configuration(
            "[install]\nlinker = \"hoisted\"\n[test]\ntimeout = 30000\n"
        ));
        for text in [
            "preload = [\"./startup.ts\"]",
            "\"preload\" = [\"./startup.ts\"]",
            "\"\\u0070reload\" = [\"./startup.ts\"]",
            "[unknown]\nvalue = true",
        ] {
            assert!(!safe_bun_configuration(text));
        }
    }
}
