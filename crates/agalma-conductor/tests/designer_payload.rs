//! Designer vendor-payload regression (`agalma-lve`).
//!
//! The bug: `LiveDesignerSession` handed the model the canonical role prompt
//! with literal `<epic-id>` placeholders and no repository identity, so two
//! epics received the same generic text. These tests capture the *actual text
//! posted to the vendor* (the `{"text": ...}` body of `POST
//! /api/session/<id>/prompt`) for two distinct crate/epic requests, in both
//! harness transports:
//!
//! - **in-process**: `OpenCodeHarness` driven directly;
//! - **process**: the real `agalma-harness-opencode-server` supervised through
//!   `ProcessBinding`.
//!
//! No vendor, no network, no paid call: a mock `SandboxApi` (hosted by an
//! `agalma-ipc` component server) starts a mock vendor HTTP server on the port
//! the adapter allocates, records the posted bodies, and reports the child
//! alive. Scratch state lives under `target/test-runs/dp-*` (a short prefix so
//! the mock sandbox socket path fits `SUN_LEN`; auto-removed on success, kept on
//! failure for forensics).

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agalma_conductor::harness_process::{resolve_server_binary, ProcessBinding, READY_TIMEOUT};
use agalma_ipc::Wire;
use agalma_conductor::sweep::{designer_prompt, RetryContext};
use agalma_contracts::harness::{
    CreateSessionRequest, HarnessApi, Limits, RunTurnRequest, StartAttemptRequest, StopEvidence,
};
use agalma_contracts::ids::{ArtifactRef, OperationId};
use agalma_contracts::sandbox::{LaunchSpec, SandboxApi, SandboxChild};
use agalma_contracts::ContractError;
use agalma_harness_opencode::{
    server_command, OpenCodeHarness, OpenCodeHarnessConfig, PROFILE_PATH_ENV, ROOT_ENV,
    SANDBOX_SOCKET_ENV,
};
use agalma_ipc::{register_sandbox, SandboxClient, Server, ServerInfo};
use agalma_testkit::ScratchDir;
use serde_json::Value;

const MODEL: &str = "role-default";
const FAKE_PROGRAM: &str = "/usr/bin/true";

// ===========================================================================
// Fixtures
// ===========================================================================

fn fresh(name: &str) -> (ScratchDir, PathBuf) {
    let dir = ScratchDir::new(&format!("dp-{name}"));
    let canonical = fs_canonical(dir.path());
    (dir, canonical)
}

fn fs_canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// One crate/epic request: the epic id, its repository, and (optionally) the
/// failed-decomposition feedback the prompt must carry.
struct Round {
    repo_name: &'static str,
    epic: &'static str,
    retry: Option<RetryContext>,
}

fn rounds() -> Vec<Round> {
    vec![
        Round {
            repo_name: "alpha",
            epic: "agalma-aaa",
            retry: None,
        },
        Round {
            repo_name: "beta",
            epic: "agalma-bbb",
            retry: Some(RetryContext {
                rejected_children: vec!["agalma-bbb.1".to_string()],
                rework_tasks: vec!["agalma-bbb-fix".to_string()],
                comments: vec!["decomp-rejected: count-budget".to_string()],
            }),
        },
    ]
}

/// Create a git workspace for a round and return `(repo, prompt_path, text)`.
fn round_fixture(dir: &Path, round: &Round) -> (PathBuf, PathBuf, String) {
    let repo = dir.join("repos").join(round.repo_name);
    std::fs::create_dir_all(&repo).expect("repo dir");
    let repo = fs_canonical(&repo);
    assert!(
        Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .expect("git init")
            .success(),
        "git init"
    );

    // Compose the exact prompt the production session hands the harness, then
    // write it where the adapter resolves `prompts_ref`.
    let text = designer_prompt(round.epic, &repo, round.retry.as_ref());
    let prompts = dir.join("prompts");
    std::fs::create_dir_all(&prompts).expect("prompts dir");
    let prompt_path = prompts.join(format!("designer-{}.md", round.epic));
    std::fs::write(&prompt_path, &text).expect("write prompt");
    (repo, fs_canonical(&prompt_path), text)
}

// ===========================================================================
// Mock vendor HTTP server (records the posted prompt bodies)
// ===========================================================================

#[derive(Clone, Debug)]
struct Posted {
    path: String,
    body: String,
}

static SESSION_SEQ: AtomicU32 = AtomicU32::new(0);

/// Start the mock sandbox component server on `socket`; it returns the shared
/// record log the mock vendor appends posted bodies to.
fn start_mock_sandbox(socket: &Path) -> Arc<Mutex<Vec<Posted>>> {
    let records: Arc<Mutex<Vec<Posted>>> = Arc::new(Mutex::new(Vec::new()));
    let mut server = Server::new(ServerInfo {
        api: "SandboxApi".to_string(),
        api_version: 0,
        impl_name: "mock-sandbox".to_string(),
        impl_version: "mock/v1".to_string(),
        capabilities: BTreeSet::new(),
    });
    register_sandbox(
        &mut server,
        MockSandbox {
            records: Arc::clone(&records),
        },
    );
    let listener = server.bind(socket).expect("bind mock sandbox");
    std::thread::spawn(move || {
        let _ = server.accept_loop(listener);
    });
    records
}

