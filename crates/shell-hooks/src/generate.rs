use crate::{Provider, Registration, receiver::validate_absolute};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// All files go into a new private directory below `parent_dir`.
pub struct GenerationOptions {
    pub parent_dir: PathBuf,
    pub hook_socket: PathBuf,
    /// An absolute Python 3 interpreter path. No `/usr/bin/env` shebang lookup.
    pub python: PathBuf,
    pub provider_executable: PathBuf,
    pub instructions: String,
    /// Provider-owned history location. No history is read or rewritten by generation.
    pub history_path: Option<PathBuf>,
    /// Explicit PATH repair, scoped to the child. Each directory must be absolute.
    pub path_prefix: Vec<PathBuf>,
    /// Optional transport replacement. Receives one envelope on stdin and context in env.
    /// Return zero only after acknowledgment. Two-second deadline. No shell command string.
    pub notifier_argv: Option<Vec<String>>,
}

/// Contains paths, not tokens. Kiro assets require explicit project deployment by the host.
#[derive(Debug)]
pub struct GeneratedHooks {
    pub directory: PathBuf,
    pub launcher: PathBuf,
    pub notifier: PathBuf,
    pub context_file: PathBuf,
    pub assets: Vec<PathBuf>,
    pub automatic_activation: bool,
    pub limitations: Vec<String>,
}

pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn path_text(path: &Path) -> Result<&str> {
    validate_absolute(path)?;
    path.to_str().context("path must be UTF-8")
}

#[cfg(unix)]
fn private_dir(path: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new().mode(0o700).create(path)?;
    Ok(())
}

#[cfg(not(unix))]
fn private_dir(_path: &Path) -> Result<()> {
    anyhow::bail!("hook generation requires Unix")
}

fn put(root: &Path, relative: &str, content: &str, executable: bool) -> Result<PathBuf> {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        // Root is fresh and private; all relative names are compile-time constants.
        fs::create_dir_all(parent)?;
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let mut file = options.open(&path)?;
    file.write_all(content.as_bytes())?;
    Ok(path)
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("JSON value")
}

pub fn generate(
    registration: &Registration,
    options: &GenerationOptions,
) -> Result<GeneratedHooks> {
    registration.binding.validate()?;
    ensure!(
        registration.token.len() == 64 && registration.token.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid hook token"
    );
    path_text(&options.parent_dir)?;
    ensure!(
        options.parent_dir.canonicalize()?.as_os_str() == options.parent_dir.as_os_str(),
        "asset parent must be canonical"
    );
    path_text(&options.hook_socket)?;
    for executable in [&options.python, &options.provider_executable] {
        path_text(executable)?;
        ensure!(
            executable.is_file(),
            "executable missing: {}",
            executable.display()
        );
    }
    if let Some(path) = &options.history_path {
        path_text(path)?;
    }
    for dir in &options.path_prefix {
        ensure!(!path_text(dir)?.contains(':'), "PATH entry contains colon");
        ensure!(
            dir.is_dir() && dir.canonicalize()?.as_os_str() == dir.as_os_str(),
            "PATH prefix must be a canonical directory"
        );
    }
    if let Some(argv) = &options.notifier_argv {
        ensure!(!argv.is_empty(), "notifier argv is empty");
        path_text(Path::new(&argv[0]))?;
        ensure!(
            argv.iter().all(|arg| !arg.contains('\0')),
            "NUL in notifier argument"
        );
    }
    ensure!(
        options.instructions.len() <= 65_536,
        "instructions too large"
    );
    let root = options
        .parent_dir
        .join(format!("noches-hooks-{}", uuid::Uuid::new_v4().simple()));
    private_dir(&root)?;
    let result = generate_inner(&root, registration, options);
    if result.is_err() {
        let _ = fs::remove_dir_all(&root);
    }
    result
}

