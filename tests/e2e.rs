//! End to end: the real `crack run` binary against the scripted mock provider
//! (examples/mock_provider.rs, served in-process). Each test gets its own home, config
//! and data directories, so nothing on the machine leaks in.
//!
//! Prompts script the mock: `steps: [["tool", {args}], ...]` makes one tool call per
//! model request, then an answer quoting the last result. A step that is itself a list
//! of calls makes them together.
#![cfg(all(unix, feature = "yolo"))]

#[allow(dead_code)]
#[path = "../examples/mock_provider.rs"]
mod mock;

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The mock's base URL; one server for every test in this binary.
fn mock_url() -> &'static str {
    static URL: OnceLock<String> = OnceLock::new();
    URL.get_or_init(|| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                    let _ = mock::serve(listener).await;
                })
        });
        format!("http://{addr}/v1")
    })
}

/// A scripted prompt.
fn steps(steps: Value) -> String {
    format!("steps: {steps}")
}

struct Env {
    root: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let root =
            std::env::temp_dir().join(format!("crack-e2e-{name}-{}-{nanos}", std::process::id()));
        let project = root.join("project");
        for dir in ["project", "home", "config", "data"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        let _ = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&project)
            .status();
        Env { root, project }
    }

    /// `crack run` in this environment (scripted with `prompt`).
    fn command(&self, prompt: &str, extra: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_crack"));
        command
            .arg("-C")
            .arg(&self.project)
            .args(["run", "-m", "custom/mock", "--format", "json"])
            .args(extra)
            .arg(prompt)
            .env("CRACK_BASE_URL", mock_url())
            .env("CRACK_DATA_DIR", self.root.join("data"))
            .env("CRACK_CONFIG_DIR", self.root.join("config"))
            .env("CRACK_DUMP_REQUESTS", self.root.join("requests"))
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("home/.config"))
            .env("XDG_DATA_HOME", self.root.join("home/.local/share"))
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .stdin(Stdio::null());
        command
    }

    /// Runs a prompt with every tool allowed; the JSON events it printed.
    fn run(&self, prompt: &str, extra: &[&str]) -> Vec<Value> {
        let mut args = vec!["--permission-mode", "auto"];
        args.extend_from_slice(extra);
        let output = self.command(prompt, &args).output().unwrap();
        let events = parse(&String::from_utf8_lossy(&output.stdout));
        assert!(
            output.status.success(),
            "crack run failed: {}\n{}",
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        );
        events
    }

    /// Every request crack sent the model, oldest first, as text.
    fn requests(&self) -> Vec<String> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.root.join("requests"))
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        files.sort();
        files
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .collect()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn parse(stdout: &str) -> Vec<Value> {
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

/// The (title, output) of each tool result.
fn results(events: &[Value]) -> Vec<(String, String)> {
    events
        .iter()
        .filter(|e| e["type"] == "tool_result")
        .map(|e| {
            (
                e["title"].as_str().unwrap_or_default().to_owned(),
                e["output"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn output_of<'a>(results: &'a [(String, String)], title: &str) -> &'a str {
    results
        .iter()
        .find(|(t, _)| t.contains(title))
        .map(|(_, o)| o.as_str())
        .unwrap_or_else(|| panic!("no result titled {title:?} in {results:#?}"))
}

#[test]
fn runs_tools_and_answers() {
    let env = Env::new("basic");
    let events = env.run(
        &steps(json!([["bash", {"command": "echo hello-from-bash"}]])),
        &[],
    );
    let results = results(&events);
    assert!(output_of(&results, "echo hello-from-bash").contains("hello-from-bash"));
    let done = events
        .iter()
        .find(|e| e["type"] == "done")
        .expect("a done event");
    assert!(
        done["result"]
            .as_str()
            .unwrap_or_default()
            .contains("hello-from-bash")
    );
}

#[test]
fn subagent_news_reaches_the_model_mid_turn() {
    let env = Env::new("news");
    let sub = steps(json!([["bash", {"command": "echo quick-result"}]]));
    env.run(
        &steps(json!([
            ["task", {"description": "quick job", "prompt": sub, "run_in_background": true}],
            ["bash", {"command": "sleep 3"}],
            ["bash", {"command": "echo main-continues"}]
        ])),
        &[],
    );
    // A main-agent request after the subagent finished carries its report.
    let news = env
        .requests()
        .iter()
        .filter(|r| !r.contains("You are a subagent"))
        .any(|r| r.contains("subagent_updates") && r.contains("quick-result"));
    assert!(news, "the report never reached the main agent mid-turn");
}

#[test]
fn headless_runs_wait_for_background_agents() {
    let env = Env::new("wait");
    let sub =
        steps(json!([["bash", {"command": "sleep 2"}], ["bash", {"command": "echo found-it"}]]));
    let started = Instant::now();
    env.run(
        &steps(json!([["task", {"description": "slow job", "prompt": sub, "run_in_background": true}]])),
        &[],
    );
    assert!(
        started.elapsed() >= Duration::from_secs(2),
        "exited before the agent finished"
    );
    let main_saw_it = env
        .requests()
        .iter()
        .filter(|r| !r.contains("You are a subagent"))
        .any(|r| r.contains("found-it"));
    assert!(
        main_saw_it,
        "the main agent never got the background report"
    );
}

#[test]
fn forks_continue_the_conversation() {
    let env = Env::new("fork");
    let branch = steps(json!([["bash", {"command": "echo forked"}]]));
    let events = env.run(
        &steps(json!([
            ["bash", {"command": "echo secret-context-123"}],
            ["task", {"description": "branch", "prompt": branch, "subagent_type": "fork"}]
        ])),
        &[],
    );
    assert!(output_of(&results(&events), "Task: branch").contains("forked"));
    let fork_saw_context = env
        .requests()
        .iter()
        .any(|r| r.contains("You are a fork") && r.contains("secret-context-123"));
    assert!(
        fork_saw_context,
        "the fork didn't start from the conversation"
    );
}

#[test]
fn siblings_message_each_other_with_a_stamped_sender() {
    let env = Env::new("siblings");
    let listen =
        steps(json!([["bash", {"command": "sleep 2"}], ["bash", {"command": "echo listened"}]]));
    let talk = steps(json!([["message_agent", {"to": "listener", "message": "hello sibling"}]]));
    let events = env.run(
        &steps(json!([
            ["task", {"description": "listener", "prompt": listen, "name": "listener", "run_in_background": true}],
            ["task", {"description": "talker", "prompt": talk}],
            ["wait_for_agents", {"timeout_seconds": 60}]
        ])),
        &[],
    );
    assert!(output_of(&results(&events), "Task: talker").contains("Sent to a1 (listener)"));
    // The listener got it, signed by crack with the talker's id (a2), not by the text.
    let stamped = env.requests().iter().any(|r| {
        r.contains(r#"message from=\"agent a2 (talker)\""#)
            || r.contains("message from=\\\"agent a2")
    });
    assert!(
        stamped,
        "the sibling message didn't arrive with a stamped sender"
    );
}

#[test]
fn a_host_program_answers_permission_prompts() {
    let env = Env::new("host");
    let prompt = steps(json!([
        ["bash", {"command": "touch made-a.txt"}],
        ["bash", {"command": "touch made-b.txt"}]
    ]));
    let mut child = env
        .command(
            &prompt,
            &["--sandbox", "off", "--permission-prompt", "stdio"],
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut asked = Vec::new();
    for line in stdout.lines() {
        let Ok(event) = serde_json::from_str::<Value>(&line.unwrap()) else {
            continue;
        };
        if event["type"] == "permission_request" {
            let command = event["input"]["command"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            let decision = if command.contains("made-a") {
                "allow"
            } else {
                "deny"
            };
            asked.push(command);
            let reply = json!({"type": "permission_response", "id": event["id"], "decision": decision, "message": "not this one"});
            writeln!(stdin, "{reply}").unwrap();
            stdin.flush().unwrap();
        }
    }
    assert!(child.wait().unwrap().success());
    assert_eq!(asked.len(), 2, "expected two prompts, got {asked:?}");
    assert!(
        env.project.join("made-a.txt").exists(),
        "the allowed command didn't run"
    );
    assert!(
        !env.project.join("made-b.txt").exists(),
        "the denied command ran"
    );
}

#[test]
fn teams_survive_a_restart_and_interrupted_agents_resume() {
    let env = Env::new("restore");
    let slow = steps(json!([
        ["bash", {"command": "echo step-one-done"}],
        ["bash", {"command": "sleep 60"}],
        ["bash", {"command": "echo never"}]
    ]));
    let prompt = steps(json!([
        ["task", {"description": "long job", "prompt": slow, "name": "worker", "run_in_background": true}],
        ["wait_for_agents", {"timeout_seconds": 120}]
    ]));
    // Start it, and kill crack once the agent is in its long command.
    let mut child = env
        .command(&prompt, &["--permission-mode", "auto"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    for line in stdout.lines() {
        let line = line.unwrap();
        // The agent's own status (the main agent's task call quotes the prompt too).
        if line.contains("subagent_status") && line.contains("sleep 60") {
            break;
        }
    }
    // Let the in-progress transcript and team reach the database.
    std::thread::sleep(Duration::from_millis(800));
    child.kill().unwrap();
    let _ = child.wait();

    // Reopen the session: the agent is back, interrupted; a message resumes it.
    let resume = steps(json!([["bash", {"command": "echo resumed-ok"}]]));
    let events = env.run(
        &steps(json!([
            ["agent_status", {}],
            ["send_message", {"to": "worker", "message": resume}],
            ["wait_for_agents", {"timeout_seconds": 60}]
        ])),
        &["-s", "last"],
    );
    let results = results(&events);
    assert!(
        output_of(&results, "Agents").contains("interrupted"),
        "{results:#?}"
    );
    assert!(output_of(&results, "Message").contains("Resumed a1 (worker)"));
    assert!(output_of(&results, "Waited").contains("resumed-ok"));
}

#[test]
fn hooks_see_tool_calls_and_can_block_them() {
    let env = Env::new("hooks");
    let log = env.root.join("hook.log");
    std::fs::write(
        env.root.join("config/config.toml"),
        format!(
            r#"
[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "cat >> {log}; echo >> {log}"

[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
if = "Bash(rm *)"
command = "echo 'no deleting in tests' >&2; exit 2"
"#,
            log = log.display()
        ),
    )
    .unwrap();
    std::fs::write(env.project.join("keep.txt"), "x").unwrap();
    let events = env.run(
        &steps(json!([
            ["bash", {"command": "echo allowed-call"}],
            ["bash", {"command": "rm keep.txt"}]
        ])),
        &[],
    );
    let results = results(&events);
    assert!(output_of(&results, "echo allowed-call").contains("allowed-call"));
    assert!(
        env.project.join("keep.txt").exists(),
        "the blocked command ran"
    );
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        logged.contains("echo allowed-call"),
        "the hook didn't see the call: {logged}"
    );
    let _ = Path::new(&log);
}
