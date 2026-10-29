// persistent notes for the chief-of-staff persona, stored as markdown files
// in ~/.taskhomie/memory so they survive across sessions and stay out of the repo
use std::fs;
use std::path::PathBuf;

const MAX_CONTEXT_BYTES: usize = 12_000;

fn memory_dir() -> Option<PathBuf> {
    let dir = dirs::home_dir()?.join(".taskhomie").join("memory");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

// names are restricted to [a-z0-9_-] so a note can never escape the memory dir
fn note_path(name: &str) -> Result<PathBuf, String> {
    let stem = name.trim().trim_end_matches(".md").to_lowercase();
    if stem.is_empty() || !stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("Invalid name: use letters, digits, '-' and '_' only".to_string());
    }
    let dir = memory_dir().ok_or("Memory directory unavailable")?;
    Ok(dir.join(format!("{stem}.md")))
}

pub fn tool_def() -> serde_json::Value {
    serde_json::json!({
        "name": "memory",
        "description": "Persistent notes that survive across sessions. Actions: list, read, write (replace a note), append. Note names are short slugs like 'profile', 'preferences', 'decisions', 'follow-ups'. File decisions and durable facts the turn they happen. Never store credentials.",
        "input_schema": {
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "read", "write", "append"]},
                "name": {"type": "string", "description": "Note name (required except for list)"},
                "content": {"type": "string", "description": "Markdown text for write/append"}
            },
            "required": ["action"]
        }
    })
}

pub fn handle(input: &serde_json::Value) -> String {
    let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
    let name = input.get("name").and_then(|v| v.as_str()).unwrap_or("");
    let content = input.get("content").and_then(|v| v.as_str()).unwrap_or("");

    let result: Result<String, String> = (|| match action {
        "list" => {
            let dir = memory_dir().ok_or("Memory directory unavailable")?;
            let mut names: Vec<String> = fs::read_dir(dir)
                .map_err(|e| e.to_string())?
                .filter_map(|e| e.ok())
                .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().to_string()))
                .collect();
            names.sort();
            Ok(if names.is_empty() { "No notes yet".to_string() } else { names.join("\n") })
        }
        "read" => fs::read_to_string(note_path(name)?).map_err(|_| format!("No note named '{name}'")),
        "write" | "append" => {
            if content.is_empty() {
                return Err("content is required".to_string());
            }
            let path = note_path(name)?;
            let text = if action == "append" {
                let mut existing = fs::read_to_string(&path).unwrap_or_default();
                if !existing.is_empty() && !existing.ends_with('\n') {
                    existing.push('\n');
                }
                existing + content + "\n"
            } else {
                content.to_string()
            };
            fs::write(&path, text).map_err(|e| e.to_string())?;
            Ok(format!("Saved '{name}'"))
        }
        _ => Err("Unknown action: use list, read, write or append".to_string()),
    })();

    result.unwrap_or_else(|e| format!("Error: {e}"))
}

// injected into the system prompt each request: profile + preferences + note index
pub fn context_block() -> Option<String> {
    let dir = memory_dir()?;
    let mut out = String::from("## Memory (~/.taskhomie/memory)\nRead before asking the principal anything they have already told you. Use the memory tool for other notes.\n");
    let mut names: Vec<String> = fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| e.path().file_stem().map(|s| s.to_string_lossy().to_string()))
        .collect();
    names.sort();
    if names.is_empty() {
        out.push_str("\nNo notes yet.");
        return Some(out);
    }
    out.push_str(&format!("\nNotes: {}\n", names.join(", ")));
    for core in ["profile", "preferences"] {
        if let Ok(text) = fs::read_to_string(dir.join(format!("{core}.md"))) {
            let end = text.char_indices().map(|(i, _)| i).take_while(|&i| i < MAX_CONTEXT_BYTES).last().unwrap_or(0);
            let slice = if text.len() > MAX_CONTEXT_BYTES { &text[..end] } else { &text[..] };
            out.push_str(&format!("\n### {core}\n{slice}\n"));
        }
    }
    Some(out)
}
