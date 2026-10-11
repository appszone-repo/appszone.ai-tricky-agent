mod api;
mod config;
mod reqlog;
mod rules;
mod runner;
mod sessions;

use clap::Parser;
use config::{Config, Overrides};
use rules::Ev;
use runner::Msg;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::ExitCode,
    time::Duration,
};
use tokio::sync::mpsc;

/// OpenAI-compatible API in front of local CLI coding agents.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Optional config file, merged over the built-in defaults [default: ./config.toml]
    #[arg(long, env = "TRICKY_CONFIG")]
    config: Option<PathBuf>,
    /// Address to listen on (overrides config)
    #[arg(long, env = "HOST")]
    host: Option<String>,
    /// Port to listen on (overrides config)
    #[arg(long, env = "PORT")]
    port: Option<u16>,
    /// Static secret clients must send as a Bearer token (overrides config).
    /// If none is set anywhere, a random one is generated and printed at start
    #[arg(long, env = "TRICKY_API_KEY", hide_env_values = true)]
    api_key: Option<String>,
    /// Write a commented config template to the config path and exit
    #[arg(long)]
    init: bool,
    /// Run a tiny test prompt through the named agent and exit
    #[arg(long, value_name = "AGENT")]
    check: Option<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), String> {
    let config_path = cli.config.clone().unwrap_or_else(|| PathBuf::from("config.toml"));

    if cli.init {
        if config_path.exists() {
            return Err(format!("{} already exists, not overwriting", config_path.display()));
        }
        std::fs::write(&config_path, config::commented_template()).map_err(|e| e.to_string())?;
        println!("wrote {}: every setting is commented out; uncomment what you want to change", config_path.display());
        return Ok(());
    }

    let mut overrides = Overrides { host: cli.host, port: cli.port, api_key: cli.api_key, generated_api_key: None };
    let mut cfg = Config::load(Some(&config_path), &overrides)?;
    // auth is always on: no configured key means a fresh random one for this run
    let key_generated = cfg.server.api_key.is_empty();
    if key_generated {
        overrides.generated_api_key = Some(format!("sk-ta-{}", uuid::Uuid::new_v4().simple()));
        cfg = Config::load(Some(&config_path), &overrides)?;
    }

    if let Some(name) = cli.check {
        return check(&cfg, &name).await;
    }

    let host = cfg.server.host.clone();
    std::fs::create_dir_all(&cfg.server.default_cwd)
        .map_err(|e| format!("cannot create the default working folder {}: {e}", cfg.server.default_cwd.display()))?;

    let addr = format!("{}:{}", host, cfg.server.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("cannot listen on {addr}: {e}"))?;

    println!("tricky-agent {} listening on http://{addr}", env!("CARGO_PKG_VERSION"));
    println!(
        "config: {} ({})",
        config_path.display(),
        if config_path.exists() { "merged over built-in defaults" } else { "not found, using built-in defaults" }
    );
    if key_generated {
        println!("key:    {}   (generated for this run; set TRICKY_API_KEY or server.api_key to keep it)", cfg.server.api_key);
    } else {
        println!("key:    configured ({} chars), send it as: Authorization: Bearer <key>", cfg.server.api_key.len());
    }
    println!("cwd:    {} (default working folder for agents)", cfg.server.default_cwd.display());
    println!("log:    GET /requests (add ?format=text for a terminal-friendly view)");
    for (name, agent) in &cfg.agents {
        let found = find_program(agent.program());
        println!(
            "agent:  {name:<8} {} {}",
            agent.program(),
            if found { "(found)" } else { "(NOT FOUND in PATH)" }
        );
    }

    let state = api::AppState::new(cfg, Some(config_path), overrides);
    tokio::select! {
        res = axum::serve(
            listener,
            api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
        ) => res.map_err(|e| e.to_string()),
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}

async fn check(cfg: &Config, name: &str) -> Result<(), String> {
    std::fs::create_dir_all(&cfg.server.default_cwd)
        .map_err(|e| format!("cannot create {}: {e}", cfg.server.default_cwd.display()))?;
    let agent = cfg.agents.get(name).ok_or_else(|| {
        format!("unknown agent {name:?}; configured: {}", cfg.agents.keys().cloned().collect::<Vec<_>>().join(", "))
    })?;
    println!("running {} with a test prompt...", agent.program());
    let spec = runner::build_spec(
        agent,
        "Reply with exactly the word: pong",
        None,
        cfg.server.default_yolo,
        None,
        cfg.server.default_cwd.clone(),
        Duration::from_secs(cfg.server.timeout_secs),
    );
    let (tx, mut rx) = mpsc::channel(64);
    tokio::spawn(runner::run(spec, tx));

    let (mut text, mut session) = (String::new(), None);
    while let Some(msg) = rx.recv().await {
        match msg {
            Msg::Event(Ev::Text(t)) => text.push_str(&t),
            Msg::Event(Ev::Session(s)) => session = Some(s),
            Msg::Event(Ev::Usage { prompt, completion }) => println!("usage:   {prompt} in / {completion} out"),
            Msg::Error(e) => return Err(e),
            Msg::Done => break,
        }
    }
    println!("session: {}", session.as_deref().unwrap_or("(none reported: resume will not work)"));
    println!("reply:   {}", text.trim());
    if text.trim().is_empty() {
        return Err("the agent produced no text: check `rules` for this agent".into());
    }
    println!("ok");
    Ok(())
}

fn find_program(program: &str) -> bool {
    let p = Path::new(program);
    if p.components().count() > 1 {
        return p.exists();
    }
    let Some(paths) = std::env::var_os("PATH") else { return false };
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or(".EXE;.CMD;.BAT".into())
            .split(';')
            .map(str::to_lowercase)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::split_paths(&paths).any(|dir| {
        exts.iter().any(|ext| dir.join(format!("{program}{ext}")).is_file())
            || dir.join(program).is_file()
    })
}
