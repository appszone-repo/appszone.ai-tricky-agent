use serde::Deserialize;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub const DEFAULT_CONFIG: &str = include_str!("../default-config.toml");

/// Command-line / env values that win over any config file.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub api_key: Option<String>,
    /// Used only when no key is configured anywhere (the per-run generated key).
    pub generated_api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Config {
    pub server: ServerCfg,
    #[serde(default)]
    pub agents: BTreeMap<String, AgentCfg>,
}

#[derive(Debug, Deserialize)]
pub struct ServerCfg {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub api_key: String,
    pub timeout_secs: u64,
    pub session_ttl_secs: u64,
    /// Folder the agents work in when a request has no `cwd` (created if missing). `~` = home.
    pub default_cwd: PathBuf,
    #[serde(default)]
    pub workspace_roots: Vec<PathBuf>,
    pub default_yolo: bool,
    pub log_capacity: usize,
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Format {
    #[default]
    Ndjson,
    Plain,
}

#[derive(Debug, Deserialize, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PromptVia {
    #[default]
    Arg,
    Stdin,
}

#[derive(Debug, Deserialize, Clone, Default)]
pub struct Rule {
    #[serde(default)]
    pub when: BTreeMap<String, String>,
    #[serde(default)]
    pub require: Vec<String>,
    #[serde(default)]
    pub absent: Vec<String>,
    pub text: Option<String>,
    pub session: Option<String>,
    pub prompt_tokens: Option<String>,
    pub completion_tokens: Option<String>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AgentCfg {
    #[serde(default = "yes")]
    pub enabled: bool,
    pub command: String,
    #[serde(default)]
    pub windows_command: Option<String>,
    pub args: Vec<String>,
    #[serde(default)]
    pub prompt_via: PromptVia,
    #[serde(default)]
    pub resume_args: Vec<String>,
    #[serde(default)]
    pub yolo_args: Vec<String>,
    #[serde(default)]
    pub model_args: Vec<String>,
    /// Model names advertised by /v1/models (any name is still accepted and passed through).
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub format: Format,
    #[serde(default)]
    pub rules: Vec<Rule>,
}

fn yes() -> bool {
    true
}

impl AgentCfg {
    pub fn program(&self) -> &str {
        if cfg!(windows) {
            self.windows_command.as_deref().unwrap_or(&self.command)
        } else {
            &self.command
        }
    }
}

impl ServerCfg {
    /// Accepted API keys: `api_key` may hold several, separated by commas.
    pub fn keys(&self) -> Vec<&str> {
        self.api_key.split(',').map(str::trim).filter(|k| !k.is_empty()).collect()
    }
}

impl Config {
    /// Built-in defaults, with the optional user file merged on top.
    pub fn load(path: Option<&Path>, ov: &Overrides) -> Result<Config, String> {
        let mut table: toml::Table = DEFAULT_CONFIG
            .parse()
            .map_err(|e| format!("built-in defaults are invalid: {e}"))?;

        if let Some(p) = path {
            match std::fs::read_to_string(p) {
                Ok(text) => {
                    let user: toml::Table = text
                        .parse()
                        .map_err(|e| format!("{}: {e}", p.display()))?;
                    merge(&mut table, user);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", p.display())),
            }
        }

        let mut cfg: Config = toml::Value::Table(table)
            .try_into()
            .map_err(|e| format!("invalid configuration: {e}"))?;

        cfg.server.default_cwd = expand_home(&cfg.server.default_cwd)?;
        cfg.server.workspace_roots = cfg
            .server
            .workspace_roots
            .iter()
            .map(|r| expand_home(r))
            .collect::<Result<_, _>>()?;

        cfg.agents.retain(|_, a| a.enabled);
        for name in cfg.agents.keys() {
            if name.is_empty() || name.contains(':') || name.contains('/') {
                return Err(format!("agent name {name:?} must be non-empty and contain no ':' or '/'"));
            }
        }

        if let Some(h) = &ov.host {
            cfg.server.host = h.clone();
        }
        if let Some(p) = ov.port {
            cfg.server.port = p;
        }
        if let Some(k) = &ov.api_key {
            cfg.server.api_key = k.clone();
        }
        if cfg.server.api_key.is_empty()
            && let Some(k) = &ov.generated_api_key
        {
            cfg.server.api_key = k.clone();
        }
        Ok(cfg)
    }
}

/// `~` and `~/x` become the user's home directory (HOME, or USERPROFILE on Windows).
fn expand_home(p: &Path) -> Result<PathBuf, String> {
    let Ok(rest) = p.strip_prefix("~") else {
        return Ok(p.to_path_buf());
    };
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("cannot expand {} : HOME is not set", p.display()))?;
    Ok(PathBuf::from(home).join(rest))
}

fn merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match v {
            toml::Value::Table(o) => match base.get_mut(&k) {
                Some(toml::Value::Table(b)) => merge(b, o),
                _ => {
                    base.insert(k, toml::Value::Table(o));
                }
            },
            v => {
                base.insert(k, v);
            }
        }
    }
}

/// The default config with every line commented out, for `--init`.
pub fn commented_template() -> String {
    let mut out = String::new();
    for line in DEFAULT_CONFIG.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            out.push_str(line);
        } else {
            out.push_str("# ");
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_expanded_only_for_a_leading_tilde() {
        // SAFETY of the env change: this is the only test touching HOME
        unsafe { std::env::set_var("HOME", "/home/x") };
        assert_eq!(expand_home(Path::new("~/.tricky-agent/agent")).unwrap(), PathBuf::from("/home/x/.tricky-agent/agent"));
        assert_eq!(expand_home(Path::new("~")).unwrap(), PathBuf::from("/home/x"));
        assert_eq!(expand_home(Path::new("/srv/a~b")).unwrap(), PathBuf::from("/srv/a~b"));
        let cfg = Config::load(None, &Default::default()).unwrap();
        assert_eq!(cfg.server.default_cwd, PathBuf::from("/home/x/.tricky-agent/agent"));
    }
}
