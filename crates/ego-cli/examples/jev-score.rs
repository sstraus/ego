//! One offline command; scheduling belongs to the coordinator.
use clap::{Parser, Subcommand};
use ego_ai::{
    http::{Http, HttpCall, PreparedHttpCall},
    jev::{
        ConfiguredDecisions,
        calibration::{self, Sample, Selection, Split},
    },
};
use ego_cli::provider_http::ProviderHttpAdapter;
use ego_core::{
    config::{JevConfig, JevRoot, JevUse, JevUseOverride},
    event::id::SessionId,
    permit::{
        CanonPath, Confinement, OneHopResponse, RunConfinement, TransportError, TransportFailure,
    },
    turn::CancelToken,
};
use ego_tools::{
    exec::{ExecLimits, ExecRunConfig, HttpCallTimeout, RunEffectPorts},
    http::SystemResolver,
    store::SessionEvidenceStore,
};
use futures::{StreamExt, future::BoxFuture};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    /// Select prompts and bands using calibration rows only.
    Calibrate {
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        ledger: PathBuf,
    },
    /// Score the frozen held-out once; never adjust the selection here.
    HeldOut {
        #[arg(long)]
        selection: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        ledger: PathBuf,
    },
    /// Append a coordinator-labelled wake to calibration; then schedule this command again.
    Append {
        #[arg(long)]
        sample: PathBuf,
    },
}
const SETS: [(&str, &str, JevUse); 3] = [
    ("wake", "pump", JevUse::Wake),
    ("destructive", "commands", JevUse::Destructive),
    ("router", "turns", JevUse::Router),
];
type Error = Box<dyn std::error::Error>;
type Record = Arc<Mutex<Option<Value>>>;
struct MeasuredHttp {
    inner: Arc<dyn Http>,
    record: Record,
}
struct MeasuredCall {
    inner: Box<dyn HttpCall>,
    record: Record,
}
impl Http for MeasuredHttp {
    fn begin_call(&self) -> Box<dyn HttpCall> {
        Box::new(MeasuredCall {
            inner: self.inner.begin_call(),
            record: self.record.clone(),
        })
    }
}
impl HttpCall for MeasuredCall {
    fn execute(
        &mut self,
        call: PreparedHttpCall,
    ) -> BoxFuture<'static, Result<OneHopResponse, TransportError>> {
        let future = self.inner.execute(call);
        let record = self.record.clone();
        Box::pin(async move {
            match future.await? {
                OneHopResponse::Response {
                    status,
                    headers,
                    mut body,
                    ..
                } => {
                    let mut bytes = Vec::new();
                    while let Some(chunk) = body.next().await {
                        let chunk = chunk?;
                        if bytes.len() + chunk.len() > 64 * 1024 {
                            return Err(TransportError::definitely_unsent(
                                TransportFailure::AuthorityMismatch,
                            ));
                        }
                        bytes.extend(chunk);
                    }
                    // Save only response metadata. Requests, headers and judged texts stay out.
                    if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                        *record.lock().expect("measurement lock") = Some(
                            json!({"status":status,"model":value["model"],"usage":value["usage"]}),
                        );
                    }
                    Ok(OneHopResponse::response(status, headers, bytes))
                }
                redirect => Ok(redirect),
            }
        })
    }
}
fn read_set(name: &str) -> Result<Vec<Sample>, Error> {
    let manifest: Value = serde_json::from_slice(&fs::read("data/jev/manifest.json")?)?;
    let frozen = manifest[name]["held_out_sha256"]
        .as_str()
        .ok_or("missing frozen hash")?;
    Ok(calibration::load_frozen(
        &fs::read_to_string(format!("data/jev/{name}.jsonl"))?,
        frozen,
    )?)
}
fn write_new(path: &Path, value: &Value) -> Result<(), Error> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(serde_json::to_string_pretty(value)?.as_bytes())?;
    file.sync_all()?;
    Ok(())
}
fn spend(path: &Path) -> Result<f64, Error> {
    let mut total = 0.0;
    if path.exists() {
        for line in fs::read_to_string(path)?.lines() {
            let value: Value = serde_json::from_str(line)?;
            let cost = value["response"]["usage"]["cost"]
                .as_f64()
                .filter(|c| c.is_finite() && *c >= 0.0)
                .ok_or("unpriced previous call; stop")?;
            total += cost;
        }
    }
    Ok(total)
}
#[tokio::main]
async fn main() -> Result<(), Error> {
    let args = Args::parse();
    if let Command::Append { sample } = &args.command {
        let mut rows = read_set("pump")?;
        let row: Sample = serde_json::from_slice(&fs::read(sample)?)?;
        if row.split != Split::Calibration {
            return Err("new wakes must join calibration, not frozen held-out".into());
        }
        rows.push(row.clone());
        let updated = rows
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<Vec<_>, _>>()?
            .join("\n")
            + "\n";
        let manifest: Value = serde_json::from_slice(&fs::read("data/jev/manifest.json")?)?;
        calibration::load_frozen(
            &updated,
            manifest["pump"]["held_out_sha256"]
                .as_str()
                .ok_or("missing frozen hash")?,
        )?;
        let mut file = OpenOptions::new()
            .append(true)
            .open("data/jev/pump.jsonl")?;
        writeln!(file, "{}", serde_json::to_string(&row)?)?;
        file.sync_all()?;
        println!(
            "appended {}; label must record whether the coordinator acted",
            row.id
        );
        return Ok(());
    }
    let (output, ledger, chosen) = match &args.command {
        Command::Calibrate { output, ledger } => (output, ledger, None),
        Command::HeldOut {
            selection,
            output,
            ledger,
        } => (
            output,
            ledger,
            Some(serde_json::from_slice::<Value>(&fs::read(selection)?)?),
        ),
        Command::Append { .. } => unreachable!(),
    };
    if output.exists() {
        return Err("output already exists; held-out evidence is immutable".into());
    }
    let mut total = spend(ledger)?;
    // Own production effect ports without an MCP runtime, journal, service or scheduler.
    let evidence_dir = tempfile::tempdir()?;
    let evidence =
        SessionEvidenceStore::open(evidence_dir.path(), SessionId::new(), 64 * 1024 * 1024)?;
    let root = fs::canonicalize(std::env::current_dir()?)?;
    let policy = RunConfinement::new(Confinement::Off, false, CanonPath::new(&root))?;
    let config = ExecRunConfig::new(
        policy,
        evidence,
        CancelToken::new(),
        ExecLimits::default(),
        HttpCallTimeout::default(),
        None,
        Arc::new(SystemResolver::new()),
    );
    let (ports, _, _) = RunEffectPorts::for_run(config).await?;
    let record = Arc::new(Mutex::new(None));
    let http = MeasuredHttp {
        inner: Arc::new(ProviderHttpAdapter::new(Arc::new(ports))),
        record: record.clone(),
    };
    // Explicit offline-evaluation root override; shipped defaults stay independently gated.
    let enabled = JevUseOverride {
        on: Some(true),
        ..Default::default()
    };
    let mut config = JevConfig::default();
    config.roots.push(JevRoot {
        path: root.to_string_lossy().into(),
        endpoint: None,
        wake: Some(enabled.clone()),
        destructive: Some(enabled.clone()),
        router: Some(enabled),
    });
    let client = ConfiguredDecisions::new(
        config,
        PathBuf::from(std::env::var("HOME")?),
        Duration::from_secs(20),
    );
    let mut result =
        json!({"phase":if chosen.is_some() {"held_out"} else {"calibration"},"uses":{}});
    for (usage, name, kind) in SETS {
        let rows = read_set(name)?;
        let calibration_hash = calibration::fingerprint(&rows, Split::Calibration)?;
        let held_hash = calibration::fingerprint(&rows, Split::HeldOut)?;
        let selected = chosen
            .as_ref()
            .map(|c| serde_json::from_value::<Selection>(c["uses"][usage]["selection"].clone()))
            .transpose()?;
        if let Some(s) = &selected {
            if s.calibration_sha256 != calibration_hash || s.held_out_sha256 != held_hash {
                return Err(
                    "dataset changed since selection; recalibrate without changing frozen held-out"
                        .into(),
                );
            }
            calibration::score(&[], s.low, s.high)?;
        }
        let prompts: Vec<String> = selected.as_ref().map(|s| vec![s.prompt.clone()]).unwrap_or(
            calibration::prompts(usage)?
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        );
        let split = if chosen.is_some() {
            Split::HeldOut
        } else {
            Split::Calibration
        };
        let mut variants = Vec::new();
        for prompt in prompts {
            let mut values = Vec::new();
            let mut probabilities = Vec::new();
            for row in rows.iter().filter(|r| r.split == split && !r.rule_settled) {
                if total + 0.01 > 1.0 {
                    return Err("USD 1 spend cap reached (USD .01 call reserve)".into());
                }
                if row.text.len() > 4096 {
                    return Err("sample exceeds bounded scoring input".into());
                }
                *record.lock().expect("measurement lock") = None;
                let answer = client
                    .ask(
                        &http,
                        &root,
                        kind,
                        true,
                        &json!({"text":row.text}),
                        &BTreeMap::from([("decision".into(), prompt.clone())]),
                    )
                    .await;
                let metadata = record.lock().expect("measurement lock").take();
                let mut log = OpenOptions::new().create(true).append(true).open(ledger)?;
                writeln!(
                    log,
                    "{}",
                    json!({"use":usage,"sample":row.id,"phase":result["phase"],"response":metadata})
                )?;
                log.sync_all()?;
                let cost = metadata
                    .as_ref()
                    .and_then(|v| v["usage"]["cost"].as_f64())
                    .filter(|c| c.is_finite() && *c >= 0.0)
                    .ok_or("unpriced call recorded; no further calls allowed")?;
                total += cost;
                let answer = answer?.ok_or("evaluation override unexpectedly disabled")?;
                let p = *answer.get("decision").ok_or("missing decision")?;
                values.push((row.label, p));
                probabilities.push(json!({"id":row.id,"label":row.label,"probability":p}));
            }
            let (low, high, metrics) = if let Some(s) = &selected {
                (s.low, s.high, calibration::score(&values, s.low, s.high)?)
            } else {
                calibration::tune(&values)?
            };
            variants.push(json!({"selection":Selection { prompt, low, high, calibration_sha256:calibration_hash.clone(), held_out_sha256:held_hash.clone() },"metrics":metrics,"probabilities":probabilities,"on":chosen.is_some() && metrics.passed(),"rules_only":usage=="wake" && (!metrics.passed()),"regression":metrics.false_negatives>0}));
        }
        let mut best = 0;
        if chosen.is_none() {
            for i in 1..variants.len() {
                let m = &variants[i]["metrics"];
                let b = &variants[best]["metrics"];
                if m["false_negatives"] == 0
                    && m["false_positives"] == 0
                    && (m["suppressed_negatives"].as_u64() > b["suppressed_negatives"].as_u64()
                        || (m["suppressed_negatives"] == b["suppressed_negatives"]
                            && m["grey"].as_u64() < b["grey"].as_u64()))
                {
                    best = i;
                }
            }
        }
        result["uses"][usage] = variants[best].clone();
        result["uses"][usage]["variants"] = json!(variants);
    }
    result["measured_total_usd"] = json!(total);
    write_new(output, &result)?;
    println!(
        "{}; measured cumulative spend USD {total:.9}",
        output.display()
    );
    Ok(())
}
