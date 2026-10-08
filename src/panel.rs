//! `byoclaude panel`: one question, or one change with `--attempt`, put to several models as
//! headless Claude Code agents; a judge compares their anonymized reports.
//!
//! ```text
//! panels/<id>/  panel.json             the record: question, members, judge, verdict, applied
//!               <label>.out.json       a member's raw `claude -p` result (and .err.log)
//!               <label>.patch          an attempt's diff against its starting state
//!               <label>.test.log       the test command's output in that attempt's worktree
//!               judge.prompt.md        what the judge saw
//! ```
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

use crate::providers::{canonical, is_claude, split};

/// Set in every panelist and judge; `byoclaude panel` refuses to run under it.
pub const GUARD: &str = "BYOCLAUDE_PANEL";

/// Variables that would make a child `claude` behave as part of the calling session.
const NESTED_SESSION: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_PLUGIN_DIRS",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_CODE_BRIDGE_SESSION_ID",
    "CLAUDE_PID",
];

const GIT_ENV: &[&str] = &["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"];

const READ_ONLY_BASH: &[&str] = &[
    "git status",
    "git diff",
    "git log",
    "git show",
    "ls",
    "rg",
    "grep",
    "find",
    "cat",
    "head",
    "tail",
    "wc",
];

/// Flags that make an allowed read-only command write files or run programs.
const DENIED_BASH: &[&str] = &[
    "git diff *--output*",
    "git log *--output*",
    "git show *--output*",
    "rg *--pre *",
    "rg *--pre=*",
    "find *-exec*",
    "find *-ok*",
    "find *-delete*",
    "find *-fprint*",
    "find *-fls*",
];

/// `git diff` options that keep patches readable by `git apply` whatever the user's git config.
const DIFF: &[&str] = &[
    "diff",
    "--binary",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
    "--no-relative",
    "--submodule=short",
    "--src-prefix=a/",
    "--dst-prefix=b/",
];

const LOCAL_PROVIDERS: &[&str] = &["ollama", "lmstudio"];
const TEST_TAIL_LINES: usize = 60;
const JUDGE_PATCH_BYTES: usize = 60_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Opinion,
    Attempt,
}

/// `byoclaude panel` arguments; unset values fall back to the `panel` settings.
pub struct Options {
    pub question: String,
    pub attempt: bool,
    pub models: Vec<String>,
    pub judge: Option<String>,
    pub size: Option<u32>,
    pub test: Option<String>,
    pub timeout: Option<u64>,
    pub json: bool,
}

/// A roster model as selection sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub id: String,
    pub provider: String,
    pub cost_output: f64,
    /// Roster status: `ready`, `capped`, `signed-out`, `no-key`.
    pub status: String,
}

/// Models picked for a panel, and those passed over with the reason.
#[derive(Debug, Default, PartialEq)]
pub struct Selection {
    pub members: Vec<String>,
    pub skipped: Vec<Skipped>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Skipped {
    pub model: String,
    pub status: String,
}

/// Candidates from `roster::roster_json`, which applies provider readiness and caps.
pub fn candidates(view: &Value) -> Vec<Candidate> {
    view["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(Candidate {
                id: m["id"].as_str()?.to_owned(),
                provider: m["provider"].as_str()?.to_owned(),
                cost_output: m["price_per_million"]["output"].as_f64().unwrap_or(0.0),
                status: m["status"].as_str().unwrap_or("").to_owned(),
            })
        })
        .collect()
}

fn status(candidate: &Candidate, relay: bool) -> &str {
    if is_claude(&candidate.id) && !relay {
        "relay-off"
    } else {
        &candidate.status
    }
}

/// A provider's models, best first: the configured Claude model for `anthropic`, then by
/// output price.
fn ranked<'a>(
    config: &crate::config::Config,
    candidates: &'a [Candidate],
    provider: &str,
) -> Vec<&'a Candidate> {
    let mut models: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.provider == provider)
        .collect();
    models.sort_by(|a, b| {
        let preferred = |c: &Candidate| c.id == config.behaves_as;
        preferred(b)
            .cmp(&preferred(a))
            .then(b.cost_output.total_cmp(&a.cost_output))
    });
    models
}

fn providers_in_order(candidates: &[Candidate]) -> Vec<&str> {
    let mut seen = Vec::new();
    for c in candidates {
        if !seen.contains(&c.provider.as_str()) {
            seen.push(c.provider.as_str());
        }
    }
    seen
}

/// Up to `size` ready models from distinct providers, each provider's best model, the
/// highest-priced providers first and local ones last.
pub fn select(
    config: &crate::config::Config,
    candidates: &[Candidate],
    relay: bool,
    size: usize,
) -> Selection {
    let mut picks: Vec<&Candidate> = Vec::new();
    let mut skipped = Vec::new();
    for provider in providers_in_order(candidates) {
        let models = ranked(config, candidates, provider);
        match models.iter().position(|c| status(c, relay) == "ready") {
            Some(index) => {
                skipped.extend(models[..index].iter().map(|c| Skipped {
                    model: c.id.clone(),
                    status: status(c, relay).to_owned(),
                }));
                picks.push(models[index]);
            }
            None => skipped.push(Skipped {
                model: models[0].id.clone(),
                status: status(models[0], relay).to_owned(),
            }),
        }
    }
    picks.sort_by(|a, b| {
        let local = |c: &Candidate| LOCAL_PROVIDERS.contains(&c.provider.as_str());
        local(a)
            .cmp(&local(b))
            .then(b.cost_output.total_cmp(&a.cost_output))
    });
    Selection {
        members: picks.iter().take(size).map(|c| c.id.clone()).collect(),
        skipped,
    }
}

/// The judge: the strongest ready model from a provider not on the panel, Claude first when
/// `anthropic` is off it; otherwise the strongest panel model, with a warning.
pub fn pick_judge(
    config: &crate::config::Config,
    candidates: &[Candidate],
    relay: bool,
    members: &[String],
) -> Option<(String, Option<String>)> {
    let on_panel: HashSet<&str> = members.iter().map(|m| split(m).0).collect();
    let ready = |c: &&Candidate| status(c, relay) == "ready";
    let best = |provider: &str| ranked(config, candidates, provider).into_iter().find(ready);
    if !on_panel.contains(crate::providers::CLAUDE_PROVIDER)
        && let Some(claude) = best(crate::providers::CLAUDE_PROVIDER)
    {
        return Some((claude.id.clone(), None));
    }
    let local = |c: &Candidate| LOCAL_PROVIDERS.contains(&c.provider.as_str());
    let outside = providers_in_order(candidates)
        .into_iter()
        .filter(|p| !on_panel.contains(p))
        .filter_map(best)
        .min_by(|a, b| {
            local(a)
                .cmp(&local(b))
                .then(b.cost_output.total_cmp(&a.cost_output))
        });
    if let Some(judge) = outside {
        return Some((judge.id.clone(), None));
    }
    let price = |id: &str| {
        candidates
            .iter()
            .find(|c| c.id == id)
            .map_or(0.0, |c| c.cost_output)
    };
    let judge = members
        .iter()
        .min_by(|a, b| price(b).total_cmp(&price(a)))?;
    Some((
        judge.clone(),
        Some(format!(
            "no ready model outside the panel's providers; {judge} judges its own panel"
        )),
    ))
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Member {
    pub label: String,
    pub model: String,
    /// `ok`, `failed` or `timeout`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_ms: u64,
    /// Uncached input; cached input is counted apart.
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    pub output_tokens: u64,
    /// Estimated USD from roster prices, `"plan"` for plan routes, or null when unknown.
    pub cost_usd_estimate: Value,
    pub report: Value,
    /// Patch file in the ledger directory; attempts only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_stats: Option<PatchStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<TestRun>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PatchStats {
    pub files: u64,
    pub added: u64,
    pub removed: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TestRun {
    pub command: String,
    /// None when the command was killed or timed out.
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub output_tail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Applied {
    pub label: String,
    pub model: String,
    pub at: String,
    /// `direct`, `3way`, or `3way-conflicts` when the merge left conflict markers.
    pub method: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Panel {
    pub id: String,
    pub created: String,
    pub cwd: String,
    /// Repository root; attempts only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    pub question: String,
    pub mode: Mode,
    pub members: Vec<Member>,
    pub skipped: Vec<Skipped>,
    pub judge: Option<Member>,
    /// The judge's verdict with panelist labels mapped to model IDs.
    pub verdict: Value,
    pub warnings: Vec<String>,
    pub applied: Option<Applied>,
}

fn panels_dir() -> Result<PathBuf> {
    Ok(crate::store::home()?.join("panels"))
}

fn civil(secs: u64) -> (i64, u64, u64, u64, u64, u64) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u64;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day, rem / 3600, rem / 60 % 60, rem % 60)
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn rfc3339(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// Sortable panel ID: UTC time plus a random suffix.
fn new_id(secs: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(secs);
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}-{}", &suffix[..4])
}

fn label(index: usize) -> String {
    char::from(b'A' + (index % 26) as u8).to_string()
}

fn save(dir: &Path, panel: &Panel) -> Result<()> {
    crate::store::write_private(
        &dir.join("panel.json"),
        format!("{}\n", serde_json::to_string_pretty(panel)?).as_bytes(),
    )
}

fn load_from(dir: &Path) -> Result<Panel> {
    let bytes = crate::store::read_private(&dir.join("panel.json"))?
        .with_context(|| format!("no panel record in {}", dir.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}/panel.json", dir.display()))
}

/// A panel's ledger directory by ID or unique ID prefix.
fn find(id: &str) -> Result<PathBuf> {
    let root = panels_dir()?;
    if !id.is_empty()
        && !id.contains(['/', '\\'])
        && id != "."
        && id != ".."
        && root.join(id).join("panel.json").is_file()
    {
        return Ok(root.join(id));
    }
    let matches: Vec<String> = ids()?
        .into_iter()
        .filter(|candidate| !id.is_empty() && candidate.starts_with(id))
        .collect();
    match matches.as_slice() {
        [one] => Ok(root.join(one)),
        [] => bail!("no panel {id:?}; `byoclaude panel list` shows recorded panels"),
        _ => bail!("{id:?} matches several panels; use more of the ID"),
    }
}

/// Recorded panel IDs, newest first.
fn ids() -> Result<Vec<String>> {
    let root = panels_dir()?;
    let mut ids: Vec<String> = match std::fs::read_dir(&root) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join("panel.json").is_file())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", root.display())),
    };
    ids.sort_unstable_by(|a, b| b.cmp(a));
    Ok(ids)
}

fn opinion_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "answer": {"type": "string", "description": "Your answer to the question."},
            "findings": {"type": "array", "items": {
                "type": "object",
                "properties": {
                    "claim": {"type": "string"},
                    "evidence": {"type": "string", "description": "file:line references or command output that back the claim"},
                    "confidence": {"type": "string", "enum": ["low", "medium", "high"]}
                },
                "required": ["claim", "evidence", "confidence"]
            }},
            "uncertainties": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["answer", "findings", "uncertainties"]
    })
}

