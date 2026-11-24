//! Approval-bypass-immune protection for metadata and shell startup files.

use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use std::path::Path;
use std::path::PathBuf;

const PROTECTED_PATH_REDLINE_PREFIX: &str = "Blocked by the protected-path redline";

const SHELL_STARTUP_FILES: &[&str] = &[
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".zshrc",
    ".zprofile",
    ".zshenv",
    ".config/fish/config.fish",
    "microsoft.powershell_profile.ps1",
    "profile.ps1",
];

pub(crate) fn enforce_protected_path_redline(
    invocation: &ToolInvocation,
) -> Result<(), FunctionCallError> {
    let codex_home = invocation.turn.config.codex_home.as_path();
    let Some(cwd) = invocation
        .step_context
        .environments
        .primary()
        .or_else(|| invocation.turn.initial_environments.primary())
        .and_then(|environment| environment.cwd().to_abs_path().ok())
    else {
        return Ok(());
    };
    let cwd = cwd.as_path();
    let Some(payload) = protected_write_payload(invocation) else {
        return Ok(());
    };
    let Some(target) = payload
        .as_shell_command()
        .and_then(|command| protected_target_in_shell_command(command, cwd, codex_home))
        .or_else(|| {
            payload
                .as_apply_patch()
                .and_then(|patch| protected_target_in_apply_patch(patch, cwd, codex_home))
        })
    else {
        return Ok(());
    };

    Err(FunctionCallError::RespondToModel(format!(
        "{PROTECTED_PATH_REDLINE_PREFIX}: `{target}` is protected in every approval mode. \
         Do not retry this write. Ask the user to make the change manually, or choose a path outside \
         `.git/hooks`, `.git/config`, Codex configuration/auth files, and shell startup files."
    )))
}

enum ProtectedWritePayload {
    ShellCommand(String),
    ApplyPatch(String),
}

impl ProtectedWritePayload {
    fn as_shell_command(&self) -> Option<&str> {
        match self {
            Self::ShellCommand(command) => Some(command.as_str()),
            Self::ApplyPatch(_) => None,
        }
    }

    fn as_apply_patch(&self) -> Option<&str> {
        match self {
            Self::ApplyPatch(patch) => Some(patch.as_str()),
            Self::ShellCommand(_) => None,
        }
    }
}

fn protected_write_payload(invocation: &ToolInvocation) -> Option<ProtectedWritePayload> {
    let tool_name = invocation.tool_name.name.as_str();
    let text = match &invocation.payload {
        ToolPayload::Function { arguments } => {
            let value = serde_json::from_str::<serde_json::Value>(arguments).ok()?;
            match tool_name {
                "exec_command" | "shell_command" => value
                    .get("cmd")
                    .or_else(|| value.get("command"))
                    .and_then(serde_json::Value::as_str)?,
                "write_stdin" => value.get("chars").and_then(serde_json::Value::as_str)?,
                "apply_patch" => value.get("patch").and_then(serde_json::Value::as_str)?,
                _ => return None,
            }
            .to_owned()
        }
        ToolPayload::Custom { input } => match tool_name {
            "exec_command" | "shell_command" | "write_stdin" | "apply_patch" => input.clone(),
            _ => return None,
        },
        ToolPayload::ToolSearch { .. } => return None,
    };

    match tool_name {
        "exec_command" | "shell_command" | "write_stdin" => {
            Some(ProtectedWritePayload::ShellCommand(text))
        }
        "apply_patch" => Some(ProtectedWritePayload::ApplyPatch(text)),
        _ => None,
    }
}

