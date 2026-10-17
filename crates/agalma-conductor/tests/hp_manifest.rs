//! HP.2 plugin-package discovery acceptance tests (`agalma-jmr.2`).
//!
//! Each test builds independent sealed-package fixtures under
//! `target/test-runs/hp-manifest-*` (no `/tmp`, no network, no child process).
//! The assertions use documented failure codes rather than implementation
//! internals, and the property tests use a recorded deterministic seed and case
//! budget.
//!
//! Behavioral coverage: an unknown package is discovered with its descriptor
//! intact; malformed/duplicate/escaped/cyclic fixtures fail before any process;
//! ordering is deterministic; root policy (required/optional/missing/permission/
//! non-directory) matches D1; and plugin-root configuration round trips through
//! the common CLI/config path.

use std::fs;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde_json::{json, Value};

use agalma_conductor::cli::{Cli, Command};
use agalma_conductor::config::{resolve_plugin_roots, Config};
use agalma_conductor::plugin_manifest::{
    discover_plugins, package_inventory_digest, DiscoveryReport, PluginRoot, PLUGIN_MANIFEST_FILE,
};
use agalma_contracts::digest::{failure_code, sha256_digest, FailureCode};
use agalma_contracts::{ContractError, PluginManifest};
use agalma_testkit::ScratchDir;

/// Recorded deterministic property-test seed (xorshift64).
const PROPERTY_SEED: u64 = 0x5eed_c0de_1234_5678;
/// Recorded property-test case budget per property.
const PROPERTY_CASES: u64 = 24;

/// Frozen `PluginManifest/v1` `maxItems` bound.
const MAX_ITEMS: usize = 100;

/// Config-schema bytes written by every standard fixture package.
const CONFIG_SCHEMA_BYTES: &[u8] =
    b"{\"type\":\"object\",\"properties\":{},\"additionalProperties\":false}\n";
/// Default-configuration bytes written by every standard fixture package.
const DEFAULT_CONFIG_BYTES: &[u8] = b"{}\n";
/// Report-schema bytes written by every standard fixture package.
const REPORT_SCHEMA_BYTES: &[u8] = b"{\"type\":\"object\",\"required\":[\"ready\"]}\n";

fn fresh(name: &str) -> ScratchDir {
    ScratchDir::new(name)
}

/// A pinned `ArtifactRef` for `bytes` at package-relative `path`.
fn byte_ref(path: &str, bytes: &[u8], media_type: &str) -> Value {
    json!({
        "path": path,
        "digest": sha256_digest(bytes),
        "bytes": bytes.len() as u64,
        "media_type": media_type,
    })
}

/// Deterministic xorshift64 generator so a property failure replays exactly.
struct XorShift64(u64);

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// A `PluginManifest/v1` whose artifact references match [`standard_files`].
fn base_manifest(plugin_id: &str) -> Value {
    json!({
        "format": "PluginManifest/v1",
        "plugin_id": plugin_id,
        "package_version": "1.2.3",
        "package_digest": format!("sha256:{}", "0".repeat(64)),
        "protocol_major": 1,
        "entrypoint": {
            "path": "bin/plugin",
            "argv": ["--serve", "--read-only"],
            "launch_profile": "reporter-read-only/v1"
        },
        "domain_apis": [],
        "requested_capabilities": ["report.publish"],
        "required_services": [],
        "optional_services": [],
        "config_schema": byte_ref("objects/config-schema.json", CONFIG_SCHEMA_BYTES, "application/json"),
        "default_configuration": byte_ref("objects/default-config.json", DEFAULT_CONFIG_BYTES, "application/json"),
        "report_schemas": [{
            "namespace": "example.reporter.status",
            "version": 1,
            "schema": byte_ref("objects/report-schema.json", REPORT_SCHEMA_BYTES, "application/json")
        }],
        "views": [{
            "view_id": "status",
            "schema_namespace": "example.reporter.status",
            "kind": "Status",
            "title": "Reporter status",
            "schema_version": 1,
            "fields": ["ready"]
        }],
        "actions": [],
        "state_compatibility": {"reads": ["reporter-state/v1"], "writes": "reporter-state/v1"}
    })
}

/// Files every fixture package ships, keyed by package-relative path.
fn standard_files() -> Vec<(String, Vec<u8>)> {
    vec![
        ("bin/plugin".to_string(), b"#!/bin/sh\nexit 0\n".to_vec()),
        (
            "objects/config-schema.json".to_string(),
            CONFIG_SCHEMA_BYTES.to_vec(),
        ),
        (
            "objects/default-config.json".to_string(),
            DEFAULT_CONFIG_BYTES.to_vec(),
        ),
        (
            "objects/report-schema.json".to_string(),
            REPORT_SCHEMA_BYTES.to_vec(),
        ),
    ]
}