fn attempt_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string", "description": "What you changed and why."},
            "changes": {"type": "array", "items": {"type": "string"}, "description": "One entry per changed file or behavior."},
            "verification": {"type": "string", "description": "What you ran to check the change, and the result."},
            "concerns": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["summary", "changes", "verification", "concerns"]
    })
}

fn verdict_schema(mode: Mode, labels: &[String]) -> Value {
    let label = json!({"type": "string", "enum": labels});
    let confidence = json!({"type": "string", "enum": ["low", "medium", "high"]});
    let mut schema = json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "agreement": {"type": "array", "items": {
                "type": "object",
                "properties": {"point": {"type": "string"}, "panelists": {"type": "array", "items": label}},
                "required": ["point", "panelists"]
            }},
            "conflicts": {"type": "array", "items": {
                "type": "object",
                "properties": {
                    "topic": {"type": "string"},
                    "positions": {"type": "array", "items": {
                        "type": "object",
                        "properties": {"panelist": label, "position": {"type": "string"}},
                        "required": ["panelist", "position"]
                    }},
                    "assessment": {"type": "string", "description": "Who is right and why, after checking the evidence."}
                },
                "required": ["topic", "positions", "assessment"]
            }},
            "unique": {"type": "array", "items": {
                "type": "object",
                "properties": {"panelist": label, "finding": {"type": "string"}, "assessment": {"type": "string"}},
                "required": ["panelist", "finding", "assessment"]
            }},
            "blind_spots": {"type": "array", "items": {"type": "string"}},
            "recommendation": {"type": "string"},
            "confidence": confidence
        },
        "required": ["summary", "agreement", "conflicts", "unique", "blind_spots", "recommendation", "confidence"]
    });
    if mode == Mode::Attempt {
        let mut winner: Vec<&str> = labels.iter().map(String::as_str).collect();
        winner.push("none");
        let properties = schema["properties"].as_object_mut().expect("object schema");
        properties.insert(
            "ranking".into(),
            json!({"type": "array", "description": "Every attempt, best first.", "items": {
                "type": "object",
                "properties": {"panelist": label, "assessment": {"type": "string"}},
                "required": ["panelist", "assessment"]
            }}),
        );
        properties.insert(
            "winner".into(),
            json!({"type": "string", "enum": winner, "description": "The attempt to apply, or none when no attempt is acceptable."}),
        );
        properties.insert(
            "merge".into(),
            json!({"type": "string", "description": "What to take from the other attempts, if anything."}),
        );
        let required = schema["required"].as_array_mut().expect("required list");
        for key in ["ranking", "winner", "merge"] {
            required.push(json!(key));
        }
    }
    schema
}

fn panelist_prompt(mode: Mode, test: &str) -> String {
    let mut prompt = String::from(
        "You are one of several independent panelists. Each panelist gets the same task and works alone; a judge compares the reports. Back every claim with evidence you gathered: file:line references, command output, documentation. Say what you could not verify instead of guessing.\n",
    );
    match mode {
        Mode::Opinion => prompt.push_str(
            "Do not modify, create or delete any files. Investigate, then return your report: the answer, findings with their evidence and confidence, and remaining uncertainties.\n",
        ),
        Mode::Attempt => prompt.push_str(
            "Make the change in this copy of the repository only; do not touch files outside it and do not commit. byoclaude collects your diff and runs the tests itself afterwards. Return your report: a summary, the changes, how you verified them, and concerns.\n",
        ),
    }
    if !test.is_empty() {
        prompt.push_str(&format!("The project's test command is `{test}`.\n"));
    }
    prompt
}

const JUDGE_PROMPT: &str = "You are the judge of a panel. Several panelists answered the same task independently; their reports follow, labeled Panelist A, B, C. Compare them: where they agree, where they conflict, findings only one made, and what all of them missed. Where reports conflict or cite evidence that matters, check it yourself with the read-only tools. Judge the work, not the writing style. Refer to panelists only by their labels. Do not modify any files.";

/// Allow rules beyond what the permission mode grants: reads and searches inside the working
/// directories need none.
fn tools(read_only_bash: &[&str], test: &str, extra: &[&str]) -> Vec<String> {
    let mut allowed: Vec<String> = extra.iter().map(|t| t.to_string()).collect();
    allowed.extend(read_only_bash.iter().map(|c| format!("Bash({c}:*)")));
    if !test.trim().is_empty() {
        allowed.push(format!("Bash({}:*)", test.trim()));
    }
    allowed
}

/// What a headless agent may touch.
struct Access<'a> {
    /// `--tools`: the tools that exist at all.
    tools: &'a str,
    /// `--allowedTools` rules.
    allowed: Vec<String>,
    /// `dontAsk` (read-only) or `acceptEdits`.
    permission_mode: &'a str,
    /// A further working directory, such as the repository root above the cwd.
    add_dir: Option<&'a Path>,
}

/// `claude -p` arguments for one panelist or the judge, after the launcher's own.
/// `--restricted` ignores user, project and local settings and confines file tools to the
/// working directories.
fn agent_args(model: &str, access: &Access, system: &str, schema: &Value) -> Vec<String> {
    let mut args: Vec<String> = [
        "-p",
        "--model",
        model,
        "--output-format",
        "json",
        "--no-session-persistence",
        "--strict-mcp-config",
        "--restricted",
        "--permission-mode",
        access.permission_mode,
        "--tools",
        access.tools,
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(dir) = access.add_dir {
        args.extend(["--add-dir".into(), dir.display().to_string()]);
    }
    // The tool lists take several values; an option must follow each.
    args.push("--disallowedTools".into());
    args.extend(DENIED_BASH.iter().map(|c| format!("Bash({c})")));
    args.push("--allowedTools".into());
    args.extend(access.allowed.iter().cloned());
    args.extend([
        "--append-system-prompt".into(),
        system.into(),
        "--json-schema".into(),
        schema.to_string(),
    ]);
    args
}

/// Token counts from a result's `usage`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Usage {
    input: u64,
    cache_read: u64,
    cache_creation: u64,
    output: u64,
}

/// What one `claude -p` run produced.
struct Outcome {
    status: &'static str,
    error: Option<String>,
    duration_ms: u64,
    usage: Usage,
    report: Value,
}

impl Outcome {
    fn failed(error: String) -> Self {
        Outcome {
            status: "failed",
            error: Some(error),
            duration_ms: 0,
            usage: Usage::default(),
            report: Value::Null,
        }
    }
}

