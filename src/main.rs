use anyhow::{Context, Result, bail, ensure};
use herdr_marks::{
    api::{Client, Host, PLUGIN},
    model, popup,
    workflow::{self, Operation, Selection},
};
use serde_json::{Value, json};
use std::{env, path::PathBuf, time::Duration};

const HELP: &str = "herdr-marks — letter marks for Herdr\n\n\
Usage: herdr-marks set LETTER [--pane ID | --workspace ID]\n\
       herdr-marks jump LETTER\n\
       herdr-marks remove LETTER\n\
       herdr-marks back\n\
       herdr-marks list [--json]\n\
       herdr-marks sync\n\
       herdr-marks open pane|workspace|jump|remove|list\n\n\
Pane letters: a-z. Workspace letters: A-Z.\n\
Run inside Herdr. Uses HERDR_SOCKET_PATH and HERDR_PLUGIN_STATE_DIR.\n\
See README.md for sidebar configuration and keybindings.";

struct Config {
    client: Client,
    state: PathBuf,
    scope: String,
}

impl Config {
    fn load() -> Result<Self> {
        ensure!(
            env::var("HERDR_ENV").as_deref() == Ok("1"),
            "run this command inside Herdr"
        );
        let socket =
            PathBuf::from(env::var_os("HERDR_SOCKET_PATH").context("missing HERDR_SOCKET_PATH")?);
        let socket = socket.canonicalize().context("resolve Herdr socket")?;
        let scope = socket
            .to_str()
            .context("Herdr socket path is not UTF-8")?
            .to_owned();
        let state = if let Some(path) = env::var_os("HERDR_PLUGIN_STATE_DIR") {
            PathBuf::from(path)
        } else {
            let root = if let Some(path) = env::var_os("XDG_STATE_HOME") {
                PathBuf::from(path)
            } else {
                PathBuf::from(env::var_os("HOME").context("missing HOME")?).join(".local/state")
            };
            root.join("herdr/plugins").join(PLUGIN)
        };
        Ok(Self {
            client: Client {
                socket,
                timeout: Duration::from_secs(5),
            },
            state,
            scope,
        })
    }
}

fn selection() -> Result<Selection> {
    let context: Value = env::var("HERDR_PLUGIN_CONTEXT_JSON")
        .ok()
        .map(|s| serde_json::from_str(&s))
        .transpose()
        .context("invalid plugin invocation context")?
        .unwrap_or_else(|| json!({}));
    let pane_id = context
        .get("focused_pane_id")
        .or_else(|| context.get("pane_id"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| env::var("HERDR_PANE_ID").ok())
        .or_else(|| env::var("HERDR_ACTIVE_PANE_ID").ok());
    let workspace_id = context
        .get("workspace_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| env::var("HERDR_WORKSPACE_ID").ok())
        .or_else(|| env::var("HERDR_ACTIVE_WORKSPACE_ID").ok());
    Ok(Selection {
        pane_id,
        workspace_id,
    })
}

fn run(config: &Config, args: &[String]) -> Result<()> {
    let command = args
        .first()
        .context("missing command; use --help")?
        .as_str();
    if command == "prompt" {
        ensure!(args.len() == 1, "prompt takes no arguments");
        return popup::prompt(&config.client, &config.state, &config.scope);
    }
    if command == "open" {
        ensure!(
            args.len() == 2,
            "open requires pane|workspace|jump|remove|list"
        );
        return popup::open(&config.client, popup::Mode::parse(&args[1])?, &selection()?);
    }
    let mut output_json = false;
    let operation = match command {
        "set" => {
            ensure!(
                args.len() == 2 || args.len() == 4,
                "set LETTER [--pane ID | --workspace ID]"
            );
            let key = model::letter(&args[1])?;
            let mut selected = selection()?;
            if args.len() == 4 {
                match args[2].as_str() {
                    "--pane" if key.is_ascii_lowercase() => {
                        selected.pane_id = Some(args[3].clone())
                    }
                    "--workspace" if key.is_ascii_uppercase() => {
                        selected.workspace_id = Some(args[3].clone())
                    }
                    _ => bail!("use --pane with a-z, or --workspace with A-Z"),
                }
            }
            Operation::Set(key, selected)
        }
        "jump" | "remove" => {
            ensure!(args.len() == 2, "{command} requires one letter");
            let key = model::letter(&args[1])?;
            if command == "jump" {
                Operation::Jump(key)
            } else {
                Operation::Remove(key)
            }
        }
        "back" | "sync" => {
            ensure!(args.len() == 1, "{command} takes no arguments");
            if command == "back" {
                Operation::Back
            } else {
                Operation::Sync
            }
        }
        "list" => {
            ensure!(
                args.len() == 1 || (args.len() == 2 && args[1] == "--json"),
                "list [--json]"
            );
            output_json = args.len() == 2;
            Operation::List
        }
        _ => bail!("unknown command {command}; use --help"),
    };
    let outcome = workflow::run(
        &config.client,
        &config.state,
        &config.scope,
        config.client.timeout,
        operation,
    )?;
    if let Some(rows) = outcome.listing {
        if output_json {
            println!("{}", serde_json::to_string_pretty(&rows)?);
        } else if let Some(rows) = rows.as_array() {
            if rows.is_empty() {
                println!("No marks yet.");
            }
            for row in rows {
                println!(
                    "{} [{}] {}",
                    if row["reachable"] == true { " " } else { "!" },
                    row["letter"].as_str().unwrap_or("?"),
                    model::safe_text(row["label"].as_str().unwrap_or(""), 120)
                );
            }
        }
    }
    if let Some(message) = outcome.message {
        println!("{message}");
        if env::var_os("HERDR_PLUGIN_ACTION_ID").is_some() {
            let _ = config.client.request(
                "notification.show",
                json!({"title": "Marks", "body": message}),
            );
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || matches!(args[0].as_str(), "--help" | "-h") {
        println!("{HELP}");
        return;
    }
    if args == ["--version"] {
        println!("herdr-marks {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    let config = match Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("Marks: {error:#}");
            std::process::exit(1);
        }
    };
    if let Err(error) = run(&config, &args) {
        eprintln!("Marks: {error:#}");
        if env::var_os("HERDR_PLUGIN_ACTION_ID").is_some() || args[0] == "prompt" {
            let _ = config.client.request(
                "notification.show",
                json!({"title": "Marks", "body": format!("{error:#}")}),
            );
        }
        std::process::exit(1);
    }
}
