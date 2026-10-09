//! S0d spike entry point: binding selection, driver-side binding verification,
//! scenario execution, and transcript output.
//!
//! Usage:
//!   s0d --decisions-smoke
//!   s0d --config <binding.json> [--run-dir <dir>] [--workspace <dir>] [--out <file>]
//!
//! The driver verifies the declared API/capability needs against the adapter's
//! `Describe` before running; a missing required capability refuses to bind.

#![allow(dead_code)]

mod contract;
mod decisions;
mod json;
mod opencode;
mod reference;
mod scenario;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use contract::{Describe, HarnessAdapter};
use json::Json;

struct BindingConfig {
    binding_id: String,
    component_api: String,
    api_version: u32,
    implementation: String,
    implementation_version: String,
    capabilities: Vec<String>,
    required_capabilities: Vec<String>,
    optional_capabilities: Vec<String>,
    config_hash: String,
}

fn load_config(path: &str) -> Result<BindingConfig, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
    let root = Json::parse(&text).map_err(|e| format!("parse {path}: {e}"))?;
    let string = |key: &str| root.get(key).and_then(Json::as_str).unwrap_or("").to_string();
    let strings = |key: &str| -> Vec<String> {
        root.get(key)
            .and_then(Json::as_arr)
            .map(|items| items.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default()
    };
    Ok(BindingConfig {
        binding_id: string("binding_id"),
        component_api: string("component_api"),
        api_version: root.get("api_version").and_then(Json::as_u64).unwrap_or(u32::MAX as u64) as u32,
        implementation: string("implementation"),
        implementation_version: string("implementation_version"),
        capabilities: strings("capabilities"),
        required_capabilities: strings("required_capabilities"),
        optional_capabilities: strings("optional_capabilities"),
        config_hash: string("config_hash"),
    })
}

/// Driver-side bind check. Refuses when the declared API or a required
/// capability is not satisfied by `Describe`.
fn verify_binding(config: &BindingConfig, describe: &Describe) -> Result<(), String> {
    if describe.api != config.component_api {
        return Err(format!(
            "refuse to bind: api mismatch config={} describe={}",
            config.component_api, describe.api
        ));
    }
    if describe.api_version != config.api_version {
        return Err(format!(
            "refuse to bind: api_version mismatch config={} describe={}",
            config.api_version, describe.api_version
        ));
    }
    let advertised: std::collections::BTreeSet<&str> =
        describe.capabilities.iter().map(String::as_str).collect();
    for cap in &config.required_capabilities {
        if !advertised.contains(cap.as_str()) {
            return Err(format!("refuse to bind: required capability missing: {cap}"));
        }
    }
    let declared: std::collections::BTreeSet<&str> =
        config.capabilities.iter().map(String::as_str).collect();
    if declared != advertised {
        return Err(format!(
            "refuse to bind: capability advertisement mismatch config={:?} describe={:?}",
            declared, advertised
        ));
    }
    Ok(())
}

fn prepare_workspace(path: &str) -> Result<(), String> {
    let dir = PathBuf::from(path);
    fs::create_dir_all(&dir).map_err(|e| format!("mkdir {}: {e}", dir.display()))?;
    if !dir.join(".git").exists() {
        let _ = Command::new("git").arg("init").arg("-q").current_dir(&dir).status();
        let _ = fs::write(dir.join("README.md"), "s0d attempt fixture\n");
        let _ = Command::new("git").args(["add", "-A"]).current_dir(&dir).status();
        let _ = Command::new("git")
            .args(["-c", "user.email=s0d@local", "-c", "user.name=s0d", "commit", "-qm", "fixture"])
            .current_dir(&dir)
            .status();
    }
    Ok(())
}