/// Write `files`, compute the sealed-inventory digest, then write the manifest
/// with that digest and return the parsed, sealed manifest.
fn seal_package(
    package_dir: &Path,
    manifest: &Value,
    files: &[(String, Vec<u8>)],
) -> PluginManifest {
    write_files(package_dir, files);
    let parsed: PluginManifest = serde_json::from_value(manifest.clone()).unwrap();
    let digest = package_inventory_digest(package_dir, &parsed).unwrap();
    let mut sealed = manifest.clone();
    sealed["package_digest"] = Value::String(digest);
    fs::write(
        package_dir.join(PLUGIN_MANIFEST_FILE),
        serde_json::to_vec_pretty(&sealed).unwrap(),
    )
    .unwrap();
    serde_json::from_value(sealed).unwrap()
}

/// Write package files (no manifest) so a test can control the manifest bytes.
fn write_files(package_dir: &Path, files: &[(String, Vec<u8>)]) {
    fs::create_dir_all(package_dir).unwrap();
    for (relative, bytes) in files {
        let path = package_dir.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, bytes).unwrap();
    }
}

/// Seal a standard package with `plugin_id` at `package_dir`.
fn simple_package(package_dir: &Path, plugin_id: &str) -> PluginManifest {
    seal_package(package_dir, &base_manifest(plugin_id), &standard_files())
}

fn required(path: &Path) -> PluginRoot {
    PluginRoot::required(path)
}

fn optional(path: &Path) -> PluginRoot {
    PluginRoot::optional(path)
}

fn discover(roots: &[PluginRoot]) -> Result<DiscoveryReport, ContractError> {
    discover_plugins(roots)
}

fn code(err: &ContractError) -> FailureCode {
    failure_code(err).expect("discovery failures carry a structured code")
}

fn message(err: &ContractError) -> String {
    format!("{err:?}")
}

/// Seal one package under `<scratch>/root/<dir_name>`, assert discovery refuses
/// with `expected`, then remove it so the next case reuses the same root.
fn expect_refusal(
    scratch: &ScratchDir,
    dir_name: &str,
    manifest: Value,
    expected: FailureCode,
) {
    let root = scratch.path().join("root");
    fs::create_dir_all(&root).unwrap();
    let dir = root.join(dir_name);
    seal_package(&dir, &manifest, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), expected, "case {dir_name}: {}", message(&err));
    fs::remove_dir_all(&dir).unwrap();
}

// --- acceptance cases -------------------------------------------------------

#[test]
fn unknown_package_discovered_with_descriptor_intact() {
    let scratch = fresh("hp-manifest-unknown");
    let root = scratch.child("plugins");
    let package_dir = root.join("reporter");
    let manifest = simple_package(&package_dir, "example.unfamiliar-reporter");

    let report = discover(&[required(&root)]).unwrap();
    assert!(report.diagnostics.is_empty());
    assert_eq!(report.plugin_ids(), vec!["example.unfamiliar-reporter"]);

    let package = report.package("example.unfamiliar-reporter").unwrap();
    assert_eq!(package.package_version, "1.2.3");
    assert_eq!(package.protocol_major, 1);
    assert_eq!(package.package_digest, manifest.package_digest);
    assert_eq!(package.root, root);
    assert_eq!(
        package.entrypoint_path,
        fs::canonicalize(package_dir.join("bin/plugin")).unwrap()
    );
    // The descriptor is intact: declarations survive discovery unchanged.
    assert_eq!(
        package.manifest.requested_capabilities,
        vec!["report.publish".to_string()]
    );
    assert_eq!(package.manifest.entrypoint.argv, vec!["--serve", "--read-only"]);
    assert_eq!(package.manifest, manifest);
}

#[test]
fn no_roots_discover_nothing() {
    let report = discover(&[]).unwrap();
    assert!(report.packages.is_empty());
    assert!(report.diagnostics.is_empty());
}

#[test]
fn missing_required_root_refuses_and_creates_nothing() {
    let scratch = fresh("hp-manifest-required-missing");
    let root = scratch.path().join("absent-required");
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::StateUnavailable);
    assert!(!root.exists(), "discovery must not create a required root");
}

#[test]
fn missing_optional_root_contributes_nothing_with_diagnostic() {
    let scratch = fresh("hp-manifest-optional-missing");
    let root = scratch.path().join("absent-optional");
    let report = discover(&[optional(&root)]).unwrap();
    assert!(report.packages.is_empty());
    assert_eq!(report.diagnostics.len(), 1);
    assert_eq!(report.diagnostics[0].root, root);
    assert!(report.diagnostics[0].message.contains("missing"));
    assert!(!root.exists(), "discovery must not create an optional root");
}