/// The structured report and usage from a `claude -p --output-format json` result.
fn parse_result(stdout: &str) -> (Result<Value, String>, Usage) {
    let result: Value = serde_json::from_str(stdout.trim())
        .ok()
        .or_else(|| {
            stdout
                .lines()
                .rev()
                .find_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        })
        .unwrap_or(Value::Null);
    let usage = &result["usage"];
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let usage = Usage {
        input: count("input_tokens"),
        cache_read: count("cache_read_input_tokens"),
        cache_creation: count("cache_creation_input_tokens"),
        output: count("output_tokens"),
    };
    let text = result["result"].as_str().unwrap_or("");
    let report = if result.is_null() {
        Err("claude printed no JSON result".to_owned())
    } else if result["is_error"] == true {
        Err(format!(
            "claude reported an error ({}): {}",
            result["subtype"].as_str().unwrap_or("error"),
            truncate(text, 300)
        ))
    } else if result["structured_output"].is_object() {
        Ok(result["structured_output"].clone())
    } else if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(text.trim()) {
        Ok(value)
    } else {
        Err("claude returned no structured report".to_owned())
    };
    (report, usage)
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

/// Kills a child started in its own process group, with everything it started, when dropped:
/// on completion, on timeout, and when an interrupt drops the run. The child must not be
/// `kill_on_drop`, so it is still alive to be walked.
struct Group(Option<u32>);

impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(root) = self.0.and_then(|pid| i32::try_from(pid).ok()) {
            kill_tree(root);
        }
    }
}

/// SIGKILL a child, its descendants, and every process group led by one of them. Claude Code
/// runs shell commands in process groups of their own, so killing the child's group alone
/// would leave them running.
#[cfg(unix)]
fn kill_tree(root: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    const SIGKILL: i32 = 9;
    let table: Vec<(i32, i32, i32)> = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,pgid="])
        .stderr(Stdio::null())
        .output()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .filter_map(|line| {
                    let mut fields = line.split_whitespace().map(|n| n.parse::<i32>().ok());
                    Some((fields.next()??, fields.next()??, fields.next()??))
                })
                .collect()
        })
        .unwrap_or_default();
    let me = std::process::id() as i32;
    // Walk only from a live child of this process; a reaped child's PID may be reused.
    let mut tree: Vec<i32> = table
        .iter()
        .filter(|&&(pid, ppid, _)| pid == root && ppid == me)
        .map(|&(pid, ..)| pid)
        .collect();
    let mut next = 0;
    while next < tree.len() {
        let parent = tree[next];
        let children: Vec<i32> = table
            .iter()
            .filter(|&&(pid, ppid, _)| ppid == parent && !tree.contains(&pid))
            .map(|&(pid, ..)| pid)
            .collect();
        tree.extend(children);
        next += 1;
    }
    let mut targets: Vec<i32> = vec![-root];
    targets.extend(
        table
            .iter()
            .filter(|&&(pid, _, pgid)| pid == pgid && pid != root && tree.contains(&pid))
            .map(|&(pid, ..)| -pid),
    );
    targets.extend(tree.iter().copied());
    for target in targets.into_iter().filter(|t| t.abs() > 1 && t.abs() != me) {
        // SAFETY: kill(2) takes plain integers and touches no memory.
        unsafe {
            kill(target, SIGKILL);
        }
    }
}

/// Run one headless `claude` with the prompt on stdin, killing it at the timeout.
async fn run_agent(
    command: std::process::Command,
    prompt: String,
    timeout: Duration,
    raw: &Path,
) -> Outcome {
    let started = Instant::now();
    let mut command = tokio::process::Command::from(command);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let failed = |error: String| Outcome {
        duration_ms: started.elapsed().as_millis() as u64,
        ..Outcome::failed(error)
    };
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return failed(format!("starting claude: {error}")),
    };
    let _group = Group(child.id());
    if let Some(mut stdin) = child.stdin.take() {
        tokio::spawn(async move {
            let _ = stdin.write_all(prompt.as_bytes()).await;
        });
    }
    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return failed(format!("waiting for claude: {error}")),
        Err(_) => {
            return Outcome {
                status: "timeout",
                error: Some(format!("no result within {}s", timeout.as_secs())),
                ..failed(String::new())
            };
        }
    };
    let _ = crate::store::write_private(&raw.with_extension("out.json"), &output.stdout);
    let _ = crate::store::write_private(&raw.with_extension("err.log"), &output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (report, usage) = parse_result(&stdout);
    let duration_ms = started.elapsed().as_millis() as u64;
    match report {
        Ok(report) => Outcome {
            status: "ok",
            error: None,
            duration_ms,
            usage,
            report,
        },
        Err(mut error) => {
            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let last = stderr.lines().rev().find(|l| !l.trim().is_empty());
                error = format!(
                    "{error}; claude exited with {}{}",
                    output.status,
                    last.map(|l| format!(": {}", truncate(l.trim(), 200)))
                        .unwrap_or_default()
                );
            }
            Outcome {
                usage,
                ..failed(error)
            }
        }
    }
}

/// Estimated cost from roster prices, or `"plan"` for the relay and the ChatGPT plan. The
/// roster has no cache prices, so cached input counts at the input price: an upper bound.
fn cost(
    config: &crate::config::Config,
    roster: &[crate::catalog::Entry],
    model: &str,
    usage: Usage,
) -> Value {
    use crate::providers::Auth;
    if crate::providers::find(config, split(model).0)
        .is_some_and(|p| matches!(p.auth, Auth::ClaudeCode | Auth::ChatGpt))
    {
        return json!("plan");
    }
    match roster.iter().find(|e| e.id == model) {
        Some(e) if e.cost_input > 0.0 || e.cost_output > 0.0 => {
            let input = usage.input + usage.cache_read + usage.cache_creation;
            let usd = (input as f64 * e.cost_input + usage.output as f64 * e.cost_output) / 1e6;
            json!((usd * 10_000.0).round() / 10_000.0)
        }
        _ => Value::Null,
    }
}

async fn git(dir: &Path, args: &[&str], stdin: Option<Vec<u8>>) -> Result<Vec<u8>> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for name in GIT_ENV {
        command.env_remove(name);
    }
    let mut child = command.spawn().context("running git")?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        tokio::spawn(async move {
            let _ = pipe.write_all(&bytes).await;
        });
    }
    let output = child.wait_with_output().await.context("running git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

async fn git_line(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git(dir, args, None).await?)
        .trim_end_matches(['\n', '\r'])
        .to_owned())
}

/// The repository an attempt starts from.
struct Repo {
    root: PathBuf,
    /// The working directory relative to the root (`src/` or empty).
    prefix: String,
    /// Tracked changes not yet committed, as a diff against HEAD.
    diff: Vec<u8>,
}

async fn repo(cwd: &Path) -> Result<Repo> {
    let root = git_line(cwd, &["rev-parse", "--show-toplevel"])
        .await
        .context("--attempt needs a git repository")?;
    git(cwd, &["rev-parse", "--verify", "--quiet", "HEAD"], None)
        .await
        .context("--attempt needs a repository with at least one commit")?;
    Ok(Repo {
        prefix: git_line(cwd, &["rev-parse", "--show-prefix"]).await?,
        diff: git(cwd, &[DIFF, &["HEAD"]].concat(), None).await?,
        root: PathBuf::from(root),
    })
}

/// A detached worktree at HEAD plus the repository's uncommitted tracked changes; returns
/// the tree ID of that starting state.
async fn add_worktree(repo: &Repo, path: &Path) -> Result<String> {
    let path_str = path.to_str().context("worktree path is not valid UTF-8")?;
    git(
        &repo.root,
        &["worktree", "add", "--detach", path_str, "HEAD"],
        None,
    )
    .await?;
    if !repo.diff.is_empty() {
        git(
            path,
            &["apply", "--whitespace=nowarn", "-"],
            Some(repo.diff.clone()),
        )
        .await
        .context("copying uncommitted changes into the worktree")?;
    }
    git(path, &["add", "-A"], None).await?;
    git_line(path, &["write-tree"]).await
}

fn patch_stats(patch: &str) -> PatchStats {
    let mut stats = PatchStats::default();
    for line in patch.lines() {
        if line.starts_with("diff --git ") {
            stats.files += 1;
        } else if line.starts_with('+') && !line.starts_with("+++ ") {
            stats.added += 1;
        } else if line.starts_with('-') && !line.starts_with("--- ") {
            stats.removed += 1;
        }
    }
    stats
}

/// Run the test command in a worktree, keeping the full log and its last lines.
async fn run_test(command: &str, cwd: &Path, log: &Path, timeout: Duration) -> TestRun {
    let mut run = TestRun {
        command: command.to_owned(),
        ..Default::default()
    };
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(log).and_then(|f| Ok((f.try_clone()?, f)));
    let Ok((out, err)) = file else {
        run.output_tail = format!("could not create {}", log.display());
        return run;
    };
    let mut test = tokio::process::Command::new("sh");
    test.args(["-c", command])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(unix)]
    test.process_group(0);
    let mut child = match test.spawn() {
        Ok(child) => child,
        Err(error) => {
            run.output_tail = format!("could not start the test command: {error}");
            return run;
        }
    };
    let group = Group(child.id());
    match tokio::time::timeout(timeout, child.wait()).await {
        Ok(Ok(status)) => run.exit_code = status.code(),
        Ok(Err(error)) => run.output_tail = format!("waiting for the test command: {error}"),
        Err(_) => run.timed_out = true,
    }
    drop(group);
    let text = String::from_utf8_lossy(&std::fs::read(log).unwrap_or_default()).into_owned();
    let lines: Vec<&str> = text.lines().collect();
    let tail = lines[lines.len().saturating_sub(TEST_TAIL_LINES)..].join("\n");
    if !tail.is_empty() {
        run.output_tail = tail;
    }
    run
}

