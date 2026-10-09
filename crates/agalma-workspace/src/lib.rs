//! Agalma workspace service.
//!
//! Owning wave: **M0.5** (`agalma-4ui.7`). Implements
//! `agalma_contracts::WorkspaceApi`: isolated checkout, diff, compare-and-update
//! integration, tag/revert, and reconciliation.
//!
//! Git command details stay private to this crate: the `git` CLI is invoked
//! through `std::process` and never leaks paths or arguments through the
//! contract. The service is serial (the conductor drives one checkout at a
//! time); [`Workspace::tag`] and [`Workspace::revert`] address the most recently
//! prepared repository.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use agalma_contracts::{Checkout, ContractError, ExecutionId, MergeReceipt, WorkspaceApi};

/// Upper bound on the rendered working-tree diff (bytes); longer diffs are
/// truncated to keep receipts bounded.
const MAX_DIFF_BYTES: usize = 256 * 1024;

/// A workspace rooted at a state directory.
///
/// Each execution gets `<base_dir>/runs/<execution>/checkout`, an independent
/// git repository (own `.git`) initialized from a template.
pub struct Workspace {
    base_dir: PathBuf,
    /// Repository of the most recently prepared checkout, used by the
    /// repository-less [`WorkspaceApi::tag`] and [`WorkspaceApi::revert`].
    current_repo: Option<PathBuf>,
}

