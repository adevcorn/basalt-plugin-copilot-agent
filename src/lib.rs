//! copilot-agent — Basalt plugin providing the GitHub Copilot agent launcher.
//!
//! Provides: agent-launcher:copilot
//! Parses:   copilot --output-format json   (NDJSON, one JSON object per line)
//!
//! Event format emitted by `copilot`:
//!   {"type":"tool.execution_start",  "timestamp":"...", "data":{"toolCallId":"...","toolName":"...","arguments":{...}}}
//!   {"type":"tool.execution_complete","timestamp":"...", "data":{"toolCallId":"...","success":true,"result":{"content":"..."}}}
//!   {"type":"assistant.message",      "timestamp":"...", "data":{"content":"..."}}
//!   {"type":"result", "exitCode":0, "sessionId":"..."}

use basalt_plugin_sdk::prelude::*;

basalt_plugin_meta! {
    name:              "copilot-agent",
    version:           env!("CARGO_PKG_VERSION"),
    hook_flags:        CAP_AGENT_LAUNCHER,
    provides:          "agent-launcher:copilot",
    requires:          "",
    file_globs:        "",
    activates_on:      "",
    activation_events: "",
}

// ---------------------------------------------------------------------------
// agent_metadata
// ---------------------------------------------------------------------------

#[basalt_plugin]
fn agent_metadata() -> AgentMetadata {
    AgentMetadata {
        name: "GitHub Copilot".into(),
        executable: "/opt/homebrew/bin/copilot".into(),
        args: vec![
            "--allow-all-paths".into(),
            "--output-format".into(),
            "json".into(),
            "--allow-all-tools".into(),
            "--available-tools=basalt(*)".into(),
        ],
        // New session with prompt
        resume_new_args: vec![
            "--allow-all-paths".into(),
            "--output-format".into(),
            "json".into(),
            "--allow-all-tools".into(),
            "--available-tools=basalt(*)".into(),
            "-p".into(),
            "{prompt}".into(),
        ],
        // Resume prior session using --continue
        resume_cont_args: vec![
            "--resume={session_id}".into(),
            "--allow-all-paths".into(),
            "--output-format".into(),
            "json".into(),
            "--allow-all-tools".into(),
            "--available-tools=basalt(*)".into(),
            "-p".into(),
            "{prompt}".into(),
        ],
        execution_tier: AgentExecutionTier::MountedWorkspace,
        workspace_capabilities: vec![
            "speculative-edits".into(),
            "approval-required".into(),
            "utf8-text".into(),
            "create".into(),
            "delete".into(),
            "rename".into(),
            "shadow-projection".into(),
            "mcp".into(),
        ],
    }
}

// ---------------------------------------------------------------------------
// agent_environment
// ---------------------------------------------------------------------------

#[basalt_plugin]
fn agent_environment() -> Vec<(&'static str, &'static str)> {
    vec![]
}

// ---------------------------------------------------------------------------
// Parser state — set of open toolCallIds
// State blob: `[count: u16 LE]` then per entry `[key_len: u16 LE][key bytes]`
// ---------------------------------------------------------------------------

struct ParseState {
    open_calls: Vec<String>,
    open_message: bool,
}

impl ParseState {
    fn decode(state: &[u8]) -> Self {
        if state.len() < 2 {
            return Self { open_calls: vec![], open_message: false };
        }
        let count = u16::from_le_bytes([state[0], state[1]]) as usize;
        let mut items = Vec::with_capacity(count);
        let mut cur = 2usize;
        for _ in 0..count {
            if cur + 2 > state.len() {
                break;
            }
            let klen = u16::from_le_bytes([state[cur], state[cur + 1]]) as usize;
            cur += 2;
            if cur + klen > state.len() {
                break;
            }
            if let Ok(s) = std::str::from_utf8(&state[cur..cur + klen]) {
                items.push(s.to_string());
            }
            cur += klen;
        }
        let open_message = state.get(cur).copied().unwrap_or(0) != 0;
        Self { open_calls: items, open_message }
    }