#[test]
fn non_directory_root_refuses_even_when_optional() {
    let scratch = fresh("hp-manifest-non-dir");
    let file = scratch.child("not-a-dir");
    let root = file.join("file-root");
    fs::write(&root, b"not a directory").unwrap();
    let err = discover(&[optional(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::InvalidRecord);
    assert!(message(&err).contains("not a directory"));
}

#[test]
fn invalid_package_fails_even_in_optional_root() {
    let scratch = fresh("hp-manifest-optional-invalid");
    let root = scratch.child("optional-root");
    fs::create_dir_all(root.join("broken")).unwrap();
    let err = discover(&[optional(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::InvalidRecord);
    assert!(message(&err).contains("missing"));
}

#[test]
fn malformed_manifest_refuses() {
    let scratch = fresh("hp-manifest-malformed");
    let root = scratch.child("plugins");
    let package_dir = root.join("broken");
    fs::create_dir_all(&package_dir).unwrap();
    fs::write(package_dir.join(PLUGIN_MANIFEST_FILE), b"{ not json").unwrap();
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::InvalidRecord);
    assert!(message(&err).contains("malformed manifest"));
}

#[test]
fn unknown_manifest_field_refuses() {
    let scratch = fresh("hp-manifest-unknown-field");
    let root = scratch.child("plugins");
    let package_dir = root.join("pkg");
    let mut manifest = base_manifest("example.extra-field");
    manifest["unexpected"] = json!(true);
    write_files(&package_dir, &standard_files());
    fs::write(
        package_dir.join(PLUGIN_MANIFEST_FILE),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::InvalidRecord);
}

#[test]
fn unsupported_protocol_major_and_family_refuse() {
    let scratch = fresh("hp-manifest-version");
    let root = scratch.child("plugins");

    let mut major = base_manifest("example.bad-major");
    major["protocol_major"] = json!(99);
    seal_package(&root.join("major"), &major, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::UnsupportedVersion);

    fs::remove_dir_all(root.join("major")).unwrap();

    let mut family = base_manifest("example.bad-family");
    family["format"] = json!("OtherThing/v1");
    seal_package(&root.join("family"), &family, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::UnsupportedSchema);

    fs::remove_dir_all(root.join("family")).unwrap();

    let mut version = base_manifest("example.bad-version");
    version["format"] = json!("PluginManifest/v2");
    seal_package(&root.join("version"), &version, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::UnsupportedVersion);
}

#[test]
fn duplicate_plugin_ids_conflict_across_roots() {
    let scratch = fresh("hp-manifest-duplicate");
    let root_a = scratch.child("root-a");
    let root_b = scratch.child("root-b");
    simple_package(&root_a.join("one"), "example.dup");
    simple_package(&root_b.join("two"), "example.dup");
    let err = discover(&[required(&root_a), required(&root_b)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::IdentityConflict);
    assert!(message(&err).contains("duplicate plugin_id"));
}

#[test]
fn escaped_or_absolute_entrypoint_refuses() {
    let scratch = fresh("hp-manifest-entrypoint");
    let root = scratch.child("plugins");

    let mut escaping = base_manifest("example.escaping");
    escaping["entrypoint"]["path"] = json!("../outside/plugin");
    seal_package(&root.join("escaping"), &escaping, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::ScopeDenied);

    fs::remove_dir_all(root.join("escaping")).unwrap();

    let mut absolute = base_manifest("example.absolute");
    absolute["entrypoint"]["path"] = json!("/bin/sh");
    seal_package(&root.join("absolute"), &absolute, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::ScopeDenied);
}

#[cfg(unix)]
#[test]
fn symlinked_entrypoint_escaping_package_refuses() {
    let scratch = fresh("hp-manifest-entrypoint-symlink");
    let root = scratch.child("plugins");
    let package_dir = root.join("pkg");
    simple_package(&package_dir, "example.symlink-entry");
    let outside = scratch.child("outside");
    let target = outside.join("real-exec");
    fs::write(&target, b"#!/bin/sh\nexit 0\n").unwrap();
    fs::remove_file(package_dir.join("bin/plugin")).unwrap();
    std::os::unix::fs::symlink(&target, package_dir.join("bin/plugin")).unwrap();

    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::ScopeDenied);
}

#[cfg(unix)]
#[test]
fn symlinked_inventory_entry_escaping_package_refuses() {
    let scratch = fresh("hp-manifest-inventory-symlink");
    let root = scratch.child("plugins");
    let package_dir = root.join("pkg");
    simple_package(&package_dir, "example.symlink-inventory");
    let outside = scratch.child("outside");
    let target = outside.join("secret");
    fs::write(&target, b"secret bytes").unwrap();
    std::os::unix::fs::symlink(&target, package_dir.join("escape")).unwrap();

    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::ScopeDenied);
    assert!(message(&err).contains("escapes"));
}

#[test]
fn changed_file_bytes_fail_digest_verification() {
    let scratch = fresh("hp-manifest-digest-tamper");
    let root = scratch.child("plugins");
    let package_dir = root.join("pkg");
    simple_package(&package_dir, "example.tampered");
    fs::write(package_dir.join("bin/plugin"), b"#!/bin/sh\nexit 1\n").unwrap();
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::IntegrityMismatch);
}

#[test]
fn cyclic_required_dependency_refuses() {
    let scratch = fresh("hp-manifest-cycle");
    let root = scratch.child("plugins");

    let mut a = base_manifest("example.cycle-a");
    a["required_services"] = json!(["example.cycle-b"]);
    seal_package(&root.join("a"), &a, &standard_files());
    let mut b = base_manifest("example.cycle-b");
    b["required_services"] = json!(["example.cycle-a"]);
    seal_package(&root.join("b"), &b, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::InvalidRecord);
    assert!(message(&err).contains("cycle"));

    fs::remove_dir_all(root.join("a")).unwrap();
    fs::remove_dir_all(root.join("b")).unwrap();

    let mut self_cycle = base_manifest("example.cycle-self");
    self_cycle["required_services"] = json!(["example.cycle-self"]);
    seal_package(&root.join("self"), &self_cycle, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::InvalidRecord);
}

#[test]
fn required_service_declarations_are_validated() {
    let scratch = fresh("hp-manifest-services");
    let root = scratch.child("plugins");

    let mut duplicate = base_manifest("example.dup-service");
    duplicate["required_services"] = json!(["component-store/v1", "component-store/v1"]);
    seal_package(&root.join("duplicate"), &duplicate, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::InvalidRecord);
    fs::remove_dir_all(root.join("duplicate")).unwrap();

    let mut contradictory = base_manifest("example.conflicting-service");
    contradictory["required_services"] = json!(["component-store/v1"]);
    contradictory["optional_services"] = json!(["component-store/v1"]);
    seal_package(&root.join("conflicting"), &contradictory, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::InvalidRecord);
    fs::remove_dir_all(root.join("conflicting")).unwrap();

    let mut empty = base_manifest("example.empty-service");
    empty["required_services"] = json!([""]);
    seal_package(&root.join("empty"), &empty, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::InvalidRecord);
}

#[test]
fn duplicate_action_and_view_ids_refuse() {
    let scratch = fresh("hp-manifest-declarations");
    let root = scratch.child("plugins");

    let mut manifest = base_manifest("example.dup-view");
    manifest["views"] = json!([
        {"view_id": "status", "schema_namespace": "ns", "kind": "Status",
         "title": "a", "schema_version": 1, "fields": ["ready"]},
        {"view_id": "status", "schema_namespace": "ns", "kind": "Metrics",
         "title": "b", "schema_version": 1, "fields": ["ready"]}
    ]);
    seal_package(&root.join("views"), &manifest, &standard_files());
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::InvalidRecord);
}