struct Session<'a> {
    config: &'a crate::config::Config,
    models: &'a [crate::catalog::Model],
    roster: &'a [crate::catalog::Entry],
    relay: bool,
    key: &'a str,
    dir: &'a Path,
    timeout: Duration,
    test: &'a str,
    /// The repository root when the cwd is inside one; read-only agents may read all of it.
    root: Option<&'a Path>,
}

impl Session<'_> {
    fn command(&self, model: &str, cwd: &Path, args: Vec<String>) -> Result<std::process::Command> {
        let plan = crate::launch::plan(
            self.config,
            self.models,
            self.roster,
            Some(model.to_owned()),
            self.relay,
        )?;
        let mut command = crate::launch::claude_command(&plan, self.key, None);
        command.args(args).current_dir(cwd).env(GUARD, "1");
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        for name in NESTED_SESSION.iter().chain(GIT_ENV) {
            command.env_remove(name);
        }
        Ok(command)
    }

    fn record(&self, member: &mut Member, outcome: Outcome) {
        member.status = outcome.status.to_owned();
        member.error = outcome.error;
        member.duration_ms = outcome.duration_ms;
        member.input_tokens = outcome.usage.input;
        member.cache_read_tokens = outcome.usage.cache_read;
        member.cache_creation_tokens = outcome.usage.cache_creation;
        member.output_tokens = outcome.usage.output;
        member.cost_usd_estimate = cost(self.config, self.roster, &member.model, outcome.usage);
        member.report = outcome.report;
    }
}

async fn run_member(
    cx: &Session<'_>,
    mode: Mode,
    question: &str,
    repo: Option<&Repo>,
    cwd: &Path,
    mut member: Member,
) -> Member {
    eprintln!("panel: {} {} started", member.label, member.model);
    let raw = cx.dir.join(&member.label);
    let worktree = cx.dir.join("wt").join(&member.label);
    let (work_dir, start) = match repo {
        Some(repo) => match add_worktree(repo, &worktree).await {
            Ok(start) => (worktree.join(&repo.prefix), Some(start)),
            Err(error) => {
                remove_worktree(repo, &worktree).await;
                member.status = "failed".into();
                member.error = Some(format!("{error:#}"));
                eprintln!("panel: {} {} failed: {error:#}", member.label, member.model);
                return member;
            }
        },
        None => (cwd.to_path_buf(), None),
    };
    let allowed = tools(READ_ONLY_BASH, cx.test, &["WebSearch"]);
    let (access, schema) = match mode {
        Mode::Opinion => (
            Access {
                tools: "Read,Grep,Glob,Bash,WebSearch",
                allowed,
                permission_mode: "dontAsk",
                add_dir: cx.root,
            },
            opinion_schema(),
        ),
        Mode::Attempt => (
            Access {
                tools: "Read,Grep,Glob,Bash,Edit,Write,WebSearch",
                allowed,
                permission_mode: "acceptEdits",
                add_dir: Some(&worktree),
            },
            attempt_schema(),
        ),
    };
    let args = agent_args(
        &member.model,
        &access,
        &panelist_prompt(mode, cx.test),
        &schema,
    );
    let outcome = match cx.command(&member.model, &work_dir, args) {
        Ok(command) => run_agent(command, question.to_owned(), cx.timeout, &raw).await,
        Err(error) => Outcome::failed(format!("{error:#}")),
    };
    cx.record(&mut member, outcome);
    if let (Some(repo), Some(start)) = (repo, start) {
        collect_attempt(cx, &worktree, &work_dir, &start, &mut member).await;
        remove_worktree(repo, &worktree).await;
    }
    match &member.error {
        None => eprintln!(
            "panel: {} {} finished in {}",
            member.label,
            member.model,
            duration(member.duration_ms)
        ),
        Some(error) => eprintln!(
            "panel: {} {} {}: {error}",
            member.label, member.model, member.status
        ),
    }
    member
}

/// Save an attempt's diff against its starting state and run the tests on it.
async fn collect_attempt(
    cx: &Session<'_>,
    worktree: &Path,
    work_dir: &Path,
    start: &str,
    member: &mut Member,
) {
    let patch = match git(worktree, &["add", "-A"], None).await {
        Ok(_) => git(worktree, &[DIFF, &["--cached", start]].concat(), None).await,
        Err(error) => Err(error),
    };
    let patch = match patch {
        Ok(patch) => patch,
        Err(error) => {
            member.warn(format!("collecting the patch failed: {error:#}"));
            return;
        }
    };
    let name = format!("{}.patch", member.label);
    if let Err(error) = crate::store::write_private(&cx.dir.join(&name), &patch) {
        member.warn(format!("saving the patch failed: {error:#}"));
        return;
    }
    member.patch = Some(name);
    member.patch_stats = Some(patch_stats(&String::from_utf8_lossy(&patch)));
    if member.status == "ok" && !cx.test.trim().is_empty() {
        eprintln!(
            "panel: {} {} running `{}`",
            member.label, member.model, cx.test
        );
        let log = cx.dir.join(format!("{}.test.log", member.label));
        member.test = Some(run_test(cx.test, work_dir, &log, cx.timeout).await);
    }
}

impl Member {
    fn warn(&mut self, message: String) {
        self.error = Some(match self.error.take() {
            Some(error) => format!("{error}; {message}"),
            None => message,
        });
    }
}

async fn remove_worktree(repo: &Repo, path: &Path) {
    let Some(path_str) = path.to_str() else {
        return;
    };
    if git(
        &repo.root,
        &["worktree", "remove", "--force", path_str],
        None,
    )
    .await
    .is_err()
    {
        let _ = std::fs::remove_dir_all(path);
        let _ = git(&repo.root, &["worktree", "prune"], None).await;
    }
}

/// The judge's prompt: the question and each successful report, by label only.
fn judge_input(panel: &Panel, dir: &Path) -> String {
    let mut text = format!("# Task given to every panelist\n\n{}\n\n", panel.question);
    if panel.mode == Mode::Attempt {
        text.push_str("Each panelist made the change in its own copy of the repository. byoclaude collected each copy's diff and ran the test command in it. Your working directory is the repository before any attempt.\n\n");
    }
    text.push_str("# Reports\n");
    for member in panel.members.iter().filter(|m| m.status == "ok") {
        text.push_str(&format!(
            "\n## Panelist {}\n\n```json\n{}\n```\n",
            member.label,
            serde_json::to_string_pretty(&member.report).unwrap_or_default()
        ));
        if let Some(name) = &member.patch {
            let patch = std::fs::read(dir.join(name)).unwrap_or_default();
            let patch = String::from_utf8_lossy(&patch);
            if patch.trim().is_empty() {
                text.push_str("\nPatch: no changes.\n");
            } else {
                text.push_str(&format!(
                    "\nPatch:\n\n```diff\n{}\n```\n",
                    truncate(&patch, JUDGE_PATCH_BYTES)
                ));
            }
        }
        if let Some(test) = &member.test {
            let result = match (test.timed_out, test.exit_code) {
                (true, _) => "timed out".to_owned(),
                (false, Some(code)) => format!("exit code {code}"),
                (false, None) => "killed".to_owned(),
            };
            text.push_str(&format!(
                "\nTest command `{}`: {result}\n\n```text\n{}\n```\n",
                test.command, test.output_tail
            ));
        }
    }
    text
}

fn label_models(panel: &Panel) -> Vec<(String, String)> {
    panel
        .members
        .iter()
        .map(|m| (m.label.clone(), m.model.clone()))
        .collect()
}

/// Replace panelist labels with model IDs: label fields become the ID, and "Panelist A" in
/// prose becomes "openai/gpt-…".
pub fn unmask(verdict: &Value, labels: &[(String, String)]) -> Value {
    fn walk(value: &Value, labels: &[(String, String)], label_field: bool) -> Value {
        match value {
            Value::String(text) if label_field => {
                let bare = text.trim().trim_start_matches("Panelist ").trim();
                match labels.iter().find(|(l, _)| l == bare) {
                    Some((_, model)) => json!(model),
                    None => json!(prose(text, labels)),
                }
            }
            Value::String(text) => json!(prose(text, labels)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| walk(v, labels, label_field)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| {
                        let field = matches!(k.as_str(), "panelist" | "panelists" | "winner");
                        (k.clone(), walk(v, labels, field))
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
    fn prose(text: &str, labels: &[(String, String)]) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        while let Some(at) = rest.find("Panelist ") {
            out.push_str(&rest[..at]);
            let after = &rest[at + "Panelist ".len()..];
            let end = after
                .find(|c: char| !c.is_ascii_alphanumeric())
                .unwrap_or(after.len());
            match labels.iter().find(|(l, _)| l == &after[..end]) {
                Some((_, model)) => out.push_str(model),
                None => out.push_str(&rest[at..at + "Panelist ".len() + end]),
            }
            rest = &after[end..];
        }
        out.push_str(rest);
        out
    }
    walk(verdict, labels, false)
}

/// The judge's verdict as recorded: labels mapped to models, and no winner as null.
fn settle(verdict: &Value, labels: &[(String, String)]) -> Value {
    let mut verdict = unmask(verdict, labels);
    if verdict["winner"] == "none" {
        verdict["winner"] = Value::Null;
    }
    verdict
}

fn duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs >= 60 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}.{}s", secs, (ms % 1000) / 100)
    }
}