/// A sandbox that never launches a real process: it starts a mock vendor HTTP
/// server on the adapter-allocated port and reports the child alive.
struct MockSandbox {
    records: Arc<Mutex<Vec<Posted>>>,
}

impl SandboxApi for MockSandbox {
    fn launch(&mut self, spec: LaunchSpec) -> Result<SandboxChild, ContractError> {
        let port = port_from_args(&spec.args).ok_or_else(|| {
            ContractError::KnownFailure("mock sandbox: launch has no --port".to_string())
        })?;
        start_mock_vendor(port, Arc::clone(&self.records));
        Ok(SandboxChild {
            pid: std::process::id(),
            pgid: std::process::id(),
            attempt: spec.attempt,
            started_at_unix_ms: 0,
        })
    }

    fn alive(&self, _child: &SandboxChild) -> Result<bool, ContractError> {
        Ok(true)
    }

    fn terminate(&mut self, _child: &SandboxChild) -> Result<StopEvidence, ContractError> {
        Ok(StopEvidence {
            process_group_gone: true,
            detail: "mock sandbox".to_string(),
        })
    }
}

fn port_from_args(args: &[String]) -> Option<u16> {
    args.windows(2)
        .find(|window| window[0] == "--port")
        .and_then(|window| window[1].parse().ok())
}

/// Bind `port` and serve the minimal OpenCode HTTP surface the adapter uses.
fn start_mock_vendor(port: u16, records: Arc<Mutex<Vec<Posted>>>) {
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind mock vendor");
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let records = Arc::clone(&records);
            std::thread::spawn(move || {
                let _ = serve_vendor(stream, &records);
            });
        }
    });
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn serve_vendor(mut stream: TcpStream, records: &Arc<Mutex<Vec<Posted>>>) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 1 << 20 {
            return Ok(());
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let content_length: usize = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|value| value.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_length);

    let request_line = head.lines().next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("");
    let body_text = String::from_utf8_lossy(&body).into_owned();

    if method == "POST" && path.ends_with("/prompt") {
        records.lock().expect("records").push(Posted {
            path: path.to_string(),
            body: body_text,
        });
    }

    let response_body = if method == "POST" && path == "/api/session" {
        let id = format!(
            "ses_mock_{}",
            SESSION_SEQ.fetch_add(1, Ordering::SeqCst) + 1
        );
        format!("{{\"data\":{{\"id\":\"{id}\"}}}}")
    } else {
        "{}".to_string()
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()?;
    Ok(())
}

// ===========================================================================
// Driving the harness
// ===========================================================================

fn start_req(workspace: &Path) -> StartAttemptRequest {
    StartAttemptRequest {
        workspace: workspace.to_string_lossy().into_owned(),
        role: "designer".to_string(),
        genome_ref: ArtifactRef::derive("genome").to_string(),
        constraints_ref: ArtifactRef::derive("platform-constraints").to_string(),
        limits: Limits {
            wall_ms: 5_000,
            tokens: 0,
            cost_micros: 0,
        },
        operation_id: OperationId::new("op:designer-payload/1"),
    }
}

/// Run one designer session/turn to the vendor for `round` and return the
/// `{"text": ...}` body the adapter actually posted.
fn drive_round(
    harness: &mut dyn HarnessApi,
    repo: &Path,
    prompt_path: &Path,
    records: &Arc<Mutex<Vec<Posted>>>,
) -> String {
    let before = records.lock().expect("records").len();
    let attempt = harness
        .start_attempt(start_req(repo))
        .expect("start_attempt");
    let session = harness
        .create_session(CreateSessionRequest {
            role: "designer".to_string(),
            model: MODEL.to_string(),
            prompts_ref: prompt_path.to_string_lossy().into_owned(),
            handoff_refs: Vec::new(),
            tool_policy_ref: "artifact:tool-policy-designer".to_string(),
        })
        .expect("create_session");
    let _op = harness
        .run_turn(
            &session,
            RunTurnRequest {
                input_ref: ArtifactRef::derive("designer-turn"),
                bounded_turns: 1,
                deadline_ms: 5_000,
            },
        )
        .expect("run_turn");
    let _ = harness.close_session(&session);
    let evidence = harness.stop_attempt(&attempt).expect("stop_attempt");
    assert!(
        evidence.process_group_gone,
        "cleanup evidence: {evidence:?}"
    );

    let posted = records.lock().expect("records");
    assert_eq!(
        posted.len(),
        before + 1,
        "exactly one prompt posted per round"
    );
    let last = posted.last().expect("posted body");
    assert!(
        last.path.ends_with("/prompt"),
        "posted to the vendor prompt endpoint: {}",
        last.path
    );
    let body = last.body.clone();
    let value: Value = serde_json::from_str(&body).expect("prompt body is JSON");
    value
        .get("text")
        .and_then(Value::as_str)
        .expect("posted body carries `text`")
        .to_string()
}