    fn encode(&self) -> Vec<u8> {
        let count = self.open_calls.len().min(0xFFFF) as u16;
        let mut out = Vec::new();
        out.extend_from_slice(&count.to_le_bytes());
        for k in &self.open_calls[..count as usize] {
            let bytes = k.as_bytes();
            let klen = bytes.len().min(0xFFFF) as u16;
            out.extend_from_slice(&klen.to_le_bytes());
            out.extend_from_slice(&bytes[..klen as usize]);
        }
        out.push(self.open_message as u8);
        out
    }

    fn contains(&self, id: &str) -> bool {
        self.open_calls.iter().any(|s| s == id)
    }

    fn insert(&mut self, id: String) {
        if !self.contains(&id) {
            self.open_calls.push(id);
        }
    }

    fn remove(&mut self, id: &str) -> bool {
        if let Some(pos) = self.open_calls.iter().position(|s| s == id) {
            self.open_calls.remove(pos);
            true
        } else {
            false
        }
    }
}

// ---------------------------------------------------------------------------
// agent_parse_line
// ---------------------------------------------------------------------------

// Tool names to silently skip (internal bookkeeping, not visible actions).
static INTERNAL_TOOLS: &[&str] = &["report_intent"];

#[basalt_plugin]
fn agent_parse_line(line: &[u8], state: &[u8]) -> (Vec<u8>, Vec<AgentEvent>) {
    let Ok(line_str) = std::str::from_utf8(line) else {
        return (state.to_vec(), vec![]);
    };
    let line_str = line_str.trim();
    if line_str.is_empty() {
        return (state.to_vec(), vec![]);
    }
    let mut ps = ParseState::decode(state);
    let events = parse_copilot_line(line_str, &mut ps);
    (ps.encode(), events)
}

fn parse_copilot_line(line: &str, ps: &mut ParseState) -> Vec<AgentEvent> {
    let type_val = match json_str(line, "type") {
        Some(t) => t,
        None => return vec![],
    };

    match type_val.as_str() {
        "tool.execution_start" => {
            let data_raw = match json_object_raw(line, "data") {
                Some(r) => r,
                None => return vec![],
            };
            let tool_call_id = json_str(&data_raw, "toolCallId").unwrap_or_default();
            let tool_name = json_str(&data_raw, "toolName").unwrap_or_default();
            if tool_call_id.is_empty() || tool_name.is_empty() {
                return vec![];
            }
            if INTERNAL_TOOLS.contains(&tool_name.as_str()) {
                return vec![];
            }
            // arguments may be a JSON object or a raw string
            let (tool, category, file_paths, raw_cmd) =
                if let Some(args_raw) = json_object_raw(&data_raw, "arguments") {
                    classify_tool(&tool_name, &args_raw)
                } else {
                    let args_str = json_str(&data_raw, "arguments").unwrap_or_default();
                    classify_tool(&tool_name, &args_str)
                };
            ps.insert(tool_call_id.clone());
            vec![AgentEvent::NewEntry {
                vendor_id: tool_call_id,
                tool,
                category,
                raw_cmd,
                file_paths,
            }]
        }

        "tool.execution_complete" => {
            let data_raw = match json_object_raw(line, "data") {
                Some(r) => r,
                None => return vec![],
            };
            let tool_call_id = json_str(&data_raw, "toolCallId").unwrap_or_default();
            if tool_call_id.is_empty() {
                return vec![];
            }
            if !ps.remove(&tool_call_id) {
                return vec![];
            }
            let success = json_bool(&data_raw, "success").unwrap_or(true);
            let exit_code: i32 = if success { 0 } else { 1 };
            // Output lives in data.result.content
            let content = if let Some(result_raw) = json_object_raw(&data_raw, "result") {
                json_str(&result_raw, "content").unwrap_or_default()
            } else {
                String::new()
            };
            let lines: Vec<String> = content
                .lines()
                .filter(|l| !l.is_empty() && *l != "<exited with exit code 0>")
                .map(|l| l.to_string())
                .collect();
            vec![AgentEvent::CloseEntry {
                vendor_id: tool_call_id,
                exit_code,
                output_lines: lines,
            }]
        }

        "assistant.message" => parse_assistant_message(line, ps),

        "result" => {
            // May carry sessionId — emit that first, then SessionEnded.
            let mut events = Vec::new();
            if let Some(sid) = json_str(line, "sessionId") {
                events.push(AgentEvent::SessionIDAvailable(sid));
            }
            let exit_code = json_int(line, "exitCode").unwrap_or(0);
            ps.open_message = false;
            events.push(AgentEvent::SessionEnded {
                success: exit_code == 0,
            });
            events
        }

        _ => vec![],
    }
}

