//! Thin Unix-socket client. Dependency wiring and wire contract: crates/local-api/README.md.
use std::{path::PathBuf, process::ExitCode};

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand, ValueEnum};
use futures_util::StreamExt;
use serde_json::{Map, Value, json};
use zeron_local_api::{Client, default_data_dir, discover_instances};

#[derive(Parser, Debug)]
#[command(name = if cfg!(feature = "dev") { "noches-dev" } else { "noches" }, version,
    about = "Control a running Zeron instance over its private Unix socket")]
struct Cli {
    /// Bypass manifest discovery. Health identity is still checked.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    /// Select a named instance when several are running.
    #[arg(long, global = true)]
    instance: Option<String>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Check transport health and print the selected instance identity.
    Status,
    #[command(subcommand)]
    Instance(InstanceCommand),
    #[command(subcommand)]
    Layout(LayoutCommand),
    #[command(subcommand)]
    Tab(TabCommand),
    #[command(subcommand)]
    Agent(AgentCommand),
    #[command(subcommand)]
    Agents(AgentsCommand),
    #[command(subcommand)]
    Chat(ChatCommand),
    #[command(subcommand)]
    Team(TeamCommand),
    #[command(subcommand)]
    CoordinationState(CoordinationCommand),
    /// Forward an extension verb without interpreting its domain parameters.
    Worktree(Extension),
    Workspace(Extension),
    Section(Extension),
}

#[derive(Subcommand, Debug)]
enum InstanceCommand {
    List,
    Current,
}

#[derive(Args, Debug, Default)]
struct Params {
    /// Additional parameters as a JSON object. Explicit flags take precedence.
    #[arg(long, default_value = "{}")]
    params: String,
}

impl Params {
    fn object(&self) -> Result<Map<String, Value>> {
        let value: Value = serde_json::from_str(&self.params).context("invalid --params JSON")?;
        value
            .as_object()
            .cloned()
            .context("--params must be a JSON object")
    }
}

#[derive(Subcommand, Debug)]
enum LayoutCommand {
    Views(Params),
    State(Params),
    Watch(Params),
    Move(Params),
    Stop(Params),
    Compose(Compose),
    Save(Artifact),
    Apply(Apply),
    List(Params),
    Delete(Artifact),
    Run(LayoutRun),
}

#[derive(Args, Debug)]
struct Compose {
    #[arg(long)]
    from_file: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
    /// Ask the UI to refresh the composition guard.
    #[arg(long)]
    refresh_guard: bool,
    #[command(flatten)]
    extra: Params,
}

#[derive(Args, Debug)]
struct Artifact {
    name: Option<String>,
    #[arg(long)]
    from_file: Option<PathBuf>,
    #[arg(long)]
    dry_run: bool,
    /// Let layout save replace an existing recipe (server-side overwrite).
    #[arg(long)]
    force: bool,
    #[command(flatten)]
    extra: Params,
}

#[derive(Args, Debug)]
struct Apply {
    #[command(flatten)]
    artifact: Artifact,
    /// Renderer for the applied recipe's new cells. The server requires chat
    /// or terminal; auto is not supported.
    #[arg(long, value_enum)]
    ui: Option<Ui>,
}

#[derive(Args, Debug)]
struct LayoutRun {
    #[command(flatten)]
    artifact: Artifact,
    #[arg(long)]
    count: Option<u64>,
    #[arg(long)]
    provider: Option<String>,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    prompt: Option<String>,
    #[arg(long, value_enum)]
    ui: Option<Ui>,
    #[arg(long, value_enum)]
    direction: Option<Direction>,
    #[arg(long)]
    cwd: Option<String>,
    #[arg(long)]
    workspace: Option<String>,
}

#[derive(Subcommand, Debug)]
enum TabCommand {
    Split(Split),
    SplitView(Split),
    Close(Target),
    Move(TabMove),
    Reorder(TabReorder),
}

#[derive(Args, Debug)]
struct TabMove {
    #[command(flatten)]
    target: Target,
    /// Destination view address, for example view:2 or active-view.
    #[arg(long)]
    view: String,
}