impl Workspace {
    /// Create a workspace rooted at `base_dir`.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            current_repo: None,
        }
    }

    /// Repository of the most recently prepared checkout.
    fn current_repo(&self) -> Result<&Path, ContractError> {
        self.current_repo.as_deref().ok_or_else(|| {
            ContractError::KnownFailure("no checkout has been prepared in this workspace".into())
        })
    }

    /// Prepare an isolated checkout by cloning an origin repository (**repo
    /// mode**, M1).
    ///
    /// The checkout is an independent repository (`git clone --no-hardlinks`)
    /// under `<base_dir>/runs/<exec>/checkout`, with its own Git metadata, so
    /// candidate history never shares the origin's object store until
    /// [`Workspace::integrate_origin`] transfers it. A candidate branch
    /// `candidate/<exec>` is created at the origin's `base_ref` SHA and
    /// `base_sha` records that SHA (the expected pre-integration `main`).
    pub fn prepare_repo(
        &mut self,
        origin_path: &Path,
        base_ref: &str,
        execution: &ExecutionId,
        base_dir: &Path,
    ) -> Result<Checkout, ContractError> {
        if !origin_path.is_dir() {
            return Err(ContractError::KnownFailure(format!(
                "origin is not a directory: {}",
                origin_path.display()
            )));
        }
        let base_sha = rev_parse(origin_path, base_ref)?;

        let dest = base_dir
            .join("runs")
            .join(execution_dir_name(execution))
            .join("checkout");
        if dest.exists() {
            fs::remove_dir_all(&dest).map_err(|e| {
                ContractError::KnownFailure(format!(
                    "cannot reset checkout {}: {e}",
                    dest.display()
                ))
            })?;
        }
        let parent = dest
            .parent()
            .ok_or_else(|| ContractError::KnownFailure("checkout has no parent".into()))?;
        fs::create_dir_all(parent).map_err(|e| {
            ContractError::KnownFailure(format!("cannot create {}: {e}", parent.display()))
        })?;

        let origin_arg = origin_path.to_string_lossy().into_owned();
        let dest_arg = dest.to_string_lossy().into_owned();
        git_ok(parent, &["clone", "--no-hardlinks", &origin_arg, &dest_arg])?;
        git_ok(&dest, &["config", "user.name", "Agalma"])?;
        git_ok(&dest, &["config", "user.email", "agalma@localhost"])?;
        let candidate_branch = candidate_branch(execution);
        git_ok(&dest, &["checkout", "-b", &candidate_branch, &base_sha])?;

        self.current_repo = Some(dest.clone());
        Ok(Checkout {
            path: dest.to_string_lossy().into_owned(),
            base_sha,
            candidate_branch,
        })
    }

    /// Integrate a repo-mode candidate into the origin with expected-SHA CAS.
    ///
    /// Commits the dirty candidate tree, transfers the candidate objects into
    /// the origin (fetch from the checkout, refs untouched), then
    /// `git -C <origin> update-ref refs/heads/main <candidate> <expected>`. A
    /// mismatch returns [`ContractError::Conflict`] without moving `main`. On
    /// success the origin is tagged `m1/<exec>`.
    pub fn integrate_origin(
        &self,
        checkout: &Checkout,
        origin_path: &Path,
        expected_main_sha: &str,
    ) -> Result<MergeReceipt, ContractError> {
        let repo = PathBuf::from(&checkout.path);
        let candidate_sha = commit_candidate(&repo, checkout)?;

        // Fail closed on a moved `main` *before* transferring objects: no
        // mutation at all on conflict.
        let current_main = rev_parse(origin_path, "refs/heads/main")?;
        if current_main != expected_main_sha {
            return Err(ContractError::Conflict(format!(
                "main moved: expected {expected_main_sha}, found {current_main}"
            )));
        }

        // Transfer candidate objects into the origin's object store without
        // moving any ref: `git fetch` writes FETCH_HEAD only.
        let repo_arg = repo.to_string_lossy().into_owned();
        let branch_ref = format!("refs/heads/{}", checkout.candidate_branch);
        git_ok(origin_path, &["fetch", "--no-tags", &repo_arg, &branch_ref])?;

        let out = git(
            origin_path,
            &[
                "update-ref",
                "refs/heads/main",
                &candidate_sha,
                expected_main_sha,
            ],
        )?;
        if !out.success {
            return Err(ContractError::Conflict(format!(
                "refs/heads/main compare-and-swap failed: {}",
                out.stderr.trim()
            )));
        }

        ensure_tag(
            origin_path,
            &repo_tag_for_branch(&checkout.candidate_branch),
            &candidate_sha,
        )?;

        Ok(MergeReceipt {
            expected_main_sha: expected_main_sha.to_string(),
            candidate_sha: candidate_sha.clone(),
            result_sha: candidate_sha,
        })
    }

    /// Whether the origin's `main` already points at `candidate_sha`.
    pub fn origin_merged(origin_path: &Path, candidate_sha: &str) -> Result<bool, ContractError> {
        Ok(rev_parse_opt(origin_path, "refs/heads/main")?.as_deref() == Some(candidate_sha))
    }

    /// The candidate branch HEAD SHA in `checkout`'s repository.
    pub fn candidate_sha(&self, checkout: &Checkout) -> Result<String, ContractError> {
        rev_parse(
            Path::new(&checkout.path),
            &format!("refs/heads/{}", checkout.candidate_branch),
        )
    }

    /// Derive a receipt for an already-landed repo-mode integration.
    ///
    /// Returns `Some` when the origin's `main` is already at the candidate (and
    /// the candidate is ahead of base). Performs no mutation; recovery uses it
    /// to complete without re-integrating.
    pub fn origin_receipt(
        &self,
        checkout: &Checkout,
        origin_path: &Path,
    ) -> Result<Option<MergeReceipt>, ContractError> {
        let candidate_sha = self.candidate_sha(checkout)?;
        if candidate_sha == checkout.base_sha || !Self::origin_merged(origin_path, &candidate_sha)?
        {
            return Ok(None);
        }
        Ok(Some(MergeReceipt {
            expected_main_sha: checkout.base_sha.clone(),
            candidate_sha: candidate_sha.clone(),
            result_sha: candidate_sha,
        }))
    }

    /// Compare-and-swap the origin's `main` back to `to_sha` (repo-mode revert).
    pub fn revert_origin(
        &self,
        origin_path: &Path,
        to_sha: &str,
        from_sha: &str,
    ) -> Result<(), ContractError> {
        let out = git(
            origin_path,
            &["update-ref", "refs/heads/main", to_sha, from_sha],
        )?;
        if !out.success {
            return Err(ContractError::Conflict(format!(
                "repo revert compare-and-swap failed: {}",
                out.stderr.trim()
            )));
        }
        Ok(())
    }

    /// Derive the merge receipt for an already-landed integration.
    ///
    /// Returns `Some(MergeReceipt)` when `main` already points at the candidate
    /// branch HEAD and the candidate is ahead of the recorded base (i.e. the
    /// compare-and-update in [`WorkspaceApi::integrate`] completed but its
    /// completion was not recorded before a crash). Returns `None` when the
    /// merge has not happened, so the caller may safely [`WorkspaceApi::integrate`].
    ///
    /// This performs no update: recovery reconciles from Git state instead of
    /// re-merging. It records the repository as the current one so a follow-up
    /// [`WorkspaceApi::tag`] targets the same checkout.
    pub fn integrated_receipt(
        &mut self,
        checkout: &Checkout,
    ) -> Result<Option<MergeReceipt>, ContractError> {
        let repo = PathBuf::from(&checkout.path);
        let Some(main) = rev_parse_opt(&repo, "refs/heads/main")? else {
            return Ok(None);
        };
        let Some(candidate) =
            rev_parse_opt(&repo, &format!("refs/heads/{}", checkout.candidate_branch))?
        else {
            return Ok(None);
        };
        self.current_repo = Some(repo);
        if candidate != checkout.base_sha && main == candidate {
            Ok(Some(MergeReceipt {
                expected_main_sha: checkout.base_sha.clone(),
                result_sha: main,
                candidate_sha: candidate,
            }))
        } else {
            Ok(None)
        }
    }
}

