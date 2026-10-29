// file-based task queue: scheduled jobs (Hermes cron, launchd) hand Taskhomie work by
// dropping a JSON file in ~/.taskhomie/queue/inbox; the result lands in outbox/<id>.json.
// No network listener, so only processes that can write to the user's home can submit work.
//
//   inbox/<id>.json   {"task": "...", "mode": "computer"|"browser", "model": "claude-sonnet-5-5"}
//   running/ done/    where the task file moves while running / after
//   outbox/<id>.json  {"id", "status": "done"|"error", "result", "error", "started_at", "finished_at"}
//   drafts/<id>.md    where unattended runs park anything that needs approval
use crate::agent::{Agent, AgentMode};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::AppHandle;
use tokio::sync::Mutex;

const POLL_SECS: u64 = 3;
const TASK_TIMEOUT_SECS: u64 = 900;
const DEFAULT_MODEL: &str = "claude-sonnet-5-5";

fn root() -> Option<PathBuf> {
    let root = dirs::home_dir()?.join(".taskhomie").join("queue");
    for sub in ["inbox", "running", "done", "outbox", "drafts"] {
        std::fs::create_dir_all(root.join(sub)).ok()?;
    }
    Some(root)
}

// ids end up in file names, so keep them to a safe alphabet
fn safe_id(raw: &str) -> String {
    let id: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if id.is_empty() { "task".to_string() } else { id }
}

fn unattended_prompt(task: &str, id: &str, drafts: &PathBuf) -> String {
    format!(
        "UNATTENDED RUN: no human is watching and nobody can answer questions.\n\
         - Do not ask questions. State your assumptions and deliver what stands; name any gap, never invent the missing piece.\n\
         - Reversible, internal work only: read, search, draft, summarise, file notes with the memory tool.\n\
         - Do NOT send, post, delete, pay, accept or sign anything. If the task needs that, prepare it fully and save the exact content to {}/{}.md, then say what approval is needed.\n\
         - Prefer bash and web tools. Use screen control only when nothing else works.\n\
         - Finish with a short plain-text report: what you did, what you did not do, what needs a decision.\n\n\
         TASK:\n{}",
        drafts.display(),
        id,
        task
    )
}

fn write_outbox(root: &PathBuf, id: &str, body: serde_json::Value) {
    let path = root.join("outbox").join(format!("{id}.json"));
    if let Ok(text) = serde_json::to_string_pretty(&body) {
        let _ = std::fs::write(path, text);
    }
}

fn now() -> String {
    // seconds since the epoch is enough for ordering and avoids a date dependency
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().to_string())
        .unwrap_or_default()
}

pub fn start(app: AppHandle, agent: Arc<Mutex<Agent>>, running: Arc<AtomicBool>) {
    tauri::async_runtime::spawn(async move {
        let Some(root) = root() else {
            println!("[queue] could not create ~/.taskhomie/queue; task queue disabled");
            return;
        };
        println!("[queue] watching {}", root.join("inbox").display());

        loop {
            tokio::time::sleep(Duration::from_secs(POLL_SECS)).await;

            // never fight an interactive run for the screen
            if running.load(Ordering::SeqCst) {
                continue;
            }

            let mut files: Vec<PathBuf> = match std::fs::read_dir(root.join("inbox")) {
                Ok(rd) => rd
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "json"))
                    .collect(),
                Err(_) => continue,
            };
            files.sort();
            let Some(file) = files.into_iter().next() else { continue };

            let stem = file.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
            let running_path = root.join("running").join(file.file_name().unwrap_or_default());
            if std::fs::rename(&file, &running_path).is_err() {
                continue;
            }

            let spec: serde_json::Value = match std::fs::read_to_string(&running_path)
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
            {
                Some(v) => v,
                None => {
                    write_outbox(&root, &safe_id(&stem), serde_json::json!({
                        "id": safe_id(&stem), "status": "error", "error": "task file is not valid JSON",
                        "finished_at": now()
                    }));
                    let _ = std::fs::rename(&running_path, root.join("done").join(file.file_name().unwrap_or_default()));
                    continue;
                }
            };

            let id = safe_id(spec.get("id").and_then(|v| v.as_str()).unwrap_or(&stem));
            let task = spec.get("task").and_then(|v| v.as_str()).unwrap_or("").to_string();
            let model = spec.get("model").and_then(|v| v.as_str()).unwrap_or(DEFAULT_MODEL).to_string();
            let mode = match spec.get("mode").and_then(|v| v.as_str()) {
                Some("browser") => AgentMode::Browser,
                _ => AgentMode::Computer,
            };
            let started = now();
            println!("[queue] running {id} (model: {model}, mode: {mode:?})");

            let outcome: Result<String, String> = if task.trim().is_empty() {
                Err("task is empty".to_string())
            } else {
                let guard = agent.lock().await;
                if !guard.has_api_key() {
                    Err("no API key configured".to_string())
                } else {
                    let prompt = unattended_prompt(&task, &id, &root.join("drafts"));
                    let run = guard.run(prompt, model, mode, false, vec![], None, None, app.clone());
                    match tokio::time::timeout(Duration::from_secs(TASK_TIMEOUT_SECS), run).await {
                        Ok(Ok(_)) => Ok(guard.last_text()),
                        Ok(Err(e)) => Err(format!("{e:?}")),
                        Err(_) => {
                            running.store(false, Ordering::SeqCst);
                            Err(format!("timed out after {TASK_TIMEOUT_SECS}s"))
                        }
                    }
                }
            };

            let body = match outcome {
                Ok(result) => serde_json::json!({
                    "id": id, "status": "done", "result": result,
                    "started_at": started, "finished_at": now()
                }),
                Err(error) => serde_json::json!({
                    "id": id, "status": "error", "error": error,
                    "started_at": started, "finished_at": now()
                }),
            };
            write_outbox(&root, &id, body);
            let _ = std::fs::rename(&running_path, root.join("done").join(file.file_name().unwrap_or_default()));
            println!("[queue] finished {id}");
        }
    });
}