#[derive(Args, Debug)]
struct TabReorder {
    #[command(flatten)]
    target: Target,
    /// Destination view address, for example view:2 or active-view.
    #[arg(long)]
    view: String,
    /// Insert before this pane address's tab instead of appending.
    #[arg(long)]
    before: Option<String>,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

#[derive(ValueEnum, Debug, Clone, Copy)]
enum Ui {
    Chat,
    Terminal,
}

#[derive(Args, Debug)]
struct Split {
    /// Stable target string, passed unchanged to the UI.
    #[arg(long)]
    to: Option<String>,
    #[arg(long, value_enum)]
    direction: Option<Direction>,
    #[arg(long, value_enum)]
    ui: Option<Ui>,
    #[command(flatten)]
    extra: Params,
}

#[derive(Args, Debug)]
struct Target {
    /// Stable target string. The CLI never resolves labels or tab positions.
    #[arg(long)]
    to: String,
    #[command(flatten)]
    extra: Params,
}

#[derive(Args, Debug)]
struct Send {
    #[command(flatten)]
    target: Target,
    /// Message text, or supply message in --params.
    message: Option<String>,
    #[arg(long)]
    from_file: Option<PathBuf>,
    /// Ask the UI to queue the message instead of interrupting active work.
    #[arg(long)]
    queue: bool,
}

#[derive(Args, Debug)]
struct Wait {
    #[command(flatten)]
    target: Target,
    #[arg(long)]
    idle: bool,
    /// Domain wait timeout in seconds. The UI must reply within the bridge deadline.
    #[arg(long)]
    timeout: Option<u64>,
}

#[derive(Subcommand, Debug)]
enum AgentCommand {
    Send(Send),
    Read(Target),
    Wait(Wait),
    Subscribe(Target),
    Stop(Target),
    Interrupt(Target),
    ShouldStop(Target),
}

#[derive(Args, Debug)]
struct Label {
    #[command(flatten)]
    target: Target,
    label: String,
}

#[derive(Args, Debug)]
struct Group {
    #[command(flatten)]
    target: Target,
    group: String,
}

#[derive(Subcommand, Debug)]
enum AgentsCommand {
    List(Params),
    Label(Label),
    Group(Group),
}

#[derive(Subcommand, Debug)]
enum ChatCommand {
    New(Params),
    List(Params),
    Providers(Params),
    Select(Target),
    Close(Target),
}

#[derive(Args, Debug)]
struct TeamRun {
    #[arg(long)]
    from_file: Option<PathBuf>,
    #[command(flatten)]
    extra: Params,
}

#[derive(Args, Debug)]
struct TeamId {
    /// Stable team/run identifier, interpreted by the UI.
    id: String,
    #[command(flatten)]
    extra: Params,
}

#[derive(Subcommand, Debug)]
enum TeamCommand {
    Run(TeamRun),
    Watch(Params),
    Report(TeamReport),
    Status(TeamId),
    List(Params),
    Cancel(TeamId),
}

#[derive(Args, Debug)]
struct TeamReport {
    #[command(flatten)]
    team: TeamId,
    #[arg(long)]
    label: Option<String>,
    #[arg(long)]
    summary: Option<String>,
    #[arg(long)]
    result_file: Option<String>,
    #[arg(long)]
    report_capability: Option<String>,
    /// Read the capability from a 0600 file written by the app at team launch.
    #[arg(long = "capability-file")]
    capability_file: Option<PathBuf>,
}

#[derive(Args, Debug)]
struct StateKey {
    key: String,
    #[command(flatten)]
    extra: Params,
}

#[derive(Args, Debug)]
struct StateSet {
    #[command(flatten)]
    key: StateKey,
    /// JSON value, not a string encoded by the CLI.
    value: String,
    #[arg(long)]
    if_version: Option<u64>,
}

#[derive(Args, Debug)]
struct StateDelete {
    #[command(flatten)]
    key: StateKey,
    #[arg(long)]
    if_version: Option<u64>,
}

#[derive(Subcommand, Debug)]
enum CoordinationCommand {
    Get(StateKey),
    Set(StateSet),
    Delete(StateDelete),
    Watch(StateKey),
}

#[derive(Args, Debug)]
struct Extension {
    verb: String,
    /// Use an SSE subscription rather than a JSON reply.
    #[arg(long)]
    subscribe: bool,
    /// Stage a destructive request. A human still confirms it in the app.
    #[arg(long)]
    confirm: bool,
    #[command(flatten)]
    extra: Params,
}

struct Call {
    method: String,
    params: Value,
    subscribe: bool,
}

fn file_json(path: &PathBuf) -> Result<Value> {
    serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse JSON in {}", path.display()))
}

fn insert_string(params: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        params.insert(key.into(), Value::String(value));
    }
}