fn tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

fn cost_text(value: &Value) -> String {
    match value {
        Value::Number(n) => format!("${:.4}", n.as_f64().unwrap_or(0.0)),
        Value::String(s) => s.clone(),
        _ => "-".into(),
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("").trim()
}

fn list_of<'a>(value: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    value[key].as_array().into_iter().flatten()
}

fn names(value: &Value) -> String {
    match value {
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn render_report(out: &mut String, member: &Member) {
    let report = &member.report;
    if !str_of(report, "answer").is_empty() {
        out.push_str(&format!("{}\n\n", str_of(report, "answer")));
        for finding in list_of(report, "findings") {
            out.push_str(&format!(
                "- {} ({} confidence). Evidence: {}\n",
                str_of(finding, "claim"),
                str_of(finding, "confidence"),
                str_of(finding, "evidence")
            ));
        }
        for item in list_of(report, "uncertainties").filter_map(Value::as_str) {
            out.push_str(&format!("- Uncertain: {item}\n"));
        }
    } else {
        out.push_str(&format!("{}\n\n", str_of(report, "summary")));
        for item in list_of(report, "changes").filter_map(Value::as_str) {
            out.push_str(&format!("- {item}\n"));
        }
        if !str_of(report, "verification").is_empty() {
            out.push_str(&format!(
                "- Verification: {}\n",
                str_of(report, "verification")
            ));
        }
        for item in list_of(report, "concerns").filter_map(Value::as_str) {
            out.push_str(&format!("- Concern: {item}\n"));
        }
    }
    out.push('\n');
}

/// The verdict as Markdown, ending with the ledger table and path; `patches` adds each
/// attempt's diff.
pub fn render(panel: &Panel, dir: &Path, patches: bool) -> String {
    let mut out = String::new();
    let v = &panel.verdict;
    let mode = match panel.mode {
        Mode::Opinion => "opinion",
        Mode::Attempt => "attempt",
    };
    out.push_str(&format!("# Panel {} ({mode})\n\n", panel.id));
    out.push_str(&format!(
        "Question: {}\n\n",
        truncate(panel.question.lines().next().unwrap_or("").trim(), 300)
    ));
    let members: Vec<String> = panel.members.iter().map(|m| m.model.clone()).collect();
    out.push_str(&format!("Panel: {}", members.join(", ")));
    if let Some(judge) = &panel.judge {
        out.push_str(&format!(". Judge: {}", judge.model));
    }
    out.push_str("\n\n");
    for warning in &panel.warnings {
        out.push_str(&format!("Warning: {warning}\n\n"));
    }
    if v.is_object() {
        out.push_str(&format!("## Summary\n\n{}\n\n", str_of(v, "summary")));
        let agreement: Vec<&Value> = list_of(v, "agreement").collect();
        if !agreement.is_empty() {
            out.push_str("## Agreement\n\n");
            for item in agreement {
                out.push_str(&format!(
                    "- {} ({})\n",
                    str_of(item, "point"),
                    names(&item["panelists"])
                ));
            }
            out.push('\n');
        }
        let conflicts: Vec<&Value> = list_of(v, "conflicts").collect();
        if !conflicts.is_empty() {
            out.push_str("## Conflicts\n\n");
            for conflict in conflicts {
                out.push_str(&format!("### {}\n\n", str_of(conflict, "topic")));
                for position in list_of(conflict, "positions") {
                    out.push_str(&format!(
                        "- {}: {}\n",
                        names(&position["panelist"]),
                        str_of(position, "position")
                    ));
                }
                out.push_str(&format!(
                    "\nAssessment: {}\n\n",
                    str_of(conflict, "assessment")
                ));
            }
        }
        let unique: Vec<&Value> = list_of(v, "unique").collect();
        if !unique.is_empty() {
            out.push_str("## Unique findings\n\n");
            for item in unique {
                out.push_str(&format!(
                    "- {}: {} Assessment: {}\n",
                    names(&item["panelist"]),
                    str_of(item, "finding"),
                    str_of(item, "assessment")
                ));
            }
            out.push('\n');
        }
        let blind: Vec<&str> = list_of(v, "blind_spots")
            .filter_map(Value::as_str)
            .collect();
        if !blind.is_empty() {
            out.push_str("## Blind spots\n\n");
            for item in blind {
                out.push_str(&format!("- {item}\n"));
            }
            out.push('\n');
        }
        if panel.mode == Mode::Attempt {
            out.push_str("## Ranking\n\n");
            for (rank, item) in list_of(v, "ranking").enumerate() {
                out.push_str(&format!(
                    "{}. {}: {}\n",
                    rank + 1,
                    names(&item["panelist"]),
                    str_of(item, "assessment")
                ));
            }
            out.push_str(&format!(
                "\nWinner: {}\n\n",
                v["winner"].as_str().unwrap_or("none")
            ));
            if !str_of(v, "merge").is_empty() {
                out.push_str(&format!("Merge: {}\n\n", str_of(v, "merge")));
            }
        }
        out.push_str(&format!(
            "## Recommendation\n\n{}\n\nConfidence: {}\n\n",
            str_of(v, "recommendation"),
            str_of(v, "confidence")
        ));
    } else {
        out.push_str("No verdict. The reports as given:\n\n");
        for member in panel.members.iter().filter(|m| m.status == "ok") {
            out.push_str(&format!("## {} ({})\n\n", member.model, member.label));
            render_report(&mut out, member);
        }
    }
    if panel.mode == Mode::Attempt {
        out.push_str("## Patches\n\n");
        for member in &panel.members {
            let detail = match (&member.patch, &member.patch_stats) {
                (Some(name), Some(stats)) if stats.files > 0 => format!(
                    "{} files, +{} -{}: {}",
                    stats.files,
                    stats.added,
                    stats.removed,
                    dir.join(name).display()
                ),
                (Some(_), _) => "no changes".into(),
                (None, _) => "no patch".into(),
            };
            let test = match &member.test {
                Some(t) if t.timed_out => ", tests timed out".to_owned(),
                Some(t) => match t.exit_code {
                    Some(0) => ", tests passed".to_owned(),
                    Some(code) => format!(", tests failed (exit {code})"),
                    None => ", tests did not finish".to_owned(),
                },
                None => String::new(),
            };
            out.push_str(&format!(
                "- {} ({}): {detail}{test}\n",
                member.model, member.label
            ));
            if let (true, Some(name)) = (patches, &member.patch) {
                let patch = std::fs::read(dir.join(name)).unwrap_or_default();
                let patch = String::from_utf8_lossy(&patch);
                if !patch.trim().is_empty() {
                    out.push_str(&format!(
                        "\n```diff\n{}\n```\n\n",
                        truncate(patch.trim_end(), JUDGE_PATCH_BYTES)
                    ));
                }
            }
        }
        out.push('\n');
        if let Some(applied) = &panel.applied {
            out.push_str(&format!(
                "Applied {} ({}) at {}{}.\n\n",
                applied.model,
                applied.label,
                applied.at,
                if applied.method == "3way-conflicts" {
                    ", with conflicts"
                } else {
                    ""
                }
            ));
        } else if v["winner"].is_string() {
            out.push_str(&format!(
                "Apply the winner with `byoclaude panel apply {}`; name a panelist to apply another.\n\n",
                panel.id
            ));
        }
    }
    out.push_str("## Ledger\n\n| Model | Role | Status | Time | Tokens in/out | Cost |\n|---|---|---|---|---|---|\n");
    let rows = panel
        .members
        .iter()
        .map(|m| (m, format!("Panelist {}", m.label)))
        .chain(panel.judge.iter().map(|j| (j, "Judge".to_owned())));
    for (member, role) in rows {
        let status = match &member.error {
            Some(error) if member.status != "ok" => {
                format!("{}: {}", member.status, truncate(error, 80))
            }
            _ => member.status.clone(),
        };
        out.push_str(&format!(
            "| {} | {role} | {} | {} | {}/{} | {} |\n",
            member.model,
            status.replace('|', "\\|"),
            duration(member.duration_ms),
            tokens(member.input_tokens + member.cache_read_tokens + member.cache_creation_tokens),
            tokens(member.output_tokens),
            cost_text(&member.cost_usd_estimate)
        ));
    }
    if !panel.skipped.is_empty() {
        let skipped: Vec<String> = panel
            .skipped
            .iter()
            .map(|s| format!("{} ({})", s.model, s.status))
            .collect();
        out.push_str(&format!("\nSkipped: {}\n", skipped.join(", ")));
    }
    out.push_str(&format!(
        "\nTokens in include cached input. Costs are estimates from roster prices, cached input at the full input price; \"plan\" counts against a plan. Ledger: {}\n",
        dir.join("panel.json").display()
    ));
    out
}

fn read_question(question: String) -> Result<String> {
    let question = if question == "-" {
        std::io::read_to_string(std::io::stdin()).context("reading the question from stdin")?
    } else {
        question
    };
    let question = question.trim().to_owned();
    if question.is_empty() {
        bail!("the question is empty");
    }
    Ok(question)
}

/// Run a panel and print its verdict.
pub async fn run(options: Options) -> Result<()> {
    if std::env::var_os(GUARD).is_some() {
        bail!("byoclaude panel does not run inside a panel ({GUARD} is set)");
    }
    if let Some(name) = ["list", "show", "apply"]
        .into_iter()
        .find(|name| options.question == *name)
    {
        bail!(
            "{name:?} was read as the question because an option came before it; run `byoclaude panel {name}` with its options after it"
        );
    }
    let question = read_question(options.question)?;
    let mode = if options.attempt {
        Mode::Attempt
    } else {
        Mode::Opinion
    };
    let config = crate::config::load()?;
    let relay = config.relay && crate::launch::claude_signed_in();
    let models = crate::catalog::load().await.unwrap_or_default();
    if !crate::catalog::roster_exists() {
        crate::catalog::refresh(&config).await;
    }
    let roster = crate::catalog::roster_cached();
    let size = options.size.unwrap_or(config.panel.size) as usize;
    let timeout = Duration::from_secs(options.timeout.unwrap_or(config.panel.timeout_secs).max(1));
    let test = options
        .test
        .unwrap_or_else(|| config.panel.test_command.clone());
    let view = std::cell::OnceCell::new();
    let roster_candidates =
        || view.get_or_init(|| candidates(&crate::roster::roster_json(&config, &roster, false)));
    let mut warnings = Vec::new();
    let explicit: Vec<String> = if options.models.is_empty() {
        config.panel.models.clone()
    } else {
        options.models
    };
    let selection = if explicit.is_empty() {
        if size < 2 {
            bail!("a panel needs at least two members");
        }
        let selection = select(&config, roster_candidates(), relay, size);
        if selection.members.len() < 2 {
            let skipped: Vec<String> = selection
                .skipped
                .iter()
                .map(|s| format!("{} ({})", s.model, s.status))
                .collect();
            bail!(
                "fewer than two ready models from distinct providers{}; sign in to more providers or pass --models",
                if skipped.is_empty() {
                    String::new()
                } else {
                    format!(" (skipped: {})", skipped.join(", "))
                }
            );
        }
        selection
    } else {
        let mut members: Vec<String> = Vec::new();
        for id in explicit.iter().map(|m| canonical(m.trim())) {
            if !id.is_empty() && !members.contains(&id) {
                members.push(id);
            }
        }
        if members.len() < 2 {
            bail!("a panel needs at least two distinct models");
        }
        Selection {
            members,
            skipped: Vec::new(),
        }
    };
    let judge = match options
        .judge
        .or_else(|| (!config.panel.judge.is_empty()).then(|| config.panel.judge.clone()))
    {
        Some(judge) => canonical(judge.trim()),
        None => {
            let (judge, warning) =
                pick_judge(&config, roster_candidates(), relay, &selection.members)
                    .context("no model available to judge; pass --judge")?;
            warnings.extend(warning);
            judge
        }
    };
    if selection.members.contains(&judge) && warnings.is_empty() {
        warnings.push(format!("{judge} judges a panel it sits on"));
    }
    for id in selection.members.iter().chain([&judge]) {
        crate::launch::plan(&config, &models, &roster, Some(id.clone()), relay)
            .with_context(|| format!("cannot run {id}"))?;
    }
    let cwd = std::env::current_dir().context("reading the current directory")?;
    let repo = match mode {
        Mode::Attempt => Some(repo(&cwd).await?),
        Mode::Opinion => None,
    };
    let root = match &repo {
        Some(repo) => Some(repo.root.clone()),
        None => git_line(&cwd, &["rev-parse", "--show-toplevel"])
            .await
            .ok()
            .map(PathBuf::from),
    };
    let key = crate::launch::bridge_key().await?;
    if relay
        && selection
            .members
            .iter()
            .chain([&judge])
            .any(|m| is_claude(m))
    {
        crate::launch::relay_note();
    }
    let created = unix_now();
    let id = new_id(created);
    let dir = panels_dir()?.join(&id);
    crate::store::private_dir(&dir)?;
    let mut panel = Panel {
        id: id.clone(),
        created: rfc3339(created),
        cwd: cwd.display().to_string(),
        root: repo.as_ref().map(|r| r.root.display().to_string()),
        question: question.clone(),
        mode,
        members: selection
            .members
            .iter()
            .enumerate()
            .map(|(i, model)| Member {
                label: label(i),
                model: model.clone(),
                status: "running".into(),
                ..Default::default()
            })
            .collect(),
        skipped: selection.skipped,
        judge: None,
        verdict: Value::Null,
        warnings,
        applied: None,
    };
    save(&dir, &panel)?;
    for warning in &panel.warnings {
        eprintln!("panel: warning: {warning}");
    }
    if !panel.skipped.is_empty() {
        let skipped: Vec<String> = panel
            .skipped
            .iter()
            .map(|s| format!("{} ({})", s.model, s.status))
            .collect();
        eprintln!("panel: skipped {}", skipped.join(", "));
    }
    eprintln!(
        "panel: {id}: {} on {}, judge {judge}; ledger {}",
        mode_name(mode),
        selection.members.join(", "),
        dir.display()
    );
    if repo.is_some() {
        eprintln!(
            "panel: each attempt starts from HEAD plus uncommitted tracked changes; untracked files are not copied"
        );
        if test.trim().is_empty() {
            eprintln!(
                "panel: no test command (--test or panel.test_command); attempts are judged without test results"
            );
        }
    }
    let cx = Session {
        config: &config,
        models: &models,
        roster: &roster,
        relay,
        key: &key,
        dir: &dir,
        timeout,
        test: &test,
        root: root.as_deref(),
    };
    let runs = panel
        .members
        .iter()
        .cloned()
        .map(|member| run_member(&cx, mode, &question, repo.as_ref(), &cwd, member));
    let members = tokio::select! {
        members = futures_util::future::join_all(runs) => Some(members),
        () = interrupted() => None,
    };
    let Some(members) = members else {
        if let Some(repo) = &repo {
            for member in &panel.members {
                remove_worktree(repo, &dir.join("wt").join(&member.label)).await;
            }
        }
        let _ = std::fs::remove_dir(dir.join("wt"));
        for member in &mut panel.members {
            member.status = "interrupted".into();
        }
        save(&dir, &panel)?;
        bail!("interrupted; the record is in {}", dir.display());
    };
    panel.members = members;
    let _ = std::fs::remove_dir(dir.join("wt"));
    save(&dir, &panel)?;
    let reports = panel.members.iter().filter(|m| m.status == "ok").count();
    let mut failure = None;
    if reports < 2 {
        failure = Some(format!(
            "only {reports} of {} panelists returned a report; a panel needs two to judge",
            panel.members.len()
        ));
    } else {
        eprintln!("panel: judge {judge} comparing {reports} reports");
        let labels: Vec<String> = panel
            .members
            .iter()
            .filter(|m| m.status == "ok")
            .map(|m| m.label.clone())
            .collect();
        let input = judge_input(&panel, &dir);
        let _ = crate::store::write_private(&dir.join("judge.prompt.md"), input.as_bytes());
        let access = Access {
            tools: "Read,Grep,Glob,Bash",
            allowed: tools(READ_ONLY_BASH, "", &[]),
            permission_mode: "dontAsk",
            add_dir: cx.root,
        };
        let args = agent_args(
            &judge,
            &access,
            JUDGE_PROMPT,
            &verdict_schema(mode, &labels),
        );
        let outcome = match cx.command(&judge, &cwd, args) {
            Ok(command) => {
                let raw = dir.join("judge");
                tokio::select! {
                    outcome = run_agent(command, input, timeout, &raw) => outcome,
                    () = interrupted() => Outcome {
                        status: "interrupted",
                        ..Outcome::failed("interrupted".into())
                    },
                }
            }
            Err(error) => Outcome::failed(format!("{error:#}")),
        };
        let mut member = Member {
            label: "judge".into(),
            model: judge.clone(),
            ..Default::default()
        };
        cx.record(&mut member, outcome);
        let verdict = std::mem::take(&mut member.report);
        if member.status == "ok" {
            panel.verdict = settle(&verdict, &label_models(&panel));
        } else {
            failure = Some(format!(
                "the judge {judge} {}: {}",
                member.status,
                member.error.clone().unwrap_or_default()
            ));
        }
        panel.judge = Some(member);
        save(&dir, &panel)?;
    }
    if options.json {
        println!("{}", serde_json::to_string_pretty(&panel)?);
    } else {
        print!("{}", render(&panel, &dir, false));
    }
    match failure {
        Some(failure) => bail!("{failure}"),
        None => Ok(()),
    }
}

/// Resolves on Ctrl-C or SIGTERM. Agents run in their own process groups, so the terminal's
/// interrupt reaches only byoclaude, which then stops them.
async fn interrupted() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            },
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Opinion => "opinion",
        Mode::Attempt => "attempt",
    }
}