#[cfg(unix)]
#[test]
fn permission_denied_root_refuses_even_when_optional() {
    use std::os::unix::fs::PermissionsExt;

    let scratch = fresh("hp-manifest-permission");
    let root = scratch.child("locked");
    simple_package(&root.join("pkg"), "example.locked");
    let restore = root.clone();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();

    let outcome = discover(&[optional(&root)]);

    // Restore permissions first so scratch cleanup can remove the tree.
    fs::set_permissions(&restore, fs::Permissions::from_mode(0o755)).unwrap();

    let err = outcome.unwrap_err();
    assert_eq!(code(&err), FailureCode::StateUnavailable);
}

#[test]
fn ordering_is_deterministic_by_plugin_id() {
    let scratch = fresh("hp-manifest-ordering");
    let root_a = scratch.child("root-a");
    let root_b = scratch.child("root-b");

    // Directory names deliberately contradict plugin-id order.
    simple_package(&root_a.join("z-dir"), "example.zulu");
    simple_package(&root_a.join("m-dir"), "example.mike");
    simple_package(&root_b.join("a-dir"), "example.alpha");

    let first = discover(&[required(&root_a), required(&root_b)]).unwrap();
    let second = discover(&[required(&root_a), required(&root_b)]).unwrap();
    let expected = vec!["example.alpha", "example.mike", "example.zulu"];
    assert_eq!(first.plugin_ids(), expected);
    assert_eq!(second.plugin_ids(), expected);
    assert_eq!(
        first
            .packages
            .iter()
            .map(|p| p.plugin_id.clone())
            .collect::<Vec<_>>(),
        second
            .packages
            .iter()
            .map(|p| p.plugin_id.clone())
            .collect::<Vec<_>>()
    );
}