impl WorkspaceApi for Workspace {
    fn prepare(
        &mut self,
        template: &str,
        execution: &ExecutionId,
    ) -> Result<Checkout, ContractError> {
        let template = PathBuf::from(template);
        if !template.is_dir() {
            return Err(ContractError::KnownFailure(format!(
                "template is not a directory: {}",
                template.display()
            )));
        }
        if template.join(".git").exists() {
            return Err(ContractError::KnownFailure(format!(
                "template must not contain git metadata: {}",
                template.display()
            )));
        }

        let dest = self
            .base_dir
            .join("runs")
            .join(execution_dir_name(execution))
            .join("checkout");
        if dest.exists() {
            fs::remove_dir_all(&dest).map_err(|e| {
                ContractError::KnownFailure(format!(
                    "cannot reset checkout {}: {e}",
                    dest.display()
                ))
            })?;
        }
        fs::create_dir_all(&dest).map_err(|e| {
            ContractError::KnownFailure(format!("cannot create checkout {}: {e}", dest.display()))
        })?;
        copy_dir_all(&template, &dest)?;

        git_ok(&dest, &["init", "-b", "main"])?;
        git_ok(&dest, &["config", "user.name", "Agalma"])?;
        git_ok(&dest, &["config", "user.email", "agalma@localhost"])?;
        git_ok(&dest, &["add", "-A"])?;
        git_ok(
            &dest,
            &["-c", "commit.gpgsign=false", "commit", "-m", "baseline"],
        )?;
        let base_sha = rev_parse(&dest, "HEAD")?;

        let candidate_branch = candidate_branch(execution);
        git_ok(&dest, &["checkout", "-b", &candidate_branch])?;

        self.current_repo = Some(dest.clone());
        Ok(Checkout {
            path: dest.to_string_lossy().into_owned(),
            base_sha,
            candidate_branch,
        })
    }

    fn diff(&self, checkout: &Checkout) -> Result<String, ContractError> {
        let repo = Path::new(&checkout.path);
        let mut text = git_ok(repo, &["diff", "HEAD", "--no-color"])?;
        if text.len() > MAX_DIFF_BYTES {
            let mut cut = MAX_DIFF_BYTES;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            text.push_str("\n[diff truncated]\n");
        }
        Ok(text)
    }

