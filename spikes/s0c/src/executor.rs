//! Execution model: deterministic transitions, stable operation IDs, durable
//! receipts, dispatch intents, restart reconciliation, and the kill latch.
//!
//! Every transition commits the state update, the event, the operation receipt,
//! and the dispatch intent in one SQLite transaction (`Db::script(atomic(..))`).
//! Recovery reconstructs state from recorded events without performing I/O,
//! then reconciles pending operations.

use std::io::Write;
use std::path::PathBuf;

use crate::ledger::{atomic, q, qi, Db, EXECUTION_VERSION, SCHEMA_VERSION};
use crate::util::{input_hash, maybe_crash, now_ms};

pub const STATE_CREATED: &str = "created";
pub const STATE_BUILDING: &str = "building";
pub const STATE_VERIFYING: &str = "verifying";
pub const STATE_PARKED: &str = "parked";

#[derive(Debug, Clone, PartialEq)]
pub enum Delivery {
    Executed(String),
    Recorded(String),
    Reconciled(String),
    Blocked,
    Parked(String),
}

#[derive(Debug)]
pub struct Intent {
    pub op_id: String,
    #[allow(dead_code)] // retained: the durable intent payload is part of the record
    pub payload: String,
}

pub struct Compat {
    pub schema_ok: bool,
    pub detail: String,
}

struct OpRow {
    execution_id: String,
    state: String,
    input_hash: String,
    result: Option<String>,
}

pub struct Executor {
    pub db: Db,
    pub effects: PathBuf,
}

impl Executor {
    pub fn new(db_path: PathBuf, effects_path: PathBuf) -> Self {
        Executor {
            db: Db::new(db_path),
            effects: effects_path,
        }
    }

