//! `agalma designer` scoped entry point end-to-end tests (CP.6, `agalma-9m2`).
//!
//! Each test drives the real `agalma` binary or parses the CLI against a scratch
//! git + `bd` workspace under `target/test-runs/designer-*` (never `/tmp`):
//! `git init` + `bd init --skip-agents --skip-hooks`. The command fails closed
//! when the planner route is unavailable and obeys the stop policy before any
//! designer session is built. Scratch dirs auto-remove on success (kept on
//! failure for forensics).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agalma_conductor::cli::{Cli, Command as CliCommand};
use agalma_testkit::ScratchDir;
use clap::Parser;
use serde_json::Value;

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(&format!("designer-{name}"))
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).trim().to_string()
}

fn run_ok(program: &str, dir: &Path, args: &[&str]) {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} {args:?}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        stderr_of(&out)
    );
}

fn require_bd() {
    let ok = Command::new("bd")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(ok, "`bd` CLI is required for these tests but was not found");
}

/// An isolated git + bd workspace plus a conductor state dir.
struct Scratch {
    repo: PathBuf,
    state_dir: PathBuf,
    _dir: ScratchDir,
}

impl Scratch {
    fn new(name: &str) -> Self {
        require_bd();
        let dir = fresh(name);
        let repo = dir.join("repo");
        fs::create_dir_all(&repo).expect("repo dir");
        run_ok("git", &repo, &["init", "-q"]);
        run_ok("git", &repo, &["config", "user.email", "agalma@localhost"]);
        run_ok("git", &repo, &["config", "user.name", "Agalma"]);

        let out = Command::new("bd")
            .args([
                "init",
                "--prefix",
                "designer",
                "--non-interactive",
                "--skip-agents",
                "--skip-hooks",
            ])
            .current_dir(&repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd init");
        assert!(out.status.success(), "bd init: {}", stderr_of(&out));

        let state_dir = dir.join("state");
        fs::create_dir_all(&state_dir).expect("state dir");
        Scratch {
            repo,
            state_dir,
            _dir: dir,
        }
    }

    fn bd(&self, args: &[&str]) -> std::process::Output {
        Command::new("bd")
            .arg("--db")
            .arg(self.repo.join(".beads"))
            .args(args)
            .current_dir(&self.repo)
            .env("BD_NON_INTERACTIVE", "1")
            .output()
            .expect("spawn bd")
    }

    fn bd_ok(&self, args: &[&str]) -> String {
        let out = self.bd(args);
        assert!(out.status.success(), "bd {args:?}: {}", stderr_of(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn create_epic(&self, title: &str) -> String {
        let out = self.bd(&[
            "create",
            title,
            "-t",
            "epic",
            "-p",
            "2",
            "--description",
            "epic design",
            "--json",
        ]);
        assert!(out.status.success(), "bd create epic: {}", stderr_of(&out));
        let value: Value = serde_json::from_slice(&out.stdout).expect("epic json");
        value["id"].as_str().expect("epic id").to_string()
    }

    fn children(&self, epic: &str) -> Vec<Value> {
        let stdout = self.bd_ok(&["list", "--parent", epic, "--all", "--json", "--limit", "0"]);
        serde_json::from_str(&stdout).expect("children json")
    }
}

// ---------------------------------------------------------------------------

#[test]
fn designer_arg_and_help_wiring() {
    // In-process parse: --epic is required; repo/state-dir/model are flags.
    let cli = Cli::try_parse_from([
        "agalma",
        "designer",
        "--epic",
        "agalma-x",
        "--repo",
        "/r",
        "--model",
        "m",
    ])
    .expect("parse designer args");
    match cli.command {
        CliCommand::Designer(args) => {
            assert_eq!(args.epic, "agalma-x");
            assert_eq!(args.repo.as_deref(), Some(Path::new("/r")));
            assert_eq!(args.model.as_deref(), Some("m"));
        }
        _ => panic!("expected Designer command"),
    }

    // The binary advertises the subcommand and its `--epic` flag.
    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args(["designer", "--help"])
        .output()
        .expect("run agalma designer --help");
    assert!(out.status.success(), "help exit: {}", stderr_of(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("--epic"), "help lists --epic: {stdout}");
}

#[test]
fn designer_command_fails_closed_without_planner_route() {
    let scratch = Scratch::new("unavailable");
    let epic = scratch.create_epic("Blocked epic");

    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "designer",
            "--epic",
            &epic,
            "--repo",
            scratch.repo.to_str().expect("repo path"),
            "--state-dir",
            scratch.state_dir.to_str().expect("state path"),
        ])
        .env_remove("AGALMA_ALLOW_PAID")
        .env_remove("AGALMA_MODEL")
        .output()
        .expect("run agalma designer");
    assert!(
        !out.status.success(),
        "unavailable planner must fail closed; stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("planner route unavailable"),
        "clear reason: {stderr}"
    );
    assert!(
        stderr.contains("never downgrading"),
        "never downgrades: {stderr}"
    );
    assert!(
        scratch.children(&epic).is_empty(),
        "no children created without a planner route"
    );
}

#[test]
fn designer_command_stop_sentinel_suppresses_dispatch() {
    let scratch = Scratch::new("stopped");
    let epic = scratch.create_epic("Stopped epic");
    fs::write(scratch.state_dir.join("STOP"), b"stop").expect("write STOP");

    // `--model` override makes the planner gate available, so the only reason
    // dispatch stays suppressed is the stop policy.
    let out = Command::new(env!("CARGO_BIN_EXE_agalma"))
        .args([
            "designer",
            "--epic",
            &epic,
            "--repo",
            scratch.repo.to_str().expect("repo path"),
            "--state-dir",
            scratch.state_dir.to_str().expect("state path"),
            "--model",
            "test/override",
        ])
        .output()
        .expect("run agalma designer");
    assert!(!out.status.success(), "a stop must suppress the attempt");
    let stderr = stderr_of(&out);
    assert!(
        stderr.contains("stop policy active"),
        "clear reason: {stderr}"
    );
    assert!(
        !scratch.state_dir.join("sweep").exists(),
        "no designer scratch dir created while stopped"
    );
}