    fn integrate(
        &mut self,
        checkout: &Checkout,
        expected_main_sha: &str,
    ) -> Result<MergeReceipt, ContractError> {
        let repo = PathBuf::from(&checkout.path);
        let current_main = rev_parse(&repo, "refs/heads/main")?;
        if current_main != expected_main_sha {
            return Err(ContractError::Conflict(format!(
                "main moved: expected {expected_main_sha}, found {current_main}"
            )));
        }

        let candidate_sha = commit_candidate(&repo, checkout)?;
        let out = git(
            &repo,
            &[
                "update-ref",
                "refs/heads/main",
                &candidate_sha,
                expected_main_sha,
            ],
        )?;
        if !out.success {
            return Err(ContractError::Conflict(format!(
                "refs/heads/main compare-and-swap failed: {}",
                out.stderr.trim()
            )));
        }

        ensure_tag(
            &repo,
            &tag_for_branch(&checkout.candidate_branch),
            &candidate_sha,
        )?;

        self.current_repo = Some(repo);
        Ok(MergeReceipt {
            expected_main_sha: expected_main_sha.to_string(),
            candidate_sha: candidate_sha.clone(),
            result_sha: candidate_sha,
        })
    }

    fn tag(&mut self, name: &str, sha: &str) -> Result<(), ContractError> {
        let repo = self.current_repo()?.to_path_buf();
        ensure_tag(&repo, name, sha)
    }

    fn reconcile(&mut self, checkout: &Checkout) -> Result<bool, ContractError> {
        let repo = PathBuf::from(&checkout.path);
        let main_sha = rev_parse(&repo, "refs/heads/main")?;
        let candidate_sha = rev_parse(&repo, &format!("refs/heads/{}", checkout.candidate_branch))?;
        self.current_repo = Some(repo);
        Ok(candidate_sha != checkout.base_sha && main_sha == candidate_sha)
    }

    fn revert(&mut self, sha: &str) -> Result<(), ContractError> {
        let repo = self.current_repo()?.to_path_buf();
        let current_main = rev_parse(&repo, "refs/heads/main")?;
        let out = git(
            &repo,
            &["update-ref", "refs/heads/main", sha, &current_main],
        )?;
        if !out.success {
            return Err(ContractError::Conflict(format!(
                "revert compare-and-swap failed: {}",
                out.stderr.trim()
            )));
        }
        Ok(())
    }
}

/// Filesystem-safe directory component derived from an execution id.
///
/// The opaque execution id contains `:` (`exec:<task>:<gen>`), which is legal
/// in a POSIX path but breaks tools that compose `:`-separated environment
/// variables (e.g. cargo builds `DYLD_FALLBACK_LIBRARY_PATH` and rejects a
/// segment containing `:`). The run directory therefore uses this sanitized
/// form (`exec-fix-answer-1`); the ledger keeps the opaque id unchanged. It
/// matches the candidate-branch suffix so paths and refs agree.
pub fn execution_dir_name(execution: &ExecutionId) -> String {
    sanitize_ref(execution.as_str())
}

/// Git ref component derived from an execution id.
///
/// Git refs forbid `:` and other punctuation, so the opaque execution id is
/// sanitized into `[A-Za-z0-9._-]` for the candidate branch and integration tag.
fn candidate_branch(execution: &ExecutionId) -> String {
    format!("candidate/{}", sanitize_ref(execution.as_str()))
}

/// Integration tag for a candidate branch (`m0/<execution>`).
fn tag_for_branch(branch: &str) -> String {
    let suffix = branch.strip_prefix("candidate/").unwrap_or(branch);
    format!("m0/{suffix}")
}

/// Repo-mode integration tag for a candidate branch (`m1/<execution>`).
fn repo_tag_for_branch(branch: &str) -> String {
    let suffix = branch.strip_prefix("candidate/").unwrap_or(branch);
    format!("m1/{suffix}")
}