fn target(target: Target) -> Result<Map<String, Value>> {
    let mut params = target.extra.object()?;
    params.insert("to".into(), json!(target.to));
    Ok(params)
}

fn state_key(key: StateKey) -> Result<Map<String, Value>> {
    let mut params = key.extra.object()?;
    params.insert("key".into(), json!(key.key));
    Ok(params)
}

fn split_params(split: Split) -> Result<Map<String, Value>> {
    let mut p = split.extra.object()?;
    insert_string(&mut p, "to", split.to);
    if let Some(direction) = split.direction {
        p.insert(
            "direction".into(),
            json!(direction.to_possible_value().expect("direction").get_name()),
        );
    }
    if let Some(ui) = split.ui {
        p.insert(
            "ui".into(),
            json!(ui.to_possible_value().expect("ui").get_name()),
        );
    }
    Ok(p)
}

fn artifact_params(artifact: &Artifact) -> Result<Map<String, Value>> {
    let mut p = artifact.extra.object()?;
    insert_string(&mut p, "name", artifact.name.clone());
    if let Some(path) = &artifact.from_file {
        p.insert("composition".into(), file_json(path)?);
    }
    if artifact.dry_run {
        p.insert("dry_run".into(), json!(true));
    }
    if artifact.force {
        p.insert("overwrite".into(), json!(true));
    }
    Ok(p)
}