    /// Create the schema and pin the versions. Idempotent.
    pub fn init(&self) -> Result<(), String> {
        let schema = format!(
            r#"
PRAGMA journal_mode=WAL;
PRAGMA synchronous=FULL;
PRAGMA busy_timeout=5000;
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS executions (
  execution_id TEXT PRIMARY KEY,
  task_id TEXT NOT NULL,
  execution_version INTEGER NOT NULL,
  phase TEXT NOT NULL,
  state TEXT NOT NULL,
  lease_generation INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS operations (
  operation_id TEXT PRIMARY KEY,
  execution_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  inputs_json TEXT NOT NULL,
  input_hash TEXT NOT NULL,
  state TEXT NOT NULL,
  result_json TEXT,
  execution_version INTEGER NOT NULL,
  started INTEGER,
  completed INTEGER
);
CREATE TABLE IF NOT EXISTS execution_events (
  execution_id TEXT NOT NULL,
  sequence INTEGER NOT NULL,
  kind TEXT NOT NULL,
  payload TEXT NOT NULL,
  recorded_at INTEGER NOT NULL,
  PRIMARY KEY (execution_id, sequence)
);
CREATE TABLE IF NOT EXISTS dispatch_intents (
  operation_id TEXT PRIMARY KEY,
  execution_id TEXT NOT NULL,
  payload TEXT NOT NULL,
  enqueued_at INTEGER NOT NULL,
  consumed INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS kill_latch (
  id INTEGER PRIMARY KEY CHECK (id = 1),
  active INTEGER NOT NULL,
  reason TEXT,
  updated_at INTEGER NOT NULL
);
INSERT OR IGNORE INTO meta(key,value) VALUES ('schema_version','{}');
INSERT OR IGNORE INTO meta(key,value) VALUES ('execution_definition_version','{}');
INSERT OR IGNORE INTO kill_latch(id,active,reason,updated_at) VALUES (1,0,NULL,0);
"#,
            SCHEMA_VERSION, EXECUTION_VERSION
        );
        self.db.script(&schema)?;
        Ok(())
    }

    // ---- meta / compatibility -------------------------------------------------

    pub fn meta(&self, key: &str) -> Result<Option<String>, String> {
        self.db
            .scalar(&format!("SELECT value FROM meta WHERE key={}", q(key)))
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<(), String> {
        let s = format!(
            "INSERT INTO meta(key,value) VALUES ({},{}) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            q(key), q(value)
        );
        self.db.script(&atomic(&[s]))?;
        Ok(())
    }

    pub fn compat(&self) -> Compat {
        let v = self.meta("schema_version").ok().flatten();
        let schema_ok = v.as_deref() == Some(&SCHEMA_VERSION.to_string());
        Compat {
            detail: format!(
                "recorded_schema_version={} expected={}",
                v.unwrap_or_else(|| "<none>".into()),
                SCHEMA_VERSION
            ),
            schema_ok,
        }
    }

    // ---- execution lifecycle --------------------------------------------------

    /// Atomically insert the execution row and its `execution_created` event.
    pub fn start(&self, task_id: &str, execution_id: &str) -> Result<(), String> {
        let ts = now_ms();
        let payload = format!(
            "{{\"execution_id\":\"{execution_id}\",\"task_id\":\"{task_id}\",\"execution_version\":{EXECUTION_VERSION}}}"
        );
        let stmts = vec![
            format!(
                "INSERT INTO executions(execution_id,task_id,execution_version,phase,state,lease_generation,updated_at) VALUES ({},{},{},{},{},0,{})",
                q(execution_id), q(task_id), qi(EXECUTION_VERSION), q(STATE_CREATED), q(STATE_CREATED), qi(ts)
            ),
            format!(
                "INSERT INTO execution_events(execution_id,sequence,kind,payload,recorded_at) VALUES ({},1,'execution_created',{}, {})",
                q(execution_id), q(&payload), qi(ts)
            ),
        ];
        self.db.script(&atomic(&stmts))?;
        Ok(())
    }

    /// The single atomic transition commit.
    ///
    /// Commits, in one transaction: the execution state/phase update, the
    /// ordered event, the operation receipt (state `intended`), and the durable
    /// dispatch intent. Duplicate calls for the same logical step are
    /// idempotent when the input hash matches and rejected when it differs.
    pub fn advance(
        &self,
        execution_id: &str,
        next_phase: &str,
        op_kind: &str,
        inputs_json: &str,
    ) -> Result<String, String> {
        let op_id = format!("op:{execution_id}:{op_kind}");
        let hash = input_hash(inputs_json);

        if let Some(row) = self.db.row(&format!(
            "SELECT state,input_hash FROM operations WHERE operation_id={}",
            q(&op_id)
        ))? {
            let mut parts = row.split('|');
            let _state = parts.next().unwrap_or("");
            let existing = parts.next().unwrap_or("");
            if existing == hash {
                // Idempotent re-advance: same operation, same inputs.
                return Ok(op_id);
            }
            return Err(format!(
                "operation ID {op_id} reused with different inputs (existing={existing}, new={hash})"
            ));
        }

        let current_phase = self
            .exec_phase(execution_id)?
            .ok_or_else(|| format!("unknown execution {execution_id}"))?;
        let ts = now_ms();
        let ev_payload = format!(
            "{{\"from\":\"{current_phase}\",\"to\":\"{next_phase}\",\"op\":\"{op_id}\"}}"
        );
        let intent_payload = format!(
            "{{\"operation_id\":\"{op_id}\",\"execution_id\":\"{execution_id}\",\"kind\":\"{op_kind}\"}}"
        );

        let stmts = vec![
            format!(
                "UPDATE executions SET phase={},state={},updated_at={} WHERE execution_id={}",
                q(next_phase), q(next_phase), qi(ts), q(execution_id)
            ),
            format!(
                "INSERT INTO execution_events(execution_id,sequence,kind,payload,recorded_at) \
                 VALUES ({},(SELECT COALESCE(MAX(sequence),0)+1 FROM execution_events WHERE execution_id={}),\
                 'phase_advanced',{}, {})",
                q(execution_id), q(execution_id), q(&ev_payload), qi(ts)
            ),
            format!(
                "INSERT INTO operations(operation_id,execution_id,kind,inputs_json,input_hash,state,execution_version,started) \
                 VALUES ({},{},{},{},{},'intended',{}, {})",
                q(&op_id), q(execution_id), q(op_kind), q(inputs_json), q(&hash), qi(EXECUTION_VERSION), qi(ts)
            ),
            format!(
                "INSERT INTO dispatch_intents(operation_id,execution_id,payload,enqueued_at,consumed) \
                 VALUES ({},{},{},{},0)",
                q(&op_id), q(execution_id), q(&intent_payload), qi(ts)
            ),
        ];
        self.db.script(&atomic(&stmts))?;
        Ok(op_id)
    }

    // ---- inspection -----------------------------------------------------------

    pub fn exec_phase(&self, execution_id: &str) -> Result<Option<String>, String> {
        self.db.scalar(&format!(
            "SELECT phase FROM executions WHERE execution_id={}",
            q(execution_id)
        ))
    }

    pub fn exec_state(&self, execution_id: &str) -> Result<Option<String>, String> {
        self.db.scalar(&format!(
            "SELECT state FROM executions WHERE execution_id={}",
            q(execution_id)
        ))
    }

    pub fn exec_version(&self, execution_id: &str) -> Result<Option<i64>, String> {
        Ok(self
            .db
            .scalar(&format!(
                "SELECT execution_version FROM executions WHERE execution_id={}",
                q(execution_id)
            ))?
            .and_then(|s| s.parse().ok()))
    }

    /// Simulate a record written by an incompatible execution version.
    pub fn force_exec_version(&self, execution_id: &str, version: i64) -> Result<(), String> {
        self.db.script(&atomic(&[format!(
            "UPDATE executions SET execution_version={} WHERE execution_id={}",
            qi(version),
            q(execution_id)
        )]))?;
        Ok(())
    }

    fn op_row(&self, op_id: &str) -> Result<Option<OpRow>, String> {
        let row = self.db.row(&format!(
            "SELECT execution_id,state,input_hash,COALESCE(result_json,'') \
             FROM operations WHERE operation_id={}",
            q(op_id)
        ))?;
        Ok(row.map(|r| {
            let mut p = r.split('|');
            OpRow {
                execution_id: p.next().unwrap_or("").to_string(),
                state: p.next().unwrap_or("").to_string(),
                input_hash: p.next().unwrap_or("").to_string(),
                result: {
                    let v = p.next().unwrap_or("");
                    if v.is_empty() {
                        None
                    } else {
                        Some(v.to_string())
                    }
                },
            }
        }))
    }

    pub fn op_state(&self, op_id: &str) -> Result<Option<String>, String> {
        Ok(self.op_row(op_id)?.map(|r| r.state))
    }

    pub fn op_result(&self, op_id: &str) -> Result<Option<String>, String> {
        Ok(self.op_row(op_id)?.and_then(|r| r.result))
    }

    /// Unconsumed dispatch intents, optionally scoped to one execution.
    pub fn pending(&self, execution_id: Option<&str>) -> Result<Vec<Intent>, String> {
        let where_clause = match execution_id {
            Some(e) => format!("WHERE consumed=0 AND execution_id={}", q(e)),
            None => "WHERE consumed=0".to_string(),
        };
        let out = self.db.query(&format!(
            "SELECT operation_id,payload FROM dispatch_intents {where_clause} ORDER BY enqueued_at,operation_id"
        ))?;
        let mut v = Vec::new();
        for line in out.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut p = line.splitn(2, '|');
            v.push(Intent {
                op_id: p.next().unwrap_or("").to_string(),
                payload: p.next().unwrap_or("").to_string(),
            });
        }
        Ok(v)
    }

    // ---- dispatch / delivery --------------------------------------------------

    /// Deliver one logical operation.
    ///
    /// - completed  -> recorded result is returned, effect not repeated
    /// - parked     -> stays parked
    /// - incompatible schema/execution version -> parked
    /// - kill latch -> blocked, no effect
    /// - effect present but no completion -> reconciled (no repeat)
    /// - otherwise  -> performs the effect, then records completion
    pub fn deliver(&self, op_id: &str) -> Result<Delivery, String> {
        let row = self
            .op_row(op_id)?
            .ok_or_else(|| format!("unknown operation {op_id}"))?;

        match row.state.as_str() {
            "completed" => {
                return Ok(Delivery::Recorded(
                    row.result.unwrap_or_else(|| "{}".into()),
                ))
            }
            "parked" => {
                return Ok(Delivery::Parked(
                    row.result.unwrap_or_else(|| "{\"status\":\"parked\"}".into()),
                ))
            }
            _ => {}
        }

        if !self.compat().schema_ok {
            self.park(op_id, &row.execution_id, "incompatible schema version")?;
            return Ok(Delivery::Parked(format!(
                "parked: incompatible schema ({})",
                self.compat().detail
            )));
        }
        let ev = self.exec_version(&row.execution_id)?.unwrap_or(0);
        if ev != EXECUTION_VERSION {
            self.park(
                op_id,
                &row.execution_id,
                &format!("incompatible execution version {ev} (expected {EXECUTION_VERSION})"),
            )?;
            return Ok(Delivery::Parked(format!(
                "parked: incompatible execution version {ev}"
            )));
        }

        if self.kill_active()? {
            return Ok(Delivery::Blocked);
        }

        // Reconcile an effect that outlived a crash before its completion record.
        if self.effect_present(op_id) {
            let result = format!(
                "{{\"operation_id\":\"{op_id}\",\"status\":\"reconciled\",\"execution_version\":{EXECUTION_VERSION}}}"
            );
            self.complete(op_id, &row.execution_id, &result, true)?;
            return Ok(Delivery::Reconciled(result));
        }

        // Perform the external effect, then persist completion.
        self.perform_effect(op_id, &row.input_hash)?;
        // Injection point: crash between effect and completion record.
        maybe_crash("after_effect_before_completion");
        let result = format!(
            "{{\"operation_id\":\"{op_id}\",\"status\":\"executed\",\"execution_version\":{EXECUTION_VERSION}}}"
        );
        self.complete(op_id, &row.execution_id, &result, false)?;
        Ok(Delivery::Executed(result))
    }

    /// Record completion atomically: receipt + consumed intent + event.
    fn complete(
        &self,
        op_id: &str,
        execution_id: &str,
        result: &str,
        reconciled: bool,
    ) -> Result<(), String> {
        let ts = now_ms();
        let kind = if reconciled {
            "operation_reconciled"
        } else {
            "operation_completed"
        };
        let payload = format!(
            "{{\"operation_id\":\"{op_id}\",\"status\":\"{}\",\"reconciled\":{reconciled}}}",
            if reconciled { "reconciled" } else { "executed" }
        );
        let stmts = vec![
            format!(
                "UPDATE operations SET state='completed',result_json={},completed={} WHERE operation_id={}",
                q(result), qi(ts), q(op_id)
            ),
            format!(
                "UPDATE dispatch_intents SET consumed=1 WHERE operation_id={}",
                q(op_id)
            ),
            format!(
                "INSERT INTO execution_events(execution_id,sequence,kind,payload,recorded_at) \
                 VALUES ({},(SELECT COALESCE(MAX(sequence),0)+1 FROM execution_events WHERE execution_id={}),\
                 {}, {}, {})",
                q(execution_id), q(execution_id), q(kind), q(&payload), qi(ts)
            ),
        ];
        self.db.script(&atomic(&stmts))?;
        Ok(())
    }

    /// Park an operation and its execution. No effect is performed.
    fn park(&self, op_id: &str, execution_id: &str, reason: &str) -> Result<(), String> {
        let ts = now_ms();
        let result = format!(
            "{{\"operation_id\":\"{op_id}\",\"status\":\"parked\",\"reason\":\"{reason}\"}}"
        );
        let payload = format!("{{\"operation_id\":\"{op_id}\",\"reason\":\"{reason}\"}}");
        let stmts = vec![
            format!(
                "UPDATE operations SET state='parked',result_json={} WHERE operation_id={}",
                q(&result), q(op_id)
            ),
            format!(
                "UPDATE executions SET state='parked',updated_at={} WHERE execution_id={}",
                qi(ts), q(execution_id)
            ),
            format!(
                "INSERT INTO execution_events(execution_id,sequence,kind,payload,recorded_at) \
                 VALUES ({},(SELECT COALESCE(MAX(sequence),0)+1 FROM execution_events WHERE execution_id={}),\
                 'parked',{}, {})",
                q(execution_id), q(execution_id), q(&payload), qi(ts)
            ),
        ];
        self.db.script(&atomic(&stmts))?;
        Ok(())
    }

    // ---- kill latch -----------------------------------------------------------

    pub fn kill_active(&self) -> Result<bool, String> {
        Ok(self
            .db
            .scalar("SELECT active FROM kill_latch WHERE id=1")?
            .map(|v| v == "1")
            .unwrap_or(false))
    }

    pub fn set_kill(&self, active: bool, reason: &str) -> Result<(), String> {
        let ts = now_ms();
        self.db.script(&atomic(&[format!(
            "UPDATE kill_latch SET active={},reason={},updated_at={} WHERE id=1",
            if active { 1 } else { 0 },
            q(reason),
            qi(ts)
        )]))?;
        Ok(())
    }

    // ---- restart reconciliation ----------------------------------------------

    /// Reconstruct the execution state by replaying recorded events only. No
    /// external I/O is performed here.
    pub fn reconstruct_state(&self, execution_id: &str) -> Result<(String, String), String> {
        let events = self.db.query(&format!(
            "SELECT kind,payload FROM execution_events WHERE execution_id={} ORDER BY sequence",
            q(execution_id)
        ))?;
        let mut derived = STATE_CREATED.to_string();
        for line in events.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let mut p = line.splitn(2, '|');
            let kind = p.next().unwrap_or("");
            let payload = p.next().unwrap_or("");
            match kind {
                "execution_created" => derived = STATE_CREATED.to_string(),
                "phase_advanced" => {
                    if let Some(to) = payload_field(payload, "to") {
                        derived = to;
                    }
                }
                "parked" => derived = STATE_PARKED.to_string(),
                _ => {}
            }
        }
        let projected = self
            .exec_state(execution_id)?
            .unwrap_or_else(|| "<missing>".to_string());
        Ok((derived, projected))
    }

    // ---- external effect (fake) ----------------------------------------------

    pub fn effect_present(&self, op_id: &str) -> bool {
        match std::fs::read_to_string(&self.effects) {
            Ok(s) => s
                .lines()
                .any(|l| l.split('\t').next() == Some(op_id)),
            Err(_) => false,
        }
    }

    pub fn effect_count(&self) -> usize {
        std::fs::read_to_string(&self.effects)
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }

    fn perform_effect(&self, op_id: &str, hash: &str) -> Result<(), String> {
        if self.effect_present(op_id) {
            return Ok(());
        }
        if let Some(parent) = self.effects.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create effects dir: {e}"))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.effects)
            .map_err(|e| format!("open effects file: {e}"))?;
        writeln!(f, "{op_id}\t{hash}").map_err(|e| format!("append effect: {e}"))?;
        f.sync_all().map_err(|e| format!("sync effect: {e}"))?;
        Ok(())
    }
}

/// Extract `"key":"value"` from a compact JSON payload.
fn payload_field(payload: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let start = payload.find(&needle)? + needle.len();
    let rest = &payload[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}