/// Assert one captured vendor payload identifies exactly its own request.
fn assert_payload(text: &str, round: &Round, repo: &Path, other: &str) {
    assert!(
        text.contains(&format!("epic: {}", round.epic)),
        "payload must name the selected epic {}: {text}",
        round.epic
    );
    assert!(
        text.contains(&repo.display().to_string()),
        "payload must name the owning repository {}: {text}",
        repo.display()
    );
    assert!(
        !text.contains(other),
        "payload for {} leaked the other epic {other}: {text}",
        round.epic
    );
    assert!(
        !text.contains("bd show <epic-id>"),
        "no placeholder may stand in for the selected id: {text}"
    );
    // The canonical role instructions are preserved (not a model-specific copy).
    assert!(
        text.contains("You are the Agalma"),
        "canonical role text: {text}"
    );
    if let Some(retry) = &round.retry {
        assert!(
            text.contains("Prior decomposition feedback"),
            "retry block present: {text}"
        );
        for child in &retry.rejected_children {
            assert!(text.contains(child), "rejected child {child}: {text}");
        }
        for task in &retry.rework_tasks {
            assert!(text.contains(task), "rework task {task}: {text}");
        }
        for comment in &retry.comments {
            assert!(text.contains(comment), "comment {comment}: {text}");
        }
    } else {
        assert!(
            !text.contains("Prior decomposition feedback"),
            "no retry block on a clean first run: {text}"
        );
    }
}

fn harness_config(dir: &Path, attempt: &Path) -> OpenCodeHarnessConfig {
    let profile = dir.join("worker.sb");
    std::fs::write(&profile, "(version 1)\n(deny default)\n").expect("write profile");
    let mut config =
        OpenCodeHarnessConfig::new(profile, attempt, dir.join("protected"), dir.join("sock"));
    config.program = Some(PathBuf::from(FAKE_PROGRAM));
    config.extra_ro = Some(PathBuf::from("/usr/bin"));
    config
}

// ===========================================================================
// In-process transport
// ===========================================================================

#[test]
fn in_process_vendor_payload_identifies_epic_and_repo() {
    let (_guard, dir) = fresh("in-process");
    let sandbox_socket = dir.join("sandbox.sock");
    let records = start_mock_sandbox(&sandbox_socket);

    let sandbox = SandboxClient::connect(&sandbox_socket).expect("connect mock sandbox");
    let config = harness_config(&dir, &dir.join("attempt"));
    let mut harness = OpenCodeHarness::new(Box::new(sandbox), config);

    let all = rounds();
    let mut fixtures = Vec::new();
    for round in &all {
        fixtures.push(round_fixture(&dir, round));
    }
    for (index, round) in all.iter().enumerate() {
        let (repo, prompt_path, _) = &fixtures[index];
        let text = drive_round(&mut harness, repo, prompt_path, &records);
        let other = all[(index + 1) % all.len()].epic;
        assert_payload(&text, round, repo, other);
    }
}

// ===========================================================================
// Process transport (supervised agalma-harness-opencode-server)
// ===========================================================================

#[test]
fn process_vendor_payload_identifies_epic_and_repo() {
    let (_guard, dir) = fresh("process");
    let sandbox_socket = dir.join("sandbox.sock");
    let records = start_mock_sandbox(&sandbox_socket);

    let harness_socket = dir.join("harness.sock");
    let harness_root = dir.join("harness-root");
    let profile = dir.join("worker.sb");
    std::fs::write(&profile, "(version 1)\n(deny default)\n").expect("write profile");

    let spec = server_command()
        .arg("--socket")
        .arg(harness_socket.to_string_lossy().into_owned())
        .env(ROOT_ENV, harness_root.to_string_lossy().into_owned())
        .env(PROFILE_PATH_ENV, profile.to_string_lossy().into_owned())
        .env(
            SANDBOX_SOCKET_ENV,
            sandbox_socket.to_string_lossy().into_owned(),
        )
        .env("AGALMA_MODEL", MODEL)
        // The confined server never launches opencode for real here; the mock
        // sandbox ignores the program, but `resolve_program` still needs a path.
        .env("OPENCODE_BIN", FAKE_PROGRAM);
    let program = resolve_server_binary(&spec.program).expect("opencode server binary resolves");
    let mut binding = ProcessBinding::spawn(
        spec,
        program,
        harness_socket,
        1,
        MODEL,
        Wire::Connect,
        READY_TIMEOUT,
    )
    .expect("spawn opencode server");

    let all = rounds();
    let mut fixtures = Vec::new();
    for round in &all {
        fixtures.push(round_fixture(&dir, round));
    }
    for (index, round) in all.iter().enumerate() {
        let (repo, prompt_path, _) = &fixtures[index];
        let text = drive_round(&mut binding, repo, prompt_path, &records);
        let other = all[(index + 1) % all.len()].epic;
        assert_payload(&text, round, repo, other);
    }
}