fn sanitize_ref(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Stage and commit the dirty candidate tree; return the resulting HEAD SHA.
///
/// A clean tree is not committed (the candidate SHA is the existing HEAD).
fn commit_candidate(repo: &Path, checkout: &Checkout) -> Result<String, ContractError> {
    git_ok(repo, &["add", "-A"])?;
    let status = git(repo, &["status", "--porcelain"])?;
    if !status.success {
        return Err(ContractError::KnownFailure(format!(
            "git status failed: {}",
            status.stderr.trim()
        )));
    }
    if !status.stdout.trim().is_empty() {
        let message = format!("candidate {}", checkout.candidate_branch);
        git_ok(
            repo,
            &["-c", "commit.gpgsign=false", "commit", "-m", &message],
        )?;
    }
    rev_parse(repo, "HEAD")
}

/// Create `name` at `sha`, or accept it if it already points at `sha`.
fn ensure_tag(repo: &Path, name: &str, sha: &str) -> Result<(), ContractError> {
    let existing = git(
        repo,
        &["rev-parse", "-q", "--verify", &format!("refs/tags/{name}")],
    )?;
    if existing.success {
        let found = existing.stdout.trim();
        if found == sha {
            return Ok(());
        }
        return Err(ContractError::Conflict(format!(
            "tag {name} exists at {found}, expected {sha}"
        )));
    }
    git_ok(repo, &["tag", name, sha])?;
    Ok(())
}

fn rev_parse(repo: &Path, rev: &str) -> Result<String, ContractError> {
    Ok(git_ok(repo, &["rev-parse", rev])?.trim().to_string())
}

/// `git rev-parse -q --verify <rev>`: `Some(sha)` when the ref resolves, else
/// `None` (a missing branch is not an error for reconciliation).
fn rev_parse_opt(repo: &Path, rev: &str) -> Result<Option<String>, ContractError> {
    let out = git(repo, &["rev-parse", "-q", "--verify", rev])?;
    if !out.success {
        return Ok(None);
    }
    let sha = out.stdout.trim();
    if sha.is_empty() {
        Ok(None)
    } else {
        Ok(Some(sha.to_string()))
    }
}

struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

/// Run `git` in `dir`, capturing stdout/stderr. Never inherits the terminal, so
/// failures surface as `KnownFailure` rather than an interactive prompt.
fn git(dir: &Path, args: &[&str]) -> Result<GitOutput, ContractError> {
    let output = Command::new("git")
        .arg("--no-pager")
        .args(args)
        .current_dir(dir)
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .map_err(|e| {
            ContractError::KnownFailure(format!("failed to spawn git {}: {e}", args.join(" ")))
        })?;
    Ok(GitOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// Run `git` and fail with its captured stderr on a non-zero exit.
fn git_ok(dir: &Path, args: &[&str]) -> Result<String, ContractError> {
    let out = git(dir, args)?;
    if !out.success {
        return Err(ContractError::KnownFailure(format!(
            "git {} failed: {}",
            args.join(" "),
            out.stderr.trim()
        )));
    }
    Ok(out.stdout)
}

/// Recursively copy a template directory into a fresh checkout.
fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), ContractError> {
    for entry in fs::read_dir(src).map_err(|e| {
        ContractError::KnownFailure(format!("cannot read template {}: {e}", src.display()))
    })? {
        let entry = entry
            .map_err(|e| ContractError::KnownFailure(format!("cannot read template entry: {e}")))?;
        let target = dst.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|e| ContractError::KnownFailure(format!("cannot stat template entry: {e}")))?;
        if file_type.is_dir() {
            fs::create_dir_all(&target).map_err(|e| {
                ContractError::KnownFailure(format!("cannot create {}: {e}", target.display()))
            })?;
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(|e| {
                ContractError::KnownFailure(format!(
                    "cannot copy {} -> {}: {e}",
                    entry.path().display(),
                    target.display()
                ))
            })?;
        }
    }
    Ok(())
}