fn parse_assistant_message(line: &str, ps: &mut ParseState) -> Vec<AgentEvent> {
    let text = json_object_raw(line, "data")
        .and_then(|data_raw| json_str(&data_raw, "content"))
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return vec![];
    }

    if !ps.open_message {
        ps.open_message = true;
        return vec![AgentEvent::NewEntry {
            vendor_id: "copilot-message".into(),
            tool: text,
            category: "message".into(),
            raw_cmd: String::new(),
            file_paths: vec![],
        }];
    }

    vec![AgentEvent::AppendToEntry {
        vendor_id: "copilot-message".into(),
        text,
    }]
}

// ---------------------------------------------------------------------------
// Tool classification
// Returns (tool_label, category, file_paths, raw_cmd)
// ---------------------------------------------------------------------------

fn classify_tool(tool_name: &str, args_raw: &str) -> (String, String, Vec<String>, String) {
    match tool_name {
        "bash" | "run_command" | "execute_command" => {
            let cmd = json_str(args_raw, "command")
                .unwrap_or_else(|| args_raw.chars().take(80).collect());
            let (tool, category) = shell_classify(&cmd);
            let paths = shell_file_paths(&cmd);
            let raw = cmd;
            (tool, category, paths, raw)
        }
        "read_file" | "get_file_content" | "view" => {
            let path = json_str(args_raw, "path")
                .or_else(|| json_str(args_raw, "file_path"))
                .unwrap_or_default();
            let name_part = path.rsplit('/').next().unwrap_or(&path).to_string();
            (
                format!("Read {}", name_part),
                "read".into(),
                if path.is_empty() {
                    vec![]
                } else {
                    vec![path.clone()]
                },
                path,
            )
        }
        "write_file" | "create_file" | "update_file" | "apply_patch" => {
            let path = json_str(args_raw, "path")
                .or_else(|| json_str(args_raw, "file_path"))
                .unwrap_or_default();
            let name_part = path.rsplit('/').next().unwrap_or(&path).to_string();
            let category = match tool_name {
                "create_file" => "create",
                _ => "write",
            };
            (
                format!("Write {}", name_part),
                category.into(),
                if path.is_empty() {
                    vec![]
                } else {
                    vec![path.clone()]
                },
                if path.is_empty() {
                    args_raw.chars().take(120).collect()
                } else {
                    path
                },
            )
        }
        _ => {
            let raw = json_str(args_raw, "command")
                .or_else(|| json_str(args_raw, "path").map(|p| format!("{} {}", tool_name, p)))
                .unwrap_or_else(|| {
                    if args_raw.is_empty() {
                        tool_name.to_string()
                    } else {
                        format!("{} {}", tool_name, &args_raw[..args_raw.len().min(120)])
                    }
                });
            let display = tool_name
                .split('_')
                .map(|w| {
                    let mut c = w.chars();
                    match c.next() {
                        None => String::new(),
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                    }
                })
                .collect::<Vec<_>>()
                .join(" ");
            (display, "run".into(), vec![], raw)
        }
    }
}