#[test]
fn plugin_root_config_round_trips_through_run_cli() {
    let cli = Cli::try_parse_from([
        "agalma",
        "run",
        "--fixture",
        "fixture-dir",
        "--plugin-root",
        "/roots/one",
        "--plugin-root",
        "/roots/two",
        "--plugin-root-optional",
        "/roots/optional",
    ])
    .unwrap();
    let Command::Run(args) = cli.command else {
        panic!("expected run command");
    };
    let config = Config::resolve(&args).unwrap();
    assert_eq!(
        config.plugin_roots,
        vec![
            PluginRoot::required(PathBuf::from("/roots/one")),
            PluginRoot::required(PathBuf::from("/roots/two")),
            PluginRoot::optional(PathBuf::from("/roots/optional")),
        ]
    );
}

#[test]
fn plugin_root_config_round_trips_through_work_cli() {
    let cli = Cli::try_parse_from([
        "agalma",
        "work",
        "--repo",
        "repo-dir",
        "--plugin-root",
        "/roots/primary",
        "--plugin-root-optional",
        "/roots/extra",
    ])
    .unwrap();
    let Command::Work(args) = cli.command else {
        panic!("expected work command");
    };
    assert_eq!(
        resolve_plugin_roots(&args.plugin_roots),
        vec![
            PluginRoot::required(PathBuf::from("/roots/primary")),
            PluginRoot::optional(PathBuf::from("/roots/extra")),
        ]
    );
}

#[test]
fn plugin_root_config_defaults_to_empty() {
    let cli = Cli::try_parse_from(["agalma", "work", "--repo", "repo-dir"]).unwrap();
    let Command::Work(args) = cli.command else {
        panic!("expected work command");
    };
    assert!(resolve_plugin_roots(&args.plugin_roots).is_empty());
}

// --- review-repair fixtures (findings 1-3) ----------------------------------

#[test]
fn empty_report_schemas_refuses() {
    let scratch = fresh("hp-manifest-empty-schemas");
    let mut manifest = base_manifest("example.no-schema");
    manifest["report_schemas"] = json!([]);
    expect_refusal(
        &scratch,
        "pkg",
        manifest,
        FailureCode::InvalidRecord,
    );
}

#[test]
fn empty_views_refuses() {
    let scratch = fresh("hp-manifest-empty-views");
    let mut manifest = base_manifest("example.no-view");
    manifest["views"] = json!([]);
    expect_refusal(&scratch, "pkg", manifest, FailureCode::InvalidRecord);
}

#[test]
fn declaration_item_bounds_enforced() {
    let scratch = fresh("hp-manifest-bounds");
    let over = MAX_ITEMS + 1;

    let mut caps = base_manifest("example.too-many-caps");
    caps["requested_capabilities"] = json!((0..over)
        .map(|index| format!("cap-{index}"))
        .collect::<Vec<_>>());
    expect_refusal(&scratch, "caps", caps, FailureCode::InvalidRecord);

    let mut apis = base_manifest("example.too-many-apis");
    apis["domain_apis"] = json!((0..over)
        .map(|index| json!({"api": format!("api-{index}"), "major": 1, "capabilities": []}))
        .collect::<Vec<_>>());
    expect_refusal(&scratch, "apis", apis, FailureCode::InvalidRecord);

    let mut argv = base_manifest("example.too-many-argv");
    argv["entrypoint"]["argv"] = json!((0..over)
        .map(|index| format!("a{index}"))
        .collect::<Vec<_>>());
    expect_refusal(&scratch, "argv", argv, FailureCode::InvalidRecord);

    let mut fields = base_manifest("example.too-many-fields");
    fields["views"][0]["fields"] = json!((0..over)
        .map(|index| format!("f{index}"))
        .collect::<Vec<_>>());
    expect_refusal(&scratch, "fields", fields, FailureCode::InvalidRecord);

    let mut schemas = base_manifest("example.too-many-schemas");
    schemas["report_schemas"] = json!((0..over)
        .map(|index| json!({
            "namespace": format!("ns-{index}"),
            "version": 1,
            "schema": byte_ref("objects/report-schema.json", REPORT_SCHEMA_BYTES, "application/json")
        }))
        .collect::<Vec<_>>());
    expect_refusal(&scratch, "schemas", schemas, FailureCode::InvalidRecord);
}