fn generate_inner(
    root: &Path,
    registration: &Registration,
    options: &GenerationOptions,
) -> Result<GeneratedHooks> {
    let binding = &registration.binding;
    let provider = binding.provider;
    let notify = root.join("notify.sh");
    let relay = root.join("relay.py");
    let env_file = root.join("context.env");
    let context_file = root.join("context.json");
    let python = path_text(&options.python)?;
    let helper = format!(
        "{} {}",
        shell_quote(python),
        shell_quote(path_text(&relay)?)
    );
    let mut assets = Vec::new();
    let mut env = BTreeMap::<String, String>::new();
    for (key, value) in [
        ("NOCHES_TERMINAL_ID", binding.terminal_id.clone()),
        ("NOCHES_SESSION_ID", binding.session_id.clone()),
        (
            "NOCHES_WORKTREE_PATH",
            path_text(&binding.worktree_path)?.to_owned(),
        ),
        ("NOCHES_MANAGED_AGENT", provider.as_str().to_owned()),
        (
            "NOCHES_HOOK_SOCKET",
            path_text(&options.hook_socket)?.to_owned(),
        ),
        ("NOCHES_TOKEN", registration.token.clone()),
        ("NOCHES_HOOK_TOKEN", registration.token.clone()),
        ("NOCHES_HOOK_CONTEXT", path_text(&context_file)?.to_owned()),
        ("NOCHES_NOTIFY", path_text(&notify)?.to_owned()),
    ] {
        env.insert(key.to_owned(), value);
    }
    let prefix = format!("NOCHES_{}", provider.as_str().to_ascii_uppercase());
    env.insert(
        format!("{prefix}_SESSION_INSTRUCTIONS"),
        root.join("instructions.md").to_string_lossy().into_owned(),
    );
    env.insert(
        format!("{prefix}_HISTORY_PATH"),
        options
            .history_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    env.insert(
        format!("{prefix}_SESSION_FILE"),
        options
            .history_path
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    env.insert(format!("{prefix}_NOTIFY"), path_text(&notify)?.to_owned());
    assets.push(put(root, "instructions.md", &options.instructions, false)?);
    assets.push(put(
        root,
        "context.json",
        &pretty(&json!({
            "binding": binding, "token": registration.token, "socket": options.hook_socket,
            "notifier_argv": options.notifier_argv,
        })),
        false,
    )?);
    assets.push(put(
        root,
        "relay.py",
        include_str!("../assets/relay.py"),
        false,
    )?);
    private_dir(&root.join("queue"))?;
    private_dir(&root.join("errors"))?;

    let hook_command = |event: &str| {
        format!(
            "/bin/sh {} {}",
            shell_quote(path_text(&notify).expect("validated path")),
            shell_quote(event)
        )
    };
    let mut args = Vec::<String>::new();
    let mut limitations = vec![
        "No login rcfiles are sourced and direnv is not evaluated. Supply a trusted PATH prefix or resolve the interpreter explicitly.".to_owned(),
        "Events report observed lifecycle only; they do not grant permissions or prove task success. Same-user processes can read session credentials.".to_owned(),
    ];
    match provider {
        Provider::Claude => {
            let mut hooks = serde_json::Map::new();
            // Claude Code has no SubagentStart event; only SubagentStop exists.
            for event in [
                "SessionStart",
                "SessionEnd",
                "UserPromptSubmit",
                "Stop",
                "SubagentStop",
                "PermissionRequest",
                "Notification",
                "PreToolUse",
                "PostToolUse",
            ] {
                hooks.insert(event.to_owned(), json!([{ "hooks": [{"type": "command", "command": hook_command(event), "timeout": 30}] }]));
            }
            let settings = put(
                root,
                "claude-settings.json",
                &pretty(&json!({"hooks": hooks})),
                false,
            )?;
            args.extend(["--settings".into(), path_text(&settings)?.into()]);
            assets.push(settings);
            // Instructions are exposed as a path; no new argv may silently replace a user's prompt.
            limitations.push("Claude instructions/history env paths are host metadata, not automatic prompt injection or transcript relocation.".into());
            limitations.push("Claude notifications are observational. The queued relay does not block prompt admission, and Stop can be followed by another hook's automatic continuation. These wrappers do not provide a verified idle/input-freeze barrier for Chat/CLI handoff.".into());
        }
        Provider::Pi => {
            let extension = put(
                root,
                "pi-noches.ts",
                include_str!("../assets/pi-noches.ts"),
                false,
            )?;
            args.extend(["--extension".into(), path_text(&extension)?.into()]);
            assets.push(extension);
            limitations.push("Pi targets the installed Earendil extension API with agent_settled and ui_prompt_start/end. Older Pi builds need a separate adapter, not agent_end-as-idle.".into());
        }
        Provider::Codex => {
            let watcher = put(
                root,
                "codex-watcher.py",
                include_str!("../assets/codex-watcher.py"),
                false,
            )?;
            let notify_argv = json!([python, path_text(&watcher)?]);
            args.extend(["-c".into(), format!("notify={notify_argv}")]);
            assets.push(watcher);
            assets.push(put(
                root,
                "codex-config.toml",
                &format!("notify = {notify_argv}\n"),
                false,
            )?);
            limitations.push("Codex uses documented notify callbacks, not private rollout polling. Only agent-turn-complete is supported; prompt, permission and background events are unavailable.".into());
        }
        Provider::OpenCode => {
            assets.push(put(
                root,
                "opencode/plugins/noches.js",
                include_str!("../assets/opencode.js"),
                false,
            )?);
            assets.push(put(
                root,
                "opencode/opencode.json",
                &pretty(&json!({"$schema": "https://opencode.ai/config.json"})),
                false,
            )?);
            env.insert(
                "OPENCODE_CONFIG_DIR".into(),
                root.join("opencode").to_string_lossy().into_owned(),
            );
            limitations.push("OpenCode is not installed here. Plugin contracts were checked against official docs; runtime loading and multi-session server behavior remain unverified. A native session ID is required to filter events.".into());
        }
        Provider::Cursor => {
            assets.push(put(
                root,
                "cursor/.cursor-plugin/plugin.json",
                &pretty(&json!({"name": "noches-session", "hooks": "./hooks/hooks.json"})),
                false,
            )?);
            let mut hooks = serde_json::Map::new();
            for event in [
                "sessionStart",
                "sessionEnd",
                "beforeSubmitPrompt",
                "stop",
                "afterAgentResponse",
                "subagentStart",
                "subagentStop",
                "preToolUse",
                "postToolUse",
            ] {
                hooks.insert(
                    event.into(),
                    json!([{"command": hook_command(event), "timeout": 30}]),
                );
            }
            assets.push(put(
                root,
                "cursor/hooks/hooks.json",
                &pretty(&json!({"version": 1, "hooks": hooks})),
                false,
            )?);
            args.extend([
                "--plugin-dir".into(),
                root.join("cursor").to_string_lossy().into_owned(),
            ]);
            limitations.push("Cursor CLI --plugin-dir exists locally. The plugin/hook schema follows official docs; no authenticated agent run was performed. Pre-tool hooks are not permission requests.".into());
        }
        Provider::Kiro => {
            let mut legacy = serde_json::Map::new();
            for event in [
                "agentSpawn",
                "userPromptSubmit",
                "preToolUse",
                "postToolUse",
                "stop",
            ] {
                legacy.insert(event.into(), json!([{"command": hook_command(event)}]));
            }
            assets.push(put(
                root,
                "kiro-v2-agent.json",
                &pretty(&json!({"name": "noches-session", "hooks": legacy})),
                false,
            )?);
            let hooks: Vec<Value> = [
                "SessionStart",
                "UserPromptSubmit",
                "PreToolUse",
                "PostToolUse",
                "Stop",
            ]
            .iter()
            .map(|event| {
                json!({
                    "name": format!("noches-{event}"), "trigger": event,
                    "action": {"type": "command", "command": hook_command(event)}, "timeout": 30,
                })
            })
            .collect();
            assets.push(put(
                root,
                "kiro-v3-hooks.json",
                &pretty(&json!({"version": "v1", "hooks": hooks})),
                false,
            )?);
            limitations.push("Kiro is unavailable and current official pages describe both CLI 2 agent hooks and CLI 3 standalone hooks. Choose the matching asset after checking the installed major version. No safe per-session CLI hook-file override is verified; automatic launch is disabled. Host deployment must be explicit and must not overwrite project/user configuration.".into());
        }
    }
    let mut exports = String::from(
        "# Private session environment. Source only inside the generated child wrapper.\n",
    );
    for (key, value) in env {
        exports.push_str(&format!("export {key}={}\n", shell_quote(&value)));
    }
    if !options.path_prefix.is_empty() {
        let prefix = options
            .path_prefix
            .iter()
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>()
            .join(":");
        exports.push_str(&format!(
            "export PATH={}${{PATH:+:\"$PATH\"}}\n",
            shell_quote(&prefix)
        ));
    }
    assets.push(put(root, "context.env", &exports, false)?);
    // Insert quoted data once; recursive template replacement could reinterpret path text.
    let notify_script = format!(
        "#!/bin/sh\nset -u\numask 077\n. {}\nroot={}\nhelper() {{ {helper} \"$@\"; }}\n{}",
        shell_quote(path_text(&env_file)?),
        shell_quote(path_text(root)?),
        include_str!("../assets/notify.sh")
    );
    assets.push(put(root, "notify.sh", &notify_script, true)?);
    let automatic_activation = provider != Provider::Kiro
        && (provider != Provider::OpenCode || binding.provider_session_id.is_some());
    let launch = if !automatic_activation {
        "#!/bin/sh\nprintf '%s\\n' 'Hook activation requires host verification, native session binding or explicit project deployment. See GeneratedHooks.limitations.' >&2\nexit 78\n".to_owned()
    } else {
        format!(
            "#!/bin/sh\nset -eu\n. {}\ncd -- {}\nif [ -t 0 ] && [ -t 1 ]; then export NOCHES_INTERACTIVE=1; else export NOCHES_INTERACTIVE=0; fi\nexec {} {} \"$@\"\n",
            shell_quote(path_text(&env_file)?),
            shell_quote(path_text(&binding.worktree_path)?),
            shell_quote(path_text(&options.provider_executable)?),
            args.iter()
                .map(|arg| shell_quote(arg))
                .collect::<Vec<_>>()
                .join(" ")
        )
    };
    let launcher = put(root, "launch.sh", &launch, true)?;
    assets.push(launcher.clone());
    Ok(GeneratedHooks {
        directory: root.to_owned(),
        launcher,
        notifier: notify,
        context_file,
        assets,
        automatic_activation,
        limitations,
    })
}