/// `byoclaude panel list`.
pub fn list() -> Result<()> {
    let ids = ids()?;
    if ids.is_empty() {
        println!("No panels yet. Run `byoclaude panel \"<question>\"`.");
        return Ok(());
    }
    let root = panels_dir()?;
    println!(
        "{:<22} {:<8} {:>7}  {:<28} QUESTION",
        "ID", "MODE", "REPORTS", "OUTCOME"
    );
    for id in ids {
        let Ok(panel) = load_from(&root.join(&id)) else {
            continue;
        };
        let ok = panel.members.iter().filter(|m| m.status == "ok").count();
        let outcome = if let Some(applied) = &panel.applied {
            format!("applied {}", applied.model)
        } else if panel.verdict.is_object() {
            match panel.mode {
                Mode::Attempt => format!(
                    "winner {}",
                    panel.verdict["winner"].as_str().unwrap_or("none")
                ),
                Mode::Opinion => format!(
                    "{} confidence",
                    panel.verdict["confidence"].as_str().unwrap_or("?")
                ),
            }
        } else {
            "no verdict".into()
        };
        println!(
            "{id:<22} {:<8} {:>7}  {:<28} {}",
            mode_name(panel.mode),
            format!("{ok}/{}", panel.members.len()),
            truncate(&outcome, 28),
            truncate(panel.question.lines().next().unwrap_or(""), 60)
        );
    }
    Ok(())
}