#[test]
fn declaration_string_and_integer_constraints_enforced() {
    let scratch = fresh("hp-manifest-constraints");

    let bad_id = base_manifest("-bad-plugin-id");
    expect_refusal(&scratch, "id", bad_id, FailureCode::InvalidRecord);

    let mut empty_cap = base_manifest("example.empty-cap");
    empty_cap["requested_capabilities"] = json!([""]);
    expect_refusal(&scratch, "cap", empty_cap, FailureCode::InvalidRecord);

    let mut empty_service = base_manifest("example.empty-service-item");
    empty_service["required_services"] = json!([""]);
    expect_refusal(&scratch, "service", empty_service, FailureCode::InvalidRecord);

    let mut empty_fields = base_manifest("example.empty-fields");
    empty_fields["views"][0]["fields"] = json!([]);
    expect_refusal(&scratch, "fields", empty_fields, FailureCode::InvalidRecord);

    let mut blank_field = base_manifest("example.blank-field");
    blank_field["views"][0]["fields"] = json!([""]);
    expect_refusal(&scratch, "blank-field", blank_field, FailureCode::InvalidRecord);

    let mut blank_title = base_manifest("example.blank-title");
    blank_title["views"][0]["title"] = json!("");
    expect_refusal(&scratch, "title", blank_title, FailureCode::InvalidRecord);

    let mut bad_view_id = base_manifest("example.bad-view-id");
    bad_view_id["views"][0]["view_id"] = json!("-bad");
    expect_refusal(&scratch, "view-id", bad_view_id, FailureCode::InvalidRecord);

    let mut zero_version = base_manifest("example.zero-schema-version");
    zero_version["report_schemas"][0]["version"] = json!(0);
    expect_refusal(&scratch, "version", zero_version, FailureCode::InvalidRecord);

    let mut zero_deadline = base_manifest("example.zero-deadline");
    zero_deadline["actions"] = json!([{
        "action_id": "act",
        "input_schema": "example.input/v1",
        "required_capability": "act.run",
        "effectful": true,
        "human_confirmation": null,
        "default_deadline_ms": 0
    }]);
    expect_refusal(&scratch, "deadline", zero_deadline, FailureCode::InvalidRecord);

    let mut blank_confirmation = base_manifest("example.blank-confirmation");
    blank_confirmation["actions"] = json!([{
        "action_id": "act",
        "input_schema": "example.input/v1",
        "required_capability": "act.run",
        "effectful": false,
        "human_confirmation": "",
        "default_deadline_ms": 1000
    }]);
    expect_refusal(
        &scratch,
        "confirmation",
        blank_confirmation,
        FailureCode::InvalidRecord,
    );
}

#[test]
fn valid_artifact_references_verify() {
    let scratch = fresh("hp-manifest-refs-valid");
    let root = scratch.child("plugins");
    let manifest = simple_package(&root.join("pkg"), "example.refs-ok");
    let report = discover(&[required(&root)]).unwrap();
    let package = report.package("example.refs-ok").unwrap();
    assert_eq!(package.manifest.config_schema, manifest.config_schema);
    assert_eq!(
        package.manifest.report_schemas[0].schema,
        manifest.report_schemas[0].schema
    );
}

#[test]
fn missing_artifact_reference_refuses() {
    let scratch = fresh("hp-manifest-refs-missing");
    let mut manifest = base_manifest("example.missing-ref");
    manifest["config_schema"] = json!({
        "path": "objects/absent.json",
        "digest": sha256_digest(b"{}\n"),
        "bytes": 3,
        "media_type": "application/json"
    });
    expect_refusal(&scratch, "pkg", manifest, FailureCode::InvalidRecord);
}

#[test]
fn escaped_artifact_reference_refuses() {
    let scratch = fresh("hp-manifest-refs-escaped");
    let mut manifest = base_manifest("example.escaped-ref");
    manifest["default_configuration"] = json!({
        "path": "../outside.json",
        "digest": sha256_digest(DEFAULT_CONFIG_BYTES),
        "bytes": DEFAULT_CONFIG_BYTES.len() as u64,
        "media_type": "application/json"
    });
    expect_refusal(&scratch, "pkg", manifest, FailureCode::ScopeDenied);
}

#[test]
fn malformed_artifact_digest_shape_refuses() {
    let scratch = fresh("hp-manifest-refs-shape");
    let mut manifest = base_manifest("example.bad-ref-digest");
    manifest["config_schema"]["digest"] = json!("not-a-digest");
    // The frozen `ArtifactRef.digest` pattern keeper reports a bad shape as an
    // integrity identity failure, before any file content is read.
    expect_refusal(&scratch, "pkg", manifest, FailureCode::IntegrityMismatch);
}