fn route(command: Command) -> Result<Call> {
    let mut subscribe = false;
    let (domain, verb, params) = match command {
        Command::Layout(command) => {
            let (verb, params) = match command {
                LayoutCommand::Views(p) => ("views", p.object()?),
                LayoutCommand::State(p) => ("state", p.object()?),
                LayoutCommand::Move(p) => ("move", p.object()?),
                LayoutCommand::Stop(p) => ("stop", p.object()?),
                LayoutCommand::Watch(p) => {
                    subscribe = true;
                    ("watch", p.object()?)
                }
                LayoutCommand::List(p) => ("list", p.object()?),
                LayoutCommand::Compose(c) => {
                    let mut p = c.extra.object()?;
                    if let Some(path) = c.from_file {
                        p.insert("composition".into(), file_json(&path)?);
                    }
                    if c.dry_run {
                        p.insert("dry_run".into(), json!(true));
                    }
                    if c.refresh_guard {
                        p.insert("refresh_guard".into(), json!(true));
                    }
                    ("compose", p)
                }
                LayoutCommand::Run(r) => {
                    let mut p = r.artifact.extra.object()?;
                    match r.artifact.name {
                        Some(into) if matches!(into.as_str(), "views" | "tabs" | "panes") => {
                            p.insert("into".into(), json!(into));
                        }
                        name => insert_string(&mut p, "name", name),
                    }
                    if let Some(path) = r.artifact.from_file {
                        p.insert("composition".into(), file_json(&path)?);
                    }
                    if r.artifact.dry_run {
                        p.insert("dry_run".into(), json!(true));
                    }
                    if let Some(count) = r.count {
                        p.insert("count".into(), json!(count));
                    }
                    if let Some(provider) = r.provider {
                        let config = p
                            .entry("config")
                            .or_insert_with(|| json!({}))
                            .as_object_mut()
                            .context("config must be an object")?;
                        config.insert("harness".into(), json!(provider));
                    }
                    insert_string(&mut p, "label", r.label);
                    insert_string(&mut p, "prompt", r.prompt);
                    insert_string(&mut p, "cwd", r.cwd);
                    insert_string(&mut p, "spaceId", r.workspace);
                    if let Some(ui) = r.ui {
                        p.insert(
                            "ui".into(),
                            json!(ui.to_possible_value().expect("ui").get_name()),
                        );
                    }
                    if let Some(direction) = r.direction {
                        p.insert(
                            "direction".into(),
                            json!(direction.to_possible_value().expect("direction").get_name()),
                        );
                    }
                    ("run", p)
                }
                command => {
                    let (verb, p) = match command {
                        LayoutCommand::Save(a) => ("save", artifact_params(&a)?),
                        LayoutCommand::Delete(a) => ("delete", artifact_params(&a)?),
                        LayoutCommand::Apply(a) => {
                            let mut p = artifact_params(&a.artifact)?;
                            if let Some(ui) = a.ui {
                                p.insert(
                                    "ui".into(),
                                    json!(ui.to_possible_value().expect("ui").get_name()),
                                );
                            }
                            ensure!(
                                p.get("ui").is_some_and(|ui| ui == "chat" || ui == "terminal"),
                                "layout apply requires --ui chat or --ui terminal; auto is not supported"
                            );
                            ("apply", p)
                        }
                        _ => unreachable!(),
                    };
                    (verb, p)
                }
            };
            ("layout", verb.to_owned(), params)
        }
        Command::Tab(command) => {
            let (verb, p) = match command {
                TabCommand::Split(s) => ("split", split_params(s)?),
                TabCommand::SplitView(s) => ("split-view", split_params(s)?),
                TabCommand::Close(t) => ("close", target(t)?),
                TabCommand::Move(m) => {
                    let mut p = m.target.extra.object()?;
                    p.insert("to".into(), json!(m.target.to));
                    p.insert("view".into(), json!(m.view));
                    ("move", p)
                }
                TabCommand::Reorder(r) => {
                    let mut p = r.target.extra.object()?;
                    p.insert("to".into(), json!(r.target.to));
                    p.insert("view".into(), json!(r.view));
                    insert_string(&mut p, "before", r.before);
                    ("reorder", p)
                }
            };
            ("tab", verb.to_owned(), p)
        }
        Command::Agent(command) => {
            let (verb, p) = match command {
                AgentCommand::Send(s) => {
                    ensure!(
                        s.message.is_none() || s.from_file.is_none(),
                        "message and --from-file are mutually exclusive"
                    );
                    let mut p = target(s.target)?;
                    let message = match s.from_file {
                        Some(path) => Some(
                            std::fs::read_to_string(&path)
                                .with_context(|| format!("read {}", path.display()))?,
                        ),
                        None => s.message,
                    };
                    insert_string(&mut p, "message", message);
                    if s.queue {
                        p.insert("queue".into(), json!(true));
                    }
                    ("send", p)
                }
                AgentCommand::Read(t) => ("read", target(t)?),
                AgentCommand::Wait(w) => {
                    let mut p = target(w.target)?;
                    if w.idle {
                        p.insert("idle".into(), json!(true));
                    }
                    if let Some(timeout) = w.timeout {
                        p.insert("timeout".into(), json!(timeout));
                    }
                    ("wait", p)
                }
                AgentCommand::Subscribe(t) => {
                    subscribe = true;
                    ("subscribe", target(t)?)
                }
                AgentCommand::Stop(t) => ("stop", target(t)?),
                AgentCommand::Interrupt(t) => ("interrupt", target(t)?),
                AgentCommand::ShouldStop(t) => ("should-stop", target(t)?),
            };
            ("agent", verb.to_owned(), p)
        }
        Command::Agents(command) => {
            let (verb, p) = match command {
                AgentsCommand::List(p) => ("list", p.object()?),
                AgentsCommand::Label(l) => {
                    let mut p = target(l.target)?;
                    p.insert("label".into(), json!(l.label));
                    ("label", p)
                }
                AgentsCommand::Group(g) => {
                    let mut p = target(g.target)?;
                    p.insert("group".into(), json!(g.group));
                    ("group", p)
                }
            };
            ("agents", verb.to_owned(), p)
        }
        Command::Chat(command) => {
            let (verb, p) = match command {
                ChatCommand::New(p) => ("new", p.object()?),
                ChatCommand::List(p) => ("list", p.object()?),
                ChatCommand::Providers(p) => ("providers", p.object()?),
                ChatCommand::Select(t) => ("select", target(t)?),
                ChatCommand::Close(t) => ("close", target(t)?),
            };
            ("chat", verb.to_owned(), p)
        }
        Command::Team(command) => {
            let (verb, p) = match command {
                TeamCommand::Run(r) => {
                    let mut p = r.extra.object()?;
                    if let Some(path) = r.from_file {
                        p.insert("spec".into(), file_json(&path)?);
                    }
                    ("run", p)
                }
                TeamCommand::List(p) => ("list", p.object()?),
                TeamCommand::Watch(p) => {
                    subscribe = true;
                    ("watch", p.object()?)
                }
                TeamCommand::Report(r) => {
                    ensure!(
                        r.capability_file.is_none() || r.report_capability.is_none(),
                        "--capability-file and --report-capability are mutually exclusive"
                    );
                    let capability = match r.capability_file {
                        Some(path) => {
                            let content = std::fs::read_to_string(&path)
                                .with_context(|| format!("read {}", path.display()))?;
                            let trimmed = content.trim();
                            ensure!(
                                !trimmed.is_empty(),
                                "capability file {} is empty",
                                path.display()
                            );
                            Some(trimmed.to_owned())
                        }
                        None => r.report_capability,
                    };
                    let mut p = r.team.extra.object()?;
                    p.insert("id".into(), json!(r.team.id));
                    insert_string(&mut p, "label", r.label);
                    insert_string(&mut p, "reportCapability", capability);
                    if r.summary.is_some() || r.result_file.is_some() {
                        let report = p
                            .entry("report")
                            .or_insert_with(|| json!({}))
                            .as_object_mut()
                            .context("report must be an object")?;
                        insert_string(report, "summary", r.summary);
                        insert_string(report, "result_file", r.result_file);
                    }
                    ("report", p)
                }
                command => {
                    let (verb, t) = match command {
                        TeamCommand::Status(t) => ("status", t),
                        TeamCommand::Cancel(t) => ("cancel", t),
                        _ => unreachable!(),
                    };
                    let mut p = t.extra.object()?;
                    p.insert("id".into(), json!(t.id));
                    (verb, p)
                }
            };
            ("team", verb.to_owned(), p)
        }
        Command::CoordinationState(command) => {
            let (verb, p) = match command {
                CoordinationCommand::Get(k) => ("get", state_key(k)?),
                CoordinationCommand::Watch(k) => {
                    subscribe = true;
                    ("watch", state_key(k)?)
                }
                CoordinationCommand::Set(s) => {
                    let mut p = state_key(s.key)?;
                    p.insert(
                        "value".into(),
                        serde_json::from_str(&s.value).context("value must be JSON")?,
                    );
                    if let Some(v) = s.if_version {
                        p.insert("if_version".into(), json!(v));
                    }
                    ("set", p)
                }
                CoordinationCommand::Delete(d) => {
                    let mut p = state_key(d.key)?;
                    if let Some(v) = d.if_version {
                        p.insert("if_version".into(), json!(v));
                    }
                    ("delete", p)
                }
            };
            ("coordination-state", verb.to_owned(), p)
        }
        command @ (Command::Worktree(_) | Command::Workspace(_) | Command::Section(_)) => {
            let (domain, e) = match command {
                Command::Worktree(e) => ("worktree", e),
                Command::Workspace(e) => ("workspace", e),
                Command::Section(e) => ("section", e),
                _ => unreachable!(),
            };
            subscribe = e.subscribe;
            let mut p = e.extra.object()?;
            if e.confirm {
                p.insert("confirm".into(), json!(true));
            }
            (domain, e.verb, p)
        }
        Command::Status | Command::Instance(_) => {
            anyhow::bail!("instance commands have no UI route")
        }
    };
    Ok(Call {
        method: format!("{domain}.{verb}"),
        params: Value::Object(params),
        subscribe,
    })
}