/// `byoclaude panel show <id>`.
pub fn show(id: &str, json_output: bool) -> Result<()> {
    let dir = find(id)?;
    let panel = load_from(&dir)?;
    if json_output {
        println!("{}", serde_json::to_string_pretty(&panel)?);
    } else {
        print!("{}", render(&panel, &dir, true));
    }
    Ok(())
}

/// `byoclaude panel apply <id> [panelist]`: apply the winner's or the named patch to the
/// repository's working tree.
pub async fn apply(id: &str, panelist: Option<String>) -> Result<()> {
    let dir = find(id)?;
    let mut panel = load_from(&dir)?;
    if panel.mode != Mode::Attempt {
        bail!(
            "panel {} asked for opinions; only attempt panels have patches",
            panel.id
        );
    }
    let wanted = match panelist {
        Some(name) => name,
        None => panel.verdict["winner"]
            .as_str()
            .map(str::to_owned)
            .context("the judge named no winner; name a panelist to apply")?,
    };
    let member = panel
        .members
        .iter()
        .find(|m| {
            m.label
                .eq_ignore_ascii_case(wanted.trim_start_matches("Panelist ").trim())
                || m.model == wanted
                || m.model == canonical(&wanted)
        })
        .with_context(|| format!("no panelist {wanted:?} in panel {}", panel.id))?
        .clone();
    let patch = member
        .patch
        .as_ref()
        .map(|name| dir.join(name))
        .with_context(|| format!("{} ({}) left no patch", member.model, member.label))?;
    if std::fs::metadata(&patch).map(|m| m.len()).unwrap_or(0) == 0 {
        bail!("{} ({}) made no changes", member.model, member.label);
    }
    let root = PathBuf::from(panel.root.clone().unwrap_or_else(|| panel.cwd.clone()));
    let patch_str = patch.to_str().context("patch path is not valid UTF-8")?;
    let unmerged = async || -> Result<Vec<String>> {
        let out = git(&root, &["diff", "--name-only", "--diff-filter=U"], None).await?;
        Ok(String::from_utf8_lossy(&out)
            .lines()
            .map(str::to_owned)
            .collect())
    };
    let conflicted_before = unmerged().await?;
    let mut conflicts = Vec::new();
    let method = match git(&root, &["apply", "--whitespace=nowarn", patch_str], None).await {
        Ok(_) => "direct",
        Err(direct) => match git(
            &root,
            &["apply", "--3way", "--whitespace=nowarn", patch_str],
            None,
        )
        .await
        {
            Ok(_) => "3way",
            Err(three_way) => {
                conflicts = unmerged().await?;
                conflicts.retain(|path| !conflicted_before.contains(path));
                if conflicts.is_empty() {
                    bail!(
                        "the patch does not apply to {}.\n{direct:#}\nWith --3way: {three_way:#}",
                        root.display()
                    );
                }
                "3way-conflicts"
            }
        },
    };
    panel.applied = Some(Applied {
        label: member.label.clone(),
        model: member.model.clone(),
        at: rfc3339(unix_now()),
        method: method.into(),
    });
    save(&dir, &panel)?;
    let applied = format!("{} ({}) to {}", member.model, member.label, root.display());
    match method {
        "direct" => println!("Applied {applied}. Review with `git diff`."),
        "3way" => println!(
            "Applied {applied} with a three-way merge; the changes are staged. Review with `git diff HEAD`."
        ),
        _ => bail!(
            "applied {applied} with a three-way merge that left conflicts in {}; the changes are staged and the working tree is modified. Resolve the conflict markers, then review with `git diff HEAD`",
            conflicts.join(", ")
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, cost: f64, status: &str) -> Candidate {
        Candidate {
            id: id.into(),
            provider: split(id).0.into(),
            cost_output: cost,
            status: status.into(),
        }
    }

    fn roster() -> Vec<Candidate> {
        vec![
            candidate("claude-3-opus-20240229", 75.0, "ready"),
            candidate("claude-opus-5-5", 25.0, "ready"),
            candidate("claude-haiku-4-5", 5.0, "ready"),
            candidate("openai/gpt-sol", 0.0, "capped"),
            candidate("openai/gpt-luna", 0.0, "ready"),
            candidate("ollama/qwen", 0.0, "ready"),
            candidate("zai/glm-5", 3.2, "ready"),
            candidate("kimi/kimi-k3", 2.5, "ready"),
            candidate("deepseek/chat", 1.0, "no-key"),
        ]
    }

    #[test]
    fn selection_takes_each_providers_best_ready_model() {
        let config = crate::config::Config::default();
        let selection = select(&config, &roster(), true, 3);
        assert_eq!(
            selection.members,
            ["claude-opus-5-5", "zai/glm-5", "kimi/kimi-k3"]
        );
        assert!(selection.skipped.contains(&Skipped {
            model: "openai/gpt-sol".into(),
            status: "capped".into()
        }));
        assert!(selection.skipped.contains(&Skipped {
            model: "deepseek/chat".into(),
            status: "no-key".into()
        }));
        // Local models come last.
        let all = select(&config, &roster(), true, 10);
        assert_eq!(all.members.last().map(String::as_str), Some("ollama/qwen"));
        assert_eq!(all.members.len(), 5);
        // Without the relay, Claude is not ready.
        let no_relay = select(&config, &roster(), false, 3);
        assert_eq!(
            no_relay.members,
            ["zai/glm-5", "kimi/kimi-k3", "openai/gpt-luna"]
        );
        assert!(no_relay.skipped.iter().any(|s| s.status == "relay-off"));
    }

    #[test]
    fn judge_comes_from_outside_the_panel() {
        let config = crate::config::Config::default();
        let members = vec!["zai/glm-5".to_owned(), "kimi/kimi-k3".to_owned()];
        assert_eq!(
            pick_judge(&config, &roster(), true, &members),
            Some(("claude-opus-5-5".into(), None))
        );
        assert_eq!(
            pick_judge(&config, &roster(), false, &members),
            Some(("openai/gpt-luna".into(), None))
        );
        let members = vec!["claude-opus-5-5".to_owned(), "zai/glm-5".to_owned()];
        assert_eq!(
            pick_judge(&config, &roster(), true, &members),
            Some(("kimi/kimi-k3".into(), None))
        );
        let only = [
            candidate("zai/glm-5", 3.2, "ready"),
            candidate("kimi/kimi-k3", 2.5, "ready"),
        ];
        let members = vec!["kimi/kimi-k3".to_owned(), "zai/glm-5".to_owned()];
        let (judge, warning) = pick_judge(&config, &only, true, &members).unwrap();
        assert_eq!(judge, "zai/glm-5");
        assert!(warning.unwrap().contains("judges its own panel"));
    }

    #[test]
    fn candidates_read_the_roster_view() {
        let view = json!({"models": [
            {"id": "zai/glm-5", "provider": "zai", "status": "ready", "price_per_million": {"input": 1.0, "output": 3.2}},
            {"id": "broken"}
        ]});
        assert_eq!(candidates(&view), [candidate("zai/glm-5", 3.2, "ready")]);
    }

    #[test]
    fn unmask_maps_labels_to_models() {
        let labels = vec![
            ("A".to_owned(), "openai/gpt-sol".to_owned()),
            ("B".to_owned(), "zai/glm-5".to_owned()),
        ];
        let verdict = json!({
            "summary": "Panelist A and Panelist B agree; Panelist AB is not a label.",
            "agreement": [{"point": "x", "panelists": ["A", "Panelist B"]}],
            "conflicts": [{"topic": "t", "positions": [{"panelist": "B", "position": "p"}], "assessment": "Panelist B is right"}],
            "winner": "A",
            "ranking": [{"panelist": "A", "assessment": "best"}],
        });
        let mapped = unmask(&verdict, &labels);
        assert_eq!(
            mapped["summary"],
            "openai/gpt-sol and zai/glm-5 agree; Panelist AB is not a label."
        );
        assert_eq!(
            mapped["agreement"][0]["panelists"],
            json!(["openai/gpt-sol", "zai/glm-5"])
        );
        assert_eq!(
            mapped["conflicts"][0]["positions"][0]["panelist"],
            "zai/glm-5"
        );
        assert_eq!(mapped["conflicts"][0]["assessment"], "zai/glm-5 is right");
        assert_eq!(mapped["winner"], "openai/gpt-sol");
        assert_eq!(
            settle(&json!({"winner": "none"}), &labels)["winner"],
            Value::Null
        );
        assert_eq!(
            unmask(&json!({"winner": null}), &labels)["winner"],
            Value::Null
        );
    }

    fn sample() -> Panel {
        Panel {
            id: "20261009-120000-abcd".into(),
            created: "2026-10-09T12:00:00Z".into(),
            cwd: "/repo".into(),
            root: Some("/repo".into()),
            question: "Make it safe".into(),
            mode: Mode::Attempt,
            members: vec![
                Member {
                    label: "A".into(),
                    model: "openai/gpt-sol".into(),
                    status: "ok".into(),
                    duration_ms: 83_000,
                    input_tokens: 12_345,
                    output_tokens: 678,
                    cost_usd_estimate: json!("plan"),
                    report: json!({"summary": "did it", "changes": ["a"], "verification": "ran tests", "concerns": []}),
                    patch: Some("A.patch".into()),
                    patch_stats: Some(PatchStats {
                        files: 1,
                        added: 3,
                        removed: 1,
                    }),
                    test: Some(TestRun {
                        command: "make test".into(),
                        exit_code: Some(0),
                        timed_out: false,
                        output_tail: "ok".into(),
                    }),
                    ..Default::default()
                },
                Member {
                    label: "B".into(),
                    model: "zai/glm-5".into(),
                    status: "timeout".into(),
                    error: Some("no result within 900s".into()),
                    cost_usd_estimate: json!(0.0123),
                    ..Default::default()
                },
            ],
            skipped: vec![Skipped {
                model: "kimi/kimi-k3".into(),
                status: "capped".into(),
            }],
            judge: Some(Member {
                label: "judge".into(),
                model: "claude-opus-5-5".into(),
                status: "ok".into(),
                cost_usd_estimate: json!("plan"),
                ..Default::default()
            }),
            verdict: json!({
                "summary": "One attempt works.",
                "agreement": [{"point": "Lock the pool", "panelists": ["openai/gpt-sol"]}],
                "conflicts": [],
                "unique": [{"panelist": "openai/gpt-sol", "finding": "Race in release.", "assessment": "Confirmed."}],
                "blind_spots": ["No load test"],
                "ranking": [{"panelist": "openai/gpt-sol", "assessment": "Correct and tested."}],
                "winner": "openai/gpt-sol",
                "merge": "Nothing else.",
                "recommendation": "Apply it.",
                "confidence": "high"
            }),
            warnings: Vec::new(),
            applied: None,
        }
    }

    #[test]
    fn render_shows_verdict_and_ledger() {
        let text = render(&sample(), Path::new("/ledger"), false);
        for needle in [
            "# Panel 20261009-120000-abcd (attempt)",
            "Panel: openai/gpt-sol, zai/glm-5. Judge: claude-opus-5-5",
            "## Summary\n\nOne attempt works.",
            "- Lock the pool (openai/gpt-sol)",
            "- openai/gpt-sol: Race in release. Assessment: Confirmed.",
            "- No load test",
            "1. openai/gpt-sol: Correct and tested.",
            "Winner: openai/gpt-sol",
            "Confidence: high",
            "- openai/gpt-sol (A): 1 files, +3 -1: /ledger/A.patch, tests passed",
            "- zai/glm-5 (B): no patch",
            "byoclaude panel apply 20261009-120000-abcd",
            "| openai/gpt-sol | Panelist A | ok | 1m 23s | 12.3k/678 | plan |",
            "| zai/glm-5 | Panelist B | timeout: no result within 900s | 0.0s | 0/0 | $0.0123 |",
            "| claude-opus-5-5 | Judge | ok |",
            "Skipped: kimi/kimi-k3 (capped)",
            "Ledger: /ledger/panel.json",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(!text.contains("## Conflicts"));
        let mut no_verdict = sample();
        no_verdict.verdict = Value::Null;
        let text = render(&no_verdict, Path::new("/ledger"), false);
        assert!(text.contains("No verdict."), "{text}");
        assert!(text.contains("## openai/gpt-sol (A)\n\ndid it"), "{text}");
    }

    #[test]
    fn ledger_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let panel = sample();
        save(dir.path(), &panel).unwrap();
        assert_eq!(load_from(dir.path()).unwrap(), panel);
        let raw: Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("panel.json")).unwrap()).unwrap();
        assert_eq!(raw["mode"], "attempt");
        assert_eq!(raw["members"][0]["cost_usd_estimate"], "plan");
        assert_eq!(raw["members"][1]["report"], Value::Null);
        assert!(raw["members"][1].get("patch").is_none());
    }

    #[test]
    fn results_parse_structured_output_and_errors() {
        let ok = r#"{"type":"result","is_error":false,"result":"done","structured_output":{"answer":"yes"},"usage":{"input_tokens":10,"cache_read_input_tokens":5,"output_tokens":7}}"#;
        let (report, usage) = parse_result(ok);
        assert_eq!(report.unwrap()["answer"], "yes");
        assert_eq!(
            usage,
            Usage {
                input: 10,
                cache_read: 5,
                cache_creation: 0,
                output: 7
            }
        );
        let text = r#"{"type":"result","is_error":false,"result":"{\"answer\":\"text\"}"}"#;
        assert_eq!(parse_result(text).0.unwrap()["answer"], "text");
        let error = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"result":"boom"}"#;
        assert!(parse_result(error).0.unwrap_err().contains("boom"));
        assert!(parse_result("not json").0.is_err());
        assert!(
            parse_result(r#"{"type":"result","result":"plain"}"#)
                .0
                .is_err()
        );
    }

    #[test]
    fn ids_and_times_are_utc_and_sortable() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_760_000_000), "2025-10-09T08:53:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        let id = new_id(1_760_000_000);
        assert!(id.starts_with("20251009-085320-"), "{id}");
        assert_eq!(id.len(), "20251009-085320-abcd".len());
        assert!(new_id(1_760_000_001) > new_id(1_760_000_000));
    }

    #[test]
    fn agent_arguments_restrict_tools_and_carry_the_schema() {
        let allowed = tools(READ_ONLY_BASH, "cargo test", &["WebSearch"]);
        assert!(allowed.contains(&"Bash(git diff:*)".to_owned()));
        assert!(allowed.contains(&"Bash(cargo test:*)".to_owned()));
        // Bare file-tool rules would approve paths outside the working directories.
        for bare in ["Read", "Grep", "Glob", "Edit", "Write", "WebFetch"] {
            assert!(!allowed.iter().any(|t| t == bare), "{bare}");
        }
        let access = Access {
            tools: "Read,Grep",
            allowed,
            permission_mode: "dontAsk",
            add_dir: Some(Path::new("/repo")),
        };
        let args = agent_args("zai/glm-5", &access, "sys", &opinion_schema());
        let position = |flag: &str| args.iter().position(|a| a == flag).unwrap();
        assert_eq!(args[position("--model") + 1], "zai/glm-5");
        assert_eq!(args[position("--tools") + 1], "Read,Grep");
        assert_eq!(args[position("--permission-mode") + 1], "dontAsk");
        assert_eq!(args[position("--add-dir") + 1], "/repo");
        assert!(args.contains(&"--restricted".to_owned()));
        assert!(args.contains(&"Bash(git diff *--output*)".to_owned()));
        assert!(args.contains(&"Bash(rg *--pre *)".to_owned()));
        // Each variadic list is closed by another option.
        assert!(position("--allowedTools") > position("--disallowedTools"));
        assert!(position("--append-system-prompt") > position("--allowedTools"));
        assert!(args[position("--json-schema") + 1].contains("uncertainties"));
        let schema = verdict_schema(Mode::Attempt, &["A".into(), "B".into()]);
        assert_eq!(
            schema["properties"]["winner"]["enum"],
            json!(["A", "B", "none"])
        );
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&json!("merge"))
        );
        assert!(
            verdict_schema(Mode::Opinion, &["A".into()])["properties"]
                .get("winner")
                .is_none()
        );
    }

    #[test]
    fn patch_stats_count_files_and_lines() {
        let patch = "diff --git a/f b/f\n--- a/f\n+++ b/f\n@@ -1 +1,2 @@\n-x\n+y\n+z\ndiff --git a/g b/g\nnew file mode 100644\n--- /dev/null\n+++ b/g\n@@ -0,0 +1 @@\n+n\n";
        assert_eq!(
            patch_stats(patch),
            PatchStats {
                files: 2,
                added: 3,
                removed: 1
            }
        );
    }
}