fn format_transcript(config: &BindingConfig, report: &scenario::ScenarioReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("binding: {}\n", config.binding_id));
    out.push_str(&format!("implementation: {} {}\n", report.impl_name, report.impl_version));
    out.push_str(&format!("implementation_version: {}\n", config.implementation_version));
    out.push_str(&format!("config_hash: {}\n", config.config_hash));
    out.push_str(&format!("api: {} v{}\n", config.component_api, config.api_version));
    let caps = if report.capabilities.is_empty() {
        "none".to_string()
    } else {
        report.capabilities.join(",")
    };
    out.push_str(&format!("capabilities: {caps}\n"));
    out.push_str(&format!("optional_capabilities: {}\n", config.optional_capabilities.join(",")));
    out.push_str(&format!("required_capabilities: {}\n", config.required_capabilities.join(",")));

    out.push_str("events:\n");
    for event in &report.events {
        out.push_str(&format!("  {event}\n"));
    }

    out.push_str("checks:\n");
    for (name, ok) in &report.checks {
        out.push_str(&format!("  {} {}\n", if *ok { "PASS" } else { "FAIL" }, name));
    }

    if let Some(fallback) = &report.fallback {
        out.push_str(&format!("fallback: {fallback}\n"));
    }
    out.push_str(&format!("cleanup: {}\n", report.cleanup));
    if let Some(error) = &report.error {
        out.push_str(&format!("error: {error}\n"));
    }
    out.push_str(&format!("result: {}\n", if report.pass() { "PASS" } else { "FAIL" }));
    out
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

/// Make a path absolute relative to the current directory and canonicalize it so
/// the spawned adapter and the server agree on the same locations.
fn absolutize(path: &str) -> String {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")).join(p)
    };
    fs::canonicalize(&abs).unwrap_or(abs).to_string_lossy().into_owned()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.iter().any(|a| a == "--decisions-smoke") {
        std::process::exit(decisions::smoke_main());
    }

    let config_path = match arg_value(&args, "--config") {
        Some(p) => p,
        None => {
            eprintln!("usage: s0d --config <binding.json> [--run-dir <dir>] [--workspace <dir>] [--out <file>]");
            eprintln!("       s0d --decisions-smoke");
            std::process::exit(2);
        }
    };
    let run_dir = arg_value(&args, "--run-dir").unwrap_or_else(|| "runs/current".to_string());
    let workspace = arg_value(&args, "--workspace").unwrap_or_else(|| {
        Path::new(&run_dir).join("ws").to_string_lossy().into_owned()
    });
    let out_path = arg_value(&args, "--out");

    if let Err(e) = prepare_workspace(&workspace) {
        eprintln!("s0d: {e}");
        std::process::exit(2);
    }
    // Canonicalize after creation so the adapter, the server, and the driver all
    // use the same absolute locations.
    let run_dir = absolutize(&run_dir);
    let workspace = absolutize(&workspace);

    let config = match load_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("s0d: {e}");
            std::process::exit(2);
        }
    };

    let mut adapter: Box<dyn HarnessAdapter> = match config.implementation.as_str() {
        "reference" => Box::new(reference::ReferenceAdapter::new(&workspace)),
        "opencode" => Box::new(opencode::OpenCodeAdapter::new(&run_dir, &workspace)),
        other => {
            eprintln!("s0d: unknown implementation {other:?}");
            std::process::exit(2);
        }
    };

    let describe = adapter.describe();
    if let Err(e) = verify_binding(&config, &describe) {
        eprintln!("s0d: {e}");
        drop(adapter);
        std::process::exit(3);
    }

    let report = scenario::run(adapter.as_mut(), &config.binding_id, &workspace);
    let transcript = format_transcript(&config, &report);

    if let Some(path) = &out_path {
        if let Some(parent) = Path::new(path).parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(path, &transcript);
    }
    print!("{transcript}");

    // Drop the adapter *before* exiting so `Drop` cleanup runs even on failure
    // (`std::process::exit` does not run destructors).
    let code = if report.pass() { 0 } else { 1 };
    drop(adapter);
    std::process::exit(code);
}