async fn run(cli: Cli) -> Result<()> {
    // With an override, neither HOME nor either variant's data directory is read.
    if matches!(cli.command, Command::Instance(InstanceCommand::List)) && cli.socket.is_none() {
        let instances = discover_instances(&default_data_dir(cfg!(feature = "dev"))?).await?;
        println!("{}", serde_json::to_string_pretty(&instances)?);
        return Ok(());
    }
    let data_dir = if cli.socket.is_some() {
        PathBuf::new()
    } else {
        default_data_dir(cfg!(feature = "dev"))?
    };
    let client =
        Client::discover(&data_dir, cli.socket.as_deref(), cli.instance.as_deref()).await?;
    if matches!(
        cli.command,
        Command::Status | Command::Instance(InstanceCommand::Current)
    ) {
        println!("{}", serde_json::to_string_pretty(&client.health().await?)?);
    } else if matches!(cli.command, Command::Instance(InstanceCommand::List)) {
        println!(
            "{}",
            json!([{"manifest": client.health().await?, "error": null}])
        );
    } else {
        let call = route(cli.command)?;
        if call.subscribe {
            let mut events = client.subscribe(&call.method, call.params).await?;
            loop {
                tokio::select! {
                    signal = tokio::signal::ctrl_c() => { signal?; break; },
                    event = events.next() => {
                        let Some(event) = event else { anyhow::bail!("subscription ended; resubscribe for a fresh snapshot"); };
                        let event = event?;
                        println!("{}", json!({"event": event.event, "id": event.id, "data": serde_json::from_str::<Value>(&event.data)?}));
                        use std::io::Write;
                        std::io::stdout().flush()?;
                        ensure!(event.event != "gap", "subscription lost events; resubscribe for a fresh snapshot");
                    }
                }
            }
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(&client.request(&call.method, call.params).await?)?
            );
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn command_tree_and_variant_identity() {
        Cli::command().debug_assert();
        assert_eq!(
            Cli::command().get_name(),
            if cfg!(feature = "dev") {
                "noches-dev"
            } else {
                "noches"
            }
        );
    }

    #[test]
    fn preserves_stable_target_and_json_cas_version() {
        let cli = Cli::try_parse_from([
            "noches",
            "agent",
            "send",
            "--to",
            "workspace:abc/tab:def/view:ghi",
            "hello",
        ])
        .unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "agent.send");
        assert_eq!(
            call.params,
            json!({"to":"workspace:abc/tab:def/view:ghi", "message":"hello"})
        );
        let cli = Cli::try_parse_from([
            "noches",
            "coordination-state",
            "set",
            "key",
            "{\"a\":1}",
            "--if-version",
            "0",
        ])
        .unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "coordination-state.set");
        assert_eq!(
            call.params,
            json!({"key":"key", "value":{"a":1}, "if_version":0})
        );
    }

    #[test]
    fn run_arrangements_named_recipes_and_report_flags_route_exactly() {
        let cli = Cli::try_parse_from([
            "noches",
            "layout",
            "run",
            "panes",
            "--provider",
            "pi",
            "--ui",
            "chat",
            "--count",
            "2",
            "--label",
            "review",
            "--prompt",
            "Check the change",
            "--direction",
            "right",
            "--cwd",
            "/repo",
            "--workspace",
            "workspace",
        ])
        .unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "layout.run");
        assert_eq!(
            call.params,
            json!({"into":"panes","config":{"harness":"pi"},"ui":"chat",
            "count":2,"label":"review","prompt":"Check the change","direction":"right",
            "cwd":"/repo","spaceId":"workspace"})
        );
        let cli =
            Cli::try_parse_from(["noches", "layout", "run", "review-layout", "--dry-run"]).unwrap();
        assert_eq!(
            route(cli.command).unwrap().params,
            json!({"name":"review-layout","dry_run":true})
        );
        let cli = Cli::try_parse_from([
            "noches",
            "team",
            "report",
            "team",
            "--label",
            "reviewer",
            "--summary",
            "Complete",
            "--result-file",
            "results/report.md",
            "--report-capability",
            "secret",
            "--params",
            r#"{"scope":{"workspace":"w","worktree":"/repo"}}"#,
        ])
        .unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "team.report");
        assert_eq!(
            call.params,
            json!({"id":"team","label":"reviewer","reportCapability":"secret",
            "report":{"summary":"Complete","result_file":"results/report.md"},
            "scope":{"workspace":"w","worktree":"/repo"}})
        );
    }

    #[test]
    fn subscription_and_split_flags_are_forwarded() {
        let cli = Cli::try_parse_from(["noches", "agent", "subscribe", "--to", "id"]).unwrap();
        assert!(route(cli.command).unwrap().subscribe);
        let cli = Cli::try_parse_from([
            "noches",
            "tab",
            "split-view",
            "--direction",
            "right",
            "--ui",
            "chat",
        ])
        .unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "tab.split-view");
        assert_eq!(call.params, json!({"direction":"right", "ui":"chat"}));
        let cli = Cli::try_parse_from([
            "noches",
            "agent",
            "wait",
            "--to",
            "id",
            "--idle",
            "--timeout",
            "12",
        ])
        .unwrap();
        assert_eq!(
            route(cli.command).unwrap().params,
            json!({"to":"id", "idle":true, "timeout":12})
        );
        let cli =
            Cli::try_parse_from(["noches", "agent", "send", "--to", "id", "--queue", "hello"])
                .unwrap();
        assert_eq!(
            route(cli.command).unwrap().params,
            json!({"to":"id", "queue":true, "message":"hello"})
        );
        assert!(
            Cli::try_parse_from(["noches", "tab", "split", "--direction", "horizontal"]).is_err()
        );
        assert!(Cli::try_parse_from(["noches", "tab", "split", "--ui"]).is_err());
    }

    #[test]
    fn extension_verbs_forward_confirm_only_when_requested() {
        let cli = Cli::try_parse_from([
            "noches",
            "workspace",
            "delete",
            "--confirm",
            "--params",
            r#"{"workspaceId":"w"}"#,
        ])
        .unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "workspace.delete");
        assert_eq!(call.params, json!({"workspaceId":"w", "confirm":true}));
        let cli = Cli::try_parse_from(["noches", "worktree", "list"]).unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "worktree.list");
        assert_eq!(call.params, json!({}));
    }

    #[test]
    fn ui_auto_is_gone_and_apply_requires_explicit_ui() {
        assert!(
            Cli::try_parse_from(["noches", "tab", "split", "--ui", "auto"]).is_err(),
            "auto is not a server-supported value"
        );
        let cli = Cli::try_parse_from(["noches", "layout", "apply", "recipe"]).unwrap();
        assert!(route(cli.command).is_err());
        let cli = Cli::try_parse_from(["noches", "layout", "apply", "recipe", "--ui", "chat"])
            .unwrap();
        assert_eq!(
            route(cli.command).unwrap().params,
            json!({"name":"recipe", "ui":"chat"})
        );
        let cli =
            Cli::try_parse_from(["noches", "layout", "save", "recipe", "--force"]).unwrap();
        assert_eq!(
            route(cli.command).unwrap().params,
            json!({"name":"recipe", "overwrite":true})
        );
    }

    #[test]
    fn tab_close_move_and_reorder_route_with_view_addresses() {
        let cli = Cli::try_parse_from(["noches", "tab", "close", "--to", "tab:2"]).unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "tab.close");
        assert_eq!(call.params, json!({"to":"tab:2"}));
        let cli =
            Cli::try_parse_from(["noches", "tab", "move", "--to", "tab:1", "--view", "view:2"])
                .unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "tab.move");
        assert_eq!(call.params, json!({"to":"tab:1", "view":"view:2"}));
        let cli = Cli::try_parse_from([
            "noches",
            "tab",
            "reorder",
            "--to",
            "tab:1",
            "--view",
            "active-view",
            "--before",
            "pane:1",
        ])
        .unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "tab.reorder");
        assert_eq!(
            call.params,
            json!({"to":"tab:1", "view":"active-view", "before":"pane:1"})
        );
    }

    #[test]
    fn report_capability_file_is_read_trimmed_and_mutually_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reviewer.capability");
        std::fs::write(&path, "capability-secret\n").unwrap();
        let cli = Cli::try_parse_from([
            "noches",
            "team",
            "report",
            "team",
            "--label",
            "reviewer",
            "--capability-file",
            path.to_str().unwrap(),
            "--params",
            r#"{"scope":{"workspace":"w","worktree":"/repo"}}"#,
        ])
        .unwrap();
        let call = route(cli.command).unwrap();
        assert_eq!(call.method, "team.report");
        assert_eq!(
            call.params["reportCapability"],
            json!("capability-secret"),
            "trailing newlines are trimmed"
        );
        let cli = Cli::try_parse_from([
            "noches",
            "team",
            "report",
            "team",
            "--capability-file",
            path.to_str().unwrap(),
            "--report-capability",
            "other",
        ])
        .unwrap();
        assert!(
            route(cli.command).is_err(),
            "the two capability flags are mutually exclusive"
        );
        let cli = Cli::try_parse_from([
            "noches",
            "team",
            "report",
            "team",
            "--capability-file",
            dir.path().join("missing").to_str().unwrap(),
        ])
        .unwrap();
        assert!(route(cli.command).is_err());
    }
}