fn protected_target_in_shell_command(
    command: &str,
    cwd: &Path,
    codex_home: &Path,
) -> Option<String> {
    if command.contains("apply_patch")
        && let Some(target) = protected_target_in_apply_patch(command, cwd, codex_home)
    {
        return Some(target);
    }

    let mut current_dir = cwd.to_path_buf();
    for segment in shell_command_segments(command) {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }
        if let Some(target) = cd_target(segment) {
            current_dir = resolve_shell_path(&current_dir, target, codex_home);
        }
        if let Some(script) = nested_shell_script(segment)
            && let Some(target) =
                protected_target_in_shell_command(&script, &current_dir, codex_home)
        {
            return Some(target);
        }

        let redirection = redirection_target(segment);
        if let Some(target) = redirection
            && let Some(kind) = protected_path_kind(
                &resolve_shell_path(&current_dir, target, codex_home),
                codex_home,
            )
        {
            return Some(kind);
        }

        if segment_writes(segment) {
            if writes_local_git_config(segment) {
                return Some(".git/config (local git config)".to_string());
            }
            if let Some(target) = protected_git_config_file(segment, &current_dir, codex_home) {
                return Some(target);
            }
            if write_command_inspects_arguments(segment)
                && let Some(target) = protected_path_in_text(segment, &current_dir, codex_home)
            {
                return Some(target);
            }
        }
    }
    None
}

fn write_command_inspects_arguments(segment: &str) -> bool {
    let words = shell_words(segment);
    let Some(command) = words.first().map(|word| command_name(word)) else {
        return false;
    };
    if is_command_wrapper(&command) {
        let rest = &words[1..];
        return rest
            .iter()
            .any(|word| is_direct_write_command(&command_name(word)))
            || (rest.iter().any(|word| command_name(word) == "sed")
                && rest.iter().any(|word| word.starts_with("-i")))
            || (rest.iter().any(|word| command_name(word) == "perl")
                && rest
                    .iter()
                    .any(|word| word.starts_with("-i") || word.starts_with("-pi")))
            || rest.iter().any(|word| command_name(word) == "find") && find_command_writes(rest);
    }
    match command.as_str() {
        "sed" => words.iter().any(|word| word.starts_with("-i")),
        "perl" => words
            .iter()
            .any(|word| word.starts_with("-i") || word.starts_with("-pi")),
        "find" => find_command_writes(&words),
        "git" => false,
        _ => is_direct_write_command(&command),
    }
}

fn protected_target_in_apply_patch(patch: &str, cwd: &Path, codex_home: &Path) -> Option<String> {
    for line in patch.lines() {
        let Some(path) = line
            .strip_prefix("*** Add File: ")
            .or_else(|| line.strip_prefix("*** Update File: "))
            .or_else(|| line.strip_prefix("*** Delete File: "))
            .or_else(|| line.strip_prefix("*** Move to: "))
        else {
            continue;
        };
        let path = path.trim();
        if path.is_empty() {
            continue;
        }
        if let Some(kind) = protected_path_kind(&resolve_path(cwd, path), codex_home) {
            return Some(kind);
        }
    }
    None
}

fn protected_path_in_text(text: &str, cwd: &Path, codex_home: &Path) -> Option<String> {
    for token in text.split_whitespace() {
        let token = token.trim_matches(['"', '\'', '(', ')', '`', ',']);
        if token.is_empty() {
            continue;
        }
        let token = token
            .strip_prefix("of=")
            .or_else(|| token.strip_prefix("--output="))
            .unwrap_or(token);
        if let Some(kind) =
            protected_path_kind(&resolve_shell_path(cwd, token, codex_home), codex_home)
        {
            return Some(kind);
        }
    }
    None
}

fn redirection_target(segment: &str) -> Option<&str> {
    let bytes = segment.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != b'>' {
            continue;
        }
        let target = segment[index + 1..].trim_start_matches(['>', '&', ' ']);
        let target = target
            .split([' ', '\t', ';', '&', '|'])
            .next()
            .unwrap_or_default()
            .trim_matches(['"', '\'']);
        if !target.is_empty() {
            return Some(target);
        }
    }
    None
}

fn segment_writes(segment: &str) -> bool {
    if redirection_target(segment).is_some() {
        return true;
    }
    let words = shell_words(segment);
    if words
        .iter()
        .any(|word| is_direct_write_command(&command_name(word)))
    {
        return true;
    }
    if words.iter().any(|word| command_name(word) == "sed")
        && words.iter().any(|word| word.starts_with("-i"))
    {
        return true;
    }
    if words.iter().any(|word| command_name(word) == "perl")
        && words
            .iter()
            .any(|word| word.starts_with("-i") || word.starts_with("-pi"))
    {
        return true;
    }
    if git_config_writes(segment) {
        return true;
    }
    words.iter().any(|word| command_name(word) == "find") && find_command_writes(&words)
}

