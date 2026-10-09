//! Integration tests for the Seatbelt sandbox service.
//!
//! Run directories live under `target/test-runs/sandbox-*` (never `/tmp`), as
//! required by `docs/m0-skeleton.md` ("Test strategy and verification
//! commands"). Each test asserts one S0b boundary property:
//!
//! - writes are confined to the attempt dir; writes elsewhere are denied;
//! - a malformed/missing/unknown-param profile fails closed before any process
//!   runs (canary file stays absent), including `sandbox-exec` rc 65;
//! - termination kills the whole descendant tree and returns evidence.

#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use agalma_contracts::error::ContractError;
use agalma_contracts::ids::AttemptId;
use agalma_contracts::sandbox::{LaunchSpec, SandboxApi};
use agalma_sandbox::{SeatbeltSandbox, PRODUCT_PROFILE};

/// Per-test scratch tree under `target/test-runs/`.
struct Run {
    root: PathBuf,
    attempt: PathBuf,
    protected: PathBuf,
    sock: PathBuf,
    extra_ro: PathBuf,
}

fn new_run(test: &str) -> Run {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let base = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("test-runs");
    let root = base.join(format!("sandbox-{test}-{}-{nanos}", std::process::id()));
    for dir in ["attempt", "protected", "sock", "extra"] {
        fs::create_dir_all(root.join(dir)).expect("create run dir");
    }
    let root = fs::canonicalize(&root).expect("canonicalize run dir");
    Run {
        attempt: root.join("attempt"),
        protected: root.join("protected"),
        sock: root.join("sock"),
        extra_ro: root.join("extra"),
        root,
    }
}

impl Run {
    fn path_str(&self, path: &Path) -> String {
        path.to_str().expect("utf8 path").to_string()
    }

    fn write_profile(&self, name: &str, content: &str) -> String {
        let path = self.root.join(name);
        fs::write(&path, content).expect("write profile");
        self.path_str(&path)
    }

    /// The complete child environment with attempt-scoped temp/XDG roots.
    fn env(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        env.insert(
            "PATH".to_string(),
            "/usr/bin:/bin:/usr/sbin:/sbin".to_string(),
        );
        for (key, sub) in [
            ("TMPDIR", "tmp"),
            ("HOME", "home"),
            ("XDG_CONFIG_HOME", "xdg-config"),
            ("XDG_DATA_HOME", "xdg-data"),
            ("XDG_CACHE_HOME", "xdg-cache"),
        ] {
            let dir = self.attempt.join(sub);
            fs::create_dir_all(&dir).expect("create attempt-scoped dir");
            env.insert(key.to_string(), self.path_str(&dir));
        }
        env
    }

    fn spec(&self, profile: &str) -> LaunchSpec {
        LaunchSpec {
            program: "/bin/sh".to_string(),
            args: Vec::new(),
            cwd: self.path_str(&self.attempt),
            env: self.env(),
            profile: profile.to_string(),
            attempt: AttemptId::derive(1),
            attempt_dir: self.path_str(&self.attempt),
            protected_dir: self.path_str(&self.protected),
            sock_dir: self.path_str(&self.sock),
            extra_ro_roots: vec![self.path_str(&self.extra_ro)],
        }
    }
}