#[test]
fn artifact_bytes_mismatch_refuses() {
    let scratch = fresh("hp-manifest-refs-bytes");
    let mut manifest = base_manifest("example.short-ref");
    // Digest/bytes pin a smaller object than the file on disk.
    manifest["config_schema"] = byte_ref("objects/config-schema.json", b"{}\n", "application/json");
    expect_refusal(&scratch, "pkg", manifest, FailureCode::IntegrityMismatch);

    let mut default_mismatch = base_manifest("example.default-ref");
    default_mismatch["default_configuration"] = json!({
        "path": "objects/default-config.json",
        "digest": sha256_digest(b"{}"),
        "bytes": 2,
        "media_type": "application/json"
    });
    expect_refusal(
        &scratch,
        "default",
        default_mismatch,
        FailureCode::IntegrityMismatch,
    );

    let mut schema_mismatch = base_manifest("example.schema-ref");
    schema_mismatch["report_schemas"][0]["schema"]["bytes"] = json!(9_999_999);
    expect_refusal(
        &scratch,
        "schema",
        schema_mismatch,
        FailureCode::IntegrityMismatch,
    );
}

#[test]
fn non_file_artifact_reference_refuses() {
    let scratch = fresh("hp-manifest-refs-nonfile");
    let mut manifest = base_manifest("example.nonfile-ref");
    manifest["default_configuration"]["path"] = json!("objects");
    expect_refusal(&scratch, "pkg", manifest, FailureCode::InvalidRecord);
}

#[test]
fn empty_artifact_media_type_refuses() {
    let scratch = fresh("hp-manifest-refs-media-type");

    let mut config = base_manifest("example.blank-media-config");
    config["config_schema"]["media_type"] = json!("");
    expect_refusal(&scratch, "config", config, FailureCode::InvalidRecord);

    let mut default = base_manifest("example.blank-media-default");
    default["default_configuration"]["media_type"] = json!("");
    expect_refusal(&scratch, "default", default, FailureCode::InvalidRecord);

    let mut schema = base_manifest("example.blank-media-schema");
    schema["report_schemas"][0]["schema"]["media_type"] = json!("");
    expect_refusal(&scratch, "schema", schema, FailureCode::InvalidRecord);
}

#[cfg(unix)]
#[test]
fn directory_symlinks_in_package_refuse() {
    let scratch = fresh("hp-manifest-dir-symlink");
    let root = scratch.child("plugins");

    let internal = root.join("internal");
    simple_package(&internal, "example.internal-dir-link");
    std::os::unix::fs::symlink(&internal, internal.join("self-link")).unwrap();
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::ScopeDenied);
    assert!(message(&err).contains("directory symlink"));
    fs::remove_dir_all(root.join("internal")).unwrap();

    let ancestor = root.join("ancestor");
    simple_package(&ancestor, "example.ancestor-dir-link");
    std::os::unix::fs::symlink("..", ancestor.join("up")).unwrap();
    let err = discover(&[required(&root)]).unwrap_err();
    assert_eq!(code(&err), FailureCode::ScopeDenied);
}

// --- property tests ---------------------------------------------------------

#[test]
fn property_inventory_digest_independent_of_creation_order() {
    let scratch = fresh("hp-manifest-prop-order");
    let mut rng = XorShift64::new(PROPERTY_SEED);
    for case in 0..PROPERTY_CASES {
        let plugin_id = format!("example.prop-{case}");
        let manifest = base_manifest(&plugin_id);
        let count = 1 + rng.below(6) as usize;
        let mut files: Vec<(String, Vec<u8>)> = standard_files();
        for index in 0..count {
            let len = rng.below(48) as usize;
            let bytes: Vec<u8> = (0..len).map(|_| rng.next_u64() as u8).collect();
            files.push((format!("data/file-{index}-{}.bin", rng.below(1000)), bytes));
        }

        let root_a = scratch.child(&format!("case-{case}/root-a"));
        let root_b = scratch.child(&format!("case-{case}/root-b"));
        let sealed_a = seal_package(&root_a.join("pkg"), &manifest, &files);
        let mut reversed = files.clone();
        reversed.reverse();
        let sealed_b = seal_package(&root_b.join("pkg"), &manifest, &reversed);
        assert_eq!(sealed_a.package_digest, sealed_b.package_digest);

        let report_a = discover(&[required(&root_a)]).unwrap();
        let report_b = discover(&[required(&root_b)]).unwrap();
        assert_eq!(report_a.packages[0].package_digest, report_b.packages[0].package_digest);
    }
}