fn shell_command_segments(command: &str) -> Vec<&str> {
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote = None;
    let mut escaped = false;

    for (index, byte) in command.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        match quote {
            Some(b'\'') => {
                if byte == b'\'' {
                    quote = None;
                }
                continue;
            }
            Some(b'"') => {
                if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    quote = None;
                }
                continue;
            }
            _ => {}
        }

        match byte {
            b'\'' | b'"' => quote = Some(byte),
            b'\\' => escaped = true,
            b'\n' | b';' | b'&' | b'|' => {
                segments.push(&command[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }

    segments.push(&command[start..]);
    segments
}

fn cd_target(segment: &str) -> Option<&str> {
    let mut words = segment.split_whitespace();
    match words.next()?.to_ascii_lowercase().as_str() {
        "cd" | "pushd" => words
            .find(|word| !word.starts_with('-'))
            .map(|target| target.trim_matches(['"', '\''])),
        _ => None,
    }
}

fn shell_words(command: &str) -> Vec<String> {
    shlex::split(command).unwrap_or_else(|| {
        command
            .split_whitespace()
            .map(|word| {
                word.trim_matches(['"', '\'', '(', ')', ';', '&', '|'])
                    .to_string()
            })
            .collect()
    })
}

fn command_name(word: &str) -> String {
    word.trim_matches(['"', '\'', '(', ')', ';', '&', '|'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase()
}

fn is_command_wrapper(command: &str) -> bool {
    matches!(
        command,
        "sudo" | "env" | "command" | "builtin" | "nohup" | "time" | "exec"
    )
}

fn nested_shell_script(segment: &str) -> Option<String> {
    let words = shell_words(segment);
    let first = words.first().map(|word| command_name(word))?;
    if !matches!(first.as_str(), "sh" | "bash" | "zsh" | "fish") {
        return None;
    }
    let command_flag = words
        .iter()
        .skip(1)
        .position(|word| word.starts_with('-') && word.contains('c'))?
        + 1;
    let script = words.get(command_flag + 1..)?.join(" ");
    (!script.is_empty()).then_some(script)
}

fn is_direct_write_command(command: &str) -> bool {
    matches!(
        command,
        "rm" | "rmdir"
            | "mv"
            | "cp"
            | "install"
            | "touch"
            | "truncate"
            | "dd"
            | "chmod"
            | "chown"
            | "ln"
            | "mkdir"
            | "tee"
            | "unlink"
            | "shred"
    )
}

fn find_command_writes(words: &[String]) -> bool {
    if words.iter().any(|word| word == "-delete") {
        return true;
    }
    for (index, word) in words.iter().enumerate() {
        if !matches!(word.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
            continue;
        }
        let action = words.get(index + 1).map(|word| command_name(word));
        if action.as_deref().is_some_and(is_direct_write_command)
            || matches!(
                action.as_deref(),
                Some("sh" | "bash" | "zsh" | "fish" | "perl" | "python" | "python3")
            )
        {
            return true;
        }
    }
    false
}

fn writes_local_git_config(segment: &str) -> bool {
    let Some(args) = git_config_args(segment) else {
        return false;
    };
    if git_config_is_read(&args) {
        return false;
    }
    if args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--global" | "--system" | "--file" | "-f" | "--blob"
        )
    }) {
        return false;
    }
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--add"
                | "--replace-all"
                | "--unset"
                | "--unset-all"
                | "--rename-section"
                | "--remove-section"
                | "--edit"
                | "-e"
                | "--local"
                | "--worktree"
        )
    }) || args.iter().filter(|arg| !arg.starts_with('-')).count() >= 2
}

