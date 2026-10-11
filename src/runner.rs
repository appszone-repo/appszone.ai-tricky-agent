use crate::config::{AgentCfg, Format, PromptVia, Rule};
use crate::rules::{self, Ev};
use std::{path::PathBuf, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::Command,
    sync::mpsc,
};

pub enum Msg {
    Event(Ev),
    Error(String),
    Done,
}

pub struct RunSpec {
    pub program: String,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub format: Format,
    pub rules: Vec<Rule>,
}

pub fn build_spec(
    agent: &AgentCfg,
    prompt: &str,
    native_session: Option<&str>,
    yolo: bool,
    agent_model: Option<&str>,
    cwd: PathBuf,
    timeout: Duration,
) -> RunSpec {
    let vars = [
        ("{prompt}", prompt),
        ("{session}", native_session.unwrap_or("")),
        ("{model}", agent_model.unwrap_or("")),
    ];
    let fill_all = |list: &[String]| list.iter().map(|t| fill(t, &vars)).collect::<Vec<_>>();

    let mut args = fill_all(&agent.args);
    if native_session.is_some() {
        args.extend(fill_all(&agent.resume_args));
    }
    if yolo {
        args.extend(fill_all(&agent.yolo_args));
    }
    if agent_model.is_some() {
        args.extend(fill_all(&agent.model_args));
    }
    RunSpec {
        program: agent.program().to_string(),
        args,
        stdin: (agent.prompt_via == PromptVia::Stdin).then(|| prompt.to_string()),
        cwd,
        timeout,
        format: agent.format,
        rules: agent.rules.clone(),
    }
}

/// Single-pass placeholder substitution, so a prompt that happens to contain
/// "{session}" is never expanded a second time.
fn fill(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(i) = rest.find('{') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        match vars.iter().find(|(k, _)| tail.starts_with(k)) {
            Some((k, v)) => {
                out.push_str(v);
                rest = &tail[k.len()..];
            }
            None => {
                out.push('{');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Run the agent and stream normalised messages into `tx`.
/// Stops (and kills the child) if the receiver goes away or the timeout hits.
pub async fn run(spec: RunSpec, tx: mpsc::Sender<Msg>) {
    let mut cmd = Command::new(&spec.program);
    cmd.args(&spec.args)
        .current_dir(&spec.cwd)
        .stdin(if spec.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(Msg::Error(format!("could not start `{}`: {e}", spec.program))).await;
            return;
        }
    };

    if let (Some(text), Some(mut stdin)) = (spec.stdin, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = stdin.write_all(text.as_bytes()).await;
        });
    }

    let stderr_task = child.stderr.take().map(|mut e| {
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = (&mut e).take(16 * 1024).read_to_end(&mut buf).await;
            // keep draining so the child never blocks on a full pipe
            let _ = tokio::io::copy(&mut e, &mut tokio::io::sink()).await;
            String::from_utf8_lossy(&buf).trim().to_string()
        })
    });

    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    // non-JSON stdout (usually an error message the agent printed) kept for diagnostics
    let mut leftover = String::new();
    let deadline = tokio::time::sleep(spec.timeout);
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                let _ = child.kill().await;
                let _ = tx.send(Msg::Error(format!("agent timed out after {}s", spec.timeout.as_secs()))).await;
                return;
            }
            _ = tx.closed() => {
                let _ = child.kill().await;
                return;
            }
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    let events = match spec.format {
                        Format::Ndjson => {
                            let events = rules::apply(&spec.rules, &line);
                            if events.is_empty() && !line.trim_start().starts_with('{') && leftover.len() < 2000 {
                                leftover.push_str(line.trim());
                                leftover.push('\n');
                            }
                            events
                        }
                        Format::Plain => vec![Ev::Text(format!("{line}\n"))],
                    };
                    for ev in events {
                        if tx.send(Msg::Event(ev)).await.is_err() {
                            let _ = child.kill().await;
                            return;
                        }
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = child.kill().await;
                    let _ = tx.send(Msg::Error(format!("failed reading agent output: {e}"))).await;
                    return;
                }
            }
        }
    }

    let status = child.wait().await;
    let stderr = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => String::new(),
    };
    match status {
        Ok(s) if s.success() => {
            let _ = tx.send(Msg::Done).await;
        }
        Ok(s) => {
            let note = if stderr.is_empty() { leftover.trim() } else { stderr.as_str() };
            let detail = if note.is_empty() { String::new() } else { format!(": {note}") };
            let _ = tx.send(Msg::Error(format!("agent exited with {s}{detail}"))).await;
        }
        Err(e) => {
            let _ = tx.send(Msg::Error(format!("failed waiting for agent: {e}"))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fill;

    #[test]
    fn fill_is_single_pass() {
        let vars = [("{prompt}", "say {session}"), ("{session}", "SID")];
        assert_eq!(fill("{prompt}", &vars), "say {session}");
        assert_eq!(fill("--resume={session}", &vars), "--resume=SID");
        assert_eq!(fill("{unknown} {", &vars), "{unknown} {");
    }
}