#[test]
fn property_discovery_order_is_sorted_for_generated_sets() {
    let scratch = fresh("hp-manifest-prop-order-set");
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0xabcd);
    for case in 0..PROPERTY_CASES {
        let root = scratch.child(&format!("set-{case}"));
        let count = 1 + rng.below(5) as usize;
        let mut ids = Vec::with_capacity(count);
        for index in 0..count {
            let id = format!("example.gen-{:03}-{case}-{index}", rng.below(999));
            if ids.contains(&id) {
                continue;
            }
            ids.push(id.clone());
            let dir = root.join(format!("dir-{}", rng.below(10_000)));
            simple_package(&dir, &id);
        }
        ids.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        ids.dedup();

        let report = discover(&[required(&root)]).unwrap();
        let discovered: Vec<String> = report
            .plugin_ids()
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(discovered, ids, "case {case}");
    }
}

/// Valid packages must discover; flipping one byte of a referenced artifact
/// must turn the same package into a typed `IntegrityMismatch`.
#[test]
fn property_artifact_reference_integrity() {
    let scratch = fresh("hp-manifest-prop-artifacts");
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x77);
    let budget = 12;
    for case in 0..budget {
        let root = scratch.child(&format!("case-{case}"));
        let package_dir = root.join("pkg");
        let config = format!("{{\"n\":{}}}\n", rng.below(1000)).into_bytes();
        let mut manifest = base_manifest(&format!("example.refprop-{case}"));
        manifest["config_schema"] =
            byte_ref("objects/config-schema.json", &config, "application/json");
        let mut files: Vec<(String, Vec<u8>)> = standard_files()
            .into_iter()
            .filter(|(path, _)| path != "objects/config-schema.json")
            .collect();
        files.push(("objects/config-schema.json".to_string(), config.clone()));
        seal_package(&package_dir, &manifest, &files);
        assert!(
            discover(&[required(&root)]).is_ok(),
            "case {case}: valid package must discover"
        );

        let mut tampered = config.clone();
        tampered[0] ^= 0xff;
        fs::write(package_dir.join("objects/config-schema.json"), &tampered).unwrap();
        let err = discover(&[required(&root)]).unwrap_err();
        assert_eq!(
            code(&err),
            FailureCode::IntegrityMismatch,
            "case {case}: tampered artifact must refuse"
        );
    }
}

/// Generated nested directory-symlink trees (including ancestor links and links
/// back to the package root) must refuse with a typed error instead of
/// recursing and overflowing the stack.
#[cfg(unix)]
#[test]
fn property_directory_symlink_cycles_refuse() {
    let scratch = fresh("hp-manifest-prop-symlink");
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x1234);
    let budget = 8;
    for case in 0..budget {
        let root = scratch.child(&format!("case-{case}"));
        let package_dir = root.join("pkg");
        simple_package(&package_dir, &format!("example.cycle-{case}"));
        let depth = 1 + rng.below(3) as usize;
        let mut current = package_dir.clone();
        for level in 0..depth {
            current = current.join(format!("d{level}"));
            fs::create_dir_all(&current).unwrap();
            let target = if rng.below(2) == 0 {
                package_dir.clone()
            } else {
                PathBuf::from("..")
            };
            std::os::unix::fs::symlink(&target, current.join("back")).unwrap();
        }
        let err = discover(&[required(&root)]).unwrap_err();
        assert_eq!(
            code(&err),
            FailureCode::ScopeDenied,
            "case {case}: directory symlink must refuse"
        );
    }
}

/// Clearing `media_type` on any pinned artifact reference (config schema,
/// default configuration, or a report schema) must refuse with `InvalidRecord`
/// regardless of which reference is affected.
#[test]
fn property_empty_media_type_refuses() {
    let scratch = fresh("hp-manifest-prop-media-type");
    let mut rng = XorShift64::new(PROPERTY_SEED ^ 0x9999);
    let budget = 6;
    for case in 0..budget {
        let root = scratch.child(&format!("case-{case}"));
        let package_dir = root.join("pkg");
        let mut manifest = base_manifest(&format!("example.media-{case}"));
        match rng.below(3) {
            0 => manifest["config_schema"]["media_type"] = json!(""),
            1 => manifest["default_configuration"]["media_type"] = json!(""),
            _ => manifest["report_schemas"][0]["schema"]["media_type"] = json!(""),
        }
        seal_package(&package_dir, &manifest, &standard_files());
        let err = discover(&[required(&root)]).unwrap_err();
        assert_eq!(code(&err), FailureCode::InvalidRecord, "case {case}");
    }
}