fn git_config_writes(segment: &str) -> bool {
    let Some(args) = git_config_args(segment) else {
        return false;
    };
    if git_config_is_read(&args) {
        return false;
    }
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--add"
                | "--replace-all"
                | "--unset"
                | "--unset-all"
                | "--rename-section"
                | "--remove-section"
                | "--edit"
                | "-e"
                | "--file"
                | "-f"
                | "--local"
                | "--worktree"
        )
    }) || args.iter().filter(|arg| !arg.starts_with('-')).count() >= 2
}

fn git_config_args(segment: &str) -> Option<Vec<String>> {
    let words = shell_words(segment);
    let git_index = words.iter().position(|word| command_name(word) == "git")?;
    let config_index = words[git_index + 1..]
        .iter()
        .position(|word| word == "config")
        .map(|index| index + git_index + 1)?;
    Some(words[config_index + 1..].to_vec())
}

fn git_config_is_read(args: &[String]) -> bool {
    args.iter().any(|arg| {
        matches!(
            arg.as_str(),
            "--get"
                | "--get-all"
                | "--get-regexp"
                | "--list"
                | "-l"
                | "--show-origin"
                | "--show-scope"
                | "--name-only"
                | "--null"
        )
    })
}

fn protected_git_config_file(segment: &str, cwd: &Path, codex_home: &Path) -> Option<String> {
    let args = git_config_args(segment)?;
    for (index, arg) in args.iter().enumerate() {
        let path = if arg == "--file" || arg == "-f" {
            args.get(index + 1).map(String::as_str)
        } else {
            arg.strip_prefix("--file=")
        };
        if let Some(path) = path
            && let Some(kind) =
                protected_path_kind(&resolve_shell_path(cwd, path, codex_home), codex_home)
        {
            return Some(kind);
        }
    }
    None
}

fn resolve_path(cwd: &Path, path: &str) -> PathBuf {
    let path = Path::new(path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn resolve_shell_path(cwd: &Path, path: &str, codex_home: &Path) -> PathBuf {
    let home = codex_home.parent().unwrap_or(codex_home);
    let mut expanded = path.to_string();
    let replacements = [
        ("${CODEX_HOME}", codex_home.to_string_lossy().into_owned()),
        ("$CODEX_HOME", codex_home.to_string_lossy().into_owned()),
        ("${HOME}", home.to_string_lossy().into_owned()),
        ("$HOME", home.to_string_lossy().into_owned()),
        ("~", home.to_string_lossy().into_owned()),
    ];
    for (prefix, replacement) in replacements {
        if let Some(rest) = expanded.strip_prefix(prefix) {
            expanded = format!("{replacement}{rest}");
            break;
        }
    }
    resolve_path(cwd, &expanded)
}

fn protected_path_kind(path: &Path, codex_home: &Path) -> Option<String> {
    let normalized = normalize_path(path);
    let codex_home = normalize_path(codex_home);

    if normalized == ".git/config"
        || normalized.ends_with("/.git/config")
        || normalized == ".git/hooks"
        || normalized.contains("/.git/hooks/")
        || normalized.ends_with("/.git/hooks")
    {
        return Some(path.to_string_lossy().into_owned());
    }

    if normalized == codex_home
        || normalized.starts_with(&format!("{codex_home}/"))
        || normalized.contains("/.codex/")
    {
        let filename = normalized.rsplit('/').next().unwrap_or_default();
        if matches!(
            filename,
            "config.toml"
                | "auth.json"
                | "requirements.toml"
                | "managed_config.toml"
                | "hooks.json"
        ) {
            return Some(path.to_string_lossy().into_owned());
        }
    }

    let lower = normalized.to_ascii_lowercase();
    if SHELL_STARTUP_FILES
        .iter()
        .any(|name| lower == *name || lower.ends_with(&format!("/{name}")))
    {
        return Some(path.to_string_lossy().into_owned());
    }
    None
}

fn normalize_path(path: &Path) -> String {
    let mut lexical = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !lexical.pop() {
                    lexical.push(component.as_os_str());
                }
            }
            _ => lexical.push(component.as_os_str()),
        }
    }
    lexical
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
#[path = "protected_paths_tests.rs"]
mod tests;