fn shell_mutation_classify(first: &str) -> (String, String) {
    match first {
        "mv" => (format!("Move {}", first), "move".into()),
        "mkdir" | "touch" => (format!("Create {}", first), "create".into()),
        "rm" => (format!("Delete {}", first), "delete".into()),
        _ => (format!("Write {}", first), "write".into()),
    }
}

fn shell_classify(cmd: &str) -> (String, String) {
    let inner = extract_inner(cmd);
    let first = inner.split_whitespace().next().unwrap_or("").to_lowercase();
    match first.as_str() {
        "ls" | "find" | "cat" | "head" | "tail" | "grep" | "rg" | "fd" | "stat" => {
            (format!("List {}", first), "list".into())
        }
        "cp" | "mv" | "mkdir" | "touch" | "rm" | "tee" | "sed" | "awk" => {
            shell_mutation_classify(&first)
        }
        "git" => {
            let sub = inner.split_whitespace().nth(1).unwrap_or("").to_string();
            (format!("Git {}", sub).trim().to_string(), "git".into())
        }
        "cargo" | "swift" | "xcodebuild" | "make" | "npm" | "yarn" | "pnpm" => {
            (format!("Build {}", first), "build".into())
        }
        "curl" | "wget" => (format!("Run {}", first), "run".into()),
        _ => {
            let display = if first.is_empty() {
                "Shell".to_string()
            } else {
                let mut c = first.chars();
                match c.next() {
                    None => String::new(),
                    Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                }
            };
            (display, "run".into())
        }
    }
}

fn extract_inner(cmd: &str) -> String {
    if let Some(start) = cmd.find('\'') {
        if let Some(end) = cmd[start + 1..].rfind('\'') {
            return cmd[start + 1..start + 1 + end].to_string();
        }
    }
    cmd.to_string()
}

fn shell_file_paths(cmd: &str) -> Vec<String> {
    let inner = extract_inner(cmd);
    let shell_kw = [
        "if", "then", "else", "fi", "do", "done", "for", "while", "in", "echo", "export", "cd",
        "||", "&&", "|", ";", ">", ">>", "<", "2>",
    ];
    inner
        .split_whitespace()
        .filter(|tok| {
            if tok.starts_with('-') {
                return false;
            }
            if shell_kw.contains(tok) {
                return false;
            }
            if tok.contains('/') {
                return true;
            }
            let ext = tok.rsplit('.').next().unwrap_or("");
            [
                "swift", "rs", "ts", "js", "py", "go", "c", "cpp", "h", "toml", "json", "yaml",
                "yml", "md", "txt",
            ]
            .contains(&ext)
        })
        .map(|s| s.to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// Minimal JSON helpers (self-contained; duplicated from other agent plugins)
// ---------------------------------------------------------------------------

fn json_str(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(needle.as_str())?;
    let after_key = &json[pos + needle.len()..];
    let colon = after_key.find(':')? + 1;
    let rest = after_key[colon..].trim_start();
    if rest.starts_with('"') {
        parse_json_string(&rest[1..])
    } else {
        None
    }
}

fn json_int(json: &str, key: &str) -> Option<i64> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(needle.as_str())?;
    let after = &json[pos + needle.len()..];
    let colon = after.find(':')? + 1;
    let rest = after[colon..].trim_start();
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_bool(json: &str, key: &str) -> Option<bool> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(needle.as_str())?;
    let after = &json[pos + needle.len()..];
    let colon = after.find(':')? + 1;
    let rest = after[colon..].trim_start();
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn json_object_raw(json: &str, key: &str) -> Option<String> {
    let needle = format!("\"{}\"", key);
    let pos = json.find(needle.as_str())?;
    let after_key = &json[pos + needle.len()..];
    let colon = after_key.find(':')? + 1;
    let rest = after_key[colon..].trim_start();
    if !rest.starts_with('{') {
        return None;
    }
    let mut depth = 0usize;
    let mut end = 0usize;
    for (i, c) in rest.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    if end == 0 {
        None
    } else {
        Some(rest[..end].to_string())
    }
}

fn parse_json_string(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}