/// Poll for `path` to exist within `timeout`.
fn wait_for(path: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if path.exists() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether `pid` names a live process (treating `EPERM` as alive).
fn pid_alive(pid: u32) -> bool {
    match Command::new("/bin/kill")
        .arg("-0")
        .arg(pid.to_string())
        .output()
    {
        Ok(out) if out.status.success() => true,
        Ok(out) => String::from_utf8_lossy(&out.stderr).contains("Operation not permitted"),
        Err(_) => false,
    }
}

#[tokio::test]
async fn write_inside_allowed_outside_denied() {
    let run = new_run("write-scope");
    let profile = run.write_profile("worker.sb", PRODUCT_PROFILE);
    let outside = run.root.join("escape.txt");
    let script = format!(
        "echo inside > {attempt}/inside.txt; \
         echo outside > {outside} 2>/dev/null; \
         echo done > {attempt}/marker",
        attempt = run.attempt.display(),
        outside = outside.display(),
    );

    let mut sandbox = SeatbeltSandbox::new();
    let mut spec = run.spec(&profile);
    spec.args = vec!["-c".to_string(), script];
    let child = sandbox.launch(spec).expect("confined launch");

    assert!(
        wait_for(&run.attempt.join("marker"), Duration::from_secs(10)),
        "confined command did not finish"
    );
    assert_eq!(
        fs::read_to_string(run.attempt.join("inside.txt"))
            .expect("attempt write")
            .trim(),
        "inside"
    );
    assert!(
        !outside.exists(),
        "write outside ATTEMPT must be denied: {}",
        outside.display()
    );

    let evidence = sandbox.terminate(&child).expect("terminate");
    assert!(evidence.process_group_gone, "detail: {}", evidence.detail);
}

#[tokio::test]
async fn malformed_missing_and_unknown_params_fail_closed() {
    let run = new_run("fail-closed");
    let canary = run.root.join("canary.txt");
    let mut sandbox = SeatbeltSandbox::new();

    // Case 1: unbalanced parentheses (structural rejection).
    let malformed = run.write_profile(
        "malformed.sb",
        "(version 1)\n(deny default)\n(allow file-read* (subpath \"/usr\")\n",
    );
    // Case 2: well-formed shape but a syntax error sandbox-exec rejects (rc 65).
    let bad_operator = run.write_profile(
        "bad-operator.sb",
        "(version 1)\n(deny default)\n(allow this-is-not-an-operator)\n",
    );
    // Case 3: profile file absent.
    let missing = run.path_str(&run.root.join("does-not-exist.sb"));
    // Case 4: references a parameter the launcher never supplies.
    let undefined = run.write_profile(
        "undefined-param.sb",
        "(version 1)\n(deny default)\n\
         (allow file-read* (subpath (param \"UNDEFINED_ATTEMPT\")))\n",
    );

    for profile in [&malformed, &bad_operator, &missing, &undefined] {
        let mut spec = run.spec(profile);
        spec.args = vec![
            "-c".to_string(),
            format!("echo FAILOPEN > {}", canary.display()),
        ];
        let err = sandbox
            .launch(spec)
            .expect_err("invalid profile must fail closed");
        assert!(
            matches!(err, ContractError::KnownFailure(_)),
            "unexpected error for {profile}: {err:?}"
        );
        assert!(
            !canary.exists(),
            "canary must stay absent for profile {profile}"
        );
    }
}

#[tokio::test]
async fn terminate_kills_descendant_tree() {
    let run = new_run("terminate-tree");
    let profile = run.write_profile("worker.sb", PRODUCT_PROFILE);
    let pidfile = run.attempt.join("descendant.pid");
    let script = format!(
        "sleep 300 & echo $! > {pidfile}; wait",
        pidfile = pidfile.display(),
    );

    let mut sandbox = SeatbeltSandbox::new();
    let mut spec = run.spec(&profile);
    spec.args = vec!["-c".to_string(), script];
    let child = sandbox.launch(spec).expect("confined launch");

    assert!(
        sandbox.alive(&child).expect("alive"),
        "leader should be alive"
    );
    assert!(
        wait_for(&pidfile, Duration::from_secs(10)),
        "descendant pid file missing"
    );
    let descendant: u32 = fs::read_to_string(&pidfile)
        .expect("read descendant pid")
        .trim()
        .parse()
        .expect("parse descendant pid");
    assert!(
        pid_alive(descendant),
        "descendant {descendant} should be alive before terminate"
    );

    let evidence = sandbox.terminate(&child).expect("terminate");
    assert!(evidence.process_group_gone, "detail: {}", evidence.detail);
    assert!(
        !sandbox.alive(&child).expect("alive after terminate"),
        "leader must be dead after terminate"
    );
    assert!(
        !pid_alive(descendant),
        "descendant {descendant} must be dead after terminate"
    );
}
