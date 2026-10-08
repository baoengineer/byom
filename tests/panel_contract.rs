#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_byoclaude");

/// A fake `claude`: records its arguments, environment, working directory and prompt per
/// model, then prints a `-p --output-format json` result whose structured output fits the
/// schema it was given. In attempt mode, model-a appends to notes.txt and file.txt and model-b
/// changes nothing; groq models fail; slow models never finish and leave two sleeps, one in a
/// process group of its own as Claude Code's shell commands are.
const FAKE_CLAUDE: &str = r##"#!/bin/sh
if [ "$1" = auth ]; then
  printf '{"loggedIn":false,"authMethod":"none"}\n'
  exit 0
fi
model=""; schema=""; prev=""
for a in "$@"; do
  case "$prev" in
    --model) model="$a" ;;
    --json-schema) schema="$a" ;;
  esac
  prev="$a"
done
rec="$FAKE_RECORD/$(printf %s "$model" | tr '/' '_')"
printf 'arg=<%s>\n' "$@" > "$rec.args"
{
  printf 'panel=%s\n' "${BYOCLAUDE_PANEL-unset}"
  for v in CLAUDECODE CLAUDE_CODE_ENTRYPOINT CLAUDE_CODE_SESSION_ID CLAUDE_CODE_PLUGIN_DIRS CLAUDE_PID; do
    printf '%s=%s\n' "$v" "$(eval "printf %s \"\${$v-unset}\"")"
  done
  printf 'base=%s\nmodel_env=%s\ncwd=%s\n' "$ANTHROPIC_BASE_URL" "$ANTHROPIC_MODEL" "$(pwd -P)"
  if [ -f untracked.txt ]; then echo untracked=copied; else echo untracked=absent; fi
  if [ -f file.txt ] && grep -q local file.txt; then echo local=present; fi
} > "$rec.env"
cat > "$rec.prompt"
case "$model" in
  groq/*) echo "synthetic failure" >&2; exit 1 ;;
  */slow)
    sleep 60 & echo $! > "$rec.sleep"
    perl -e 'setpgrp(0, 0); exec "sleep", "61"' & echo $! > "$rec.detached"
    wait; exit 0 ;;
esac
usage='"usage":{"input_tokens":1000,"output_tokens":200,"cache_read_input_tokens":500,"cache_creation_input_tokens":0}'
case "$schema" in
  *winner*)
    printf '{"type":"result","subtype":"success","is_error":false,"result":"","structured_output":{"summary":"Panelist A made the change; Panelist B made none.","agreement":[],"conflicts":[],"unique":[{"panelist":"A","finding":"Adds notes","assessment":"Correct"}],"blind_spots":[],"ranking":[{"panelist":"A","assessment":"Works"},{"panelist":"B","assessment":"Empty"}],"winner":"A","merge":"Nothing from Panelist B.","recommendation":"Apply Panelist A.","confidence":"high"},%s}\n' "$usage"
    ;;
  *agreement*)
    printf '{"type":"result","subtype":"success","is_error":false,"result":"","structured_output":{"summary":"Panelist A found the race; Panelist B agreed.","agreement":[{"point":"There is a race","panelists":["A","B"]}],"conflicts":[{"topic":"Fix","positions":[{"panelist":"A","position":"lock"},{"panelist":"B","position":"atomic"}],"assessment":"Panelist B is simpler"}],"unique":[{"panelist":"B","finding":"Second caller","assessment":"Verified"}],"blind_spots":["Load"],"recommendation":"Use an atomic.","confidence":"medium"},%s}\n' "$usage"
    ;;
  *verification*)
    if [ "$model" = openai/model-a ]; then
      printf 'change by %s\n' "$model" >> notes.txt
      printf 'edit by %s\n' "$model" >> file.txt
    fi
    printf '{"type":"result","subtype":"success","is_error":false,"result":"","structured_output":{"summary":"Change by %s","changes":["notes.txt"],"verification":"none","concerns":[]},%s}\n' "$model" "$usage"
    ;;
  *)
    printf '{"type":"result","subtype":"success","is_error":false,"result":"","structured_output":{"answer":"Answer %s","findings":[{"claim":"race","evidence":"src/pool.rs:1","confidence":"high"}],"uncertainties":[]},%s}\n' "${model##*-}" "$usage"
    ;;
esac
"##;

fn port() -> u16 {
    static PORT: std::sync::OnceLock<u16> = std::sync::OnceLock::new();
    *PORT.get_or_init(|| {
        TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    })
}

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    bin: PathBuf,
    record: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let (home, bin, record) = (root.join("state"), root.join("bin"), root.join("record"));
        for dir in [&home, &bin, &record] {
            fs::create_dir(dir).unwrap();
        }
        let fake = bin.join("claude");
        fs::write(&fake, FAKE_CLAUDE).unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir_all(home.join("cache")).unwrap();
        fs::write(
            home.join("cache/models.json"),
            r#"[{"slug":"model-a","display_name":"","description":"","context_window":0,"effort_levels":[],"default_effort":null,"listed":true}]"#,
        )
        .unwrap();
        // An existing roster keeps panels from refreshing over the network.
        fs::write(home.join("cache/roster.json"), "[]").unwrap();
        fs::write(
            home.join("config.json"),
            r#"{"upstream_base_url":"http://127.0.0.1:9/v1"}"#,
        )
        .unwrap();
        Fixture {
            _temp: temp,
            root,
            home,
            bin,
            record,
        }
    }

    fn command(&self, cwd: &Path) -> Command {
        let mut command = Command::new(BIN);
        command
            .env_clear()
            .current_dir(cwd)
            .env("BYOCLAUDE_HOME", &self.home)
            .env("HOME", &self.home)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin:/usr/local/bin", self.bin.display()),
            )
            .env("BYOCLAUDE_PORT", port().to_string())
            .env("FAKE_RECORD", &self.record);
        command
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.record.join(name)).unwrap_or_else(|_| panic!("no {name}"))
    }

    fn panel_dir(&self, output: &Output) -> PathBuf {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout
            .lines()
            .find(|l| l.starts_with("# Panel "))
            .unwrap_or_else(|| panic!("no panel heading: {stdout}"));
        let id = line["# Panel ".len()..].split(' ').next().unwrap();
        self.home.join("panels").join(id)
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .env("HOME", dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
    String::from_utf8_lossy(&output.stdout).into_owned()
}

async fn start_bridge(fixture: &Fixture) -> OwnedChild {
    let bridge = OwnedChild(
        fixture
            .command(&fixture.root)
            .args(["bridge", "--port", &port().to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let url = format!("http://127.0.0.1:{}/health", port());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(key) = fs::read_to_string(fixture.home.join("bridge.key"))
            && let Ok(response) = client.get(&url).bearer_auth(key.trim()).send().await
            && response.status() == 200
        {
            return bridge;
        }
        assert!(Instant::now() < deadline, "bridge readiness timed out");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn read_json(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap()).unwrap()
}

#[test]
fn panel_refuses_to_nest() {
    let fixture = Fixture::new();
    let output = fixture
        .command(&fixture.root)
        .args(["panel", "--models", "openai/a,kimi/b", "question"])
        .env("BYOCLAUDE_PANEL", "1")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("BYOCLAUDE_PANEL"));
    assert!(
        fs::read_dir(&fixture.record).unwrap().next().is_none(),
        "no claude may run"
    );
    assert!(!fixture.home.join("panels").exists());
}

// Both scenarios share the bridge port, so they run in one test.
#[tokio::test]
async fn panel_process_contract() {
    let fixture = Fixture::new();
    let _bridge = start_bridge(&fixture).await;

    // Opinion panel, with one member failing.
    let work = fixture.root.join("work");
    fs::create_dir(&work).unwrap();
    let output = fixture
        .command(&work)
        .args([
            "panel",
            "--models",
            "openai/model-a,kimi/model-b,groq/fails",
            "--judge",
            "zai/judge",
            "--test",
            "make check",
            "Is there a race in the pool?",
        ])
        .env("CLAUDECODE", "1")
        .env("CLAUDE_CODE_ENTRYPOINT", "cli")
        .env("CLAUDE_CODE_SESSION_ID", "outer")
        .env("CLAUDE_CODE_PLUGIN_DIRS", "/outer/plugins")
        .env("CLAUDE_PID", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    for model in ["openai_model-a", "kimi_model-b", "groq_fails", "zai_judge"] {
        let args = fixture.read(&format!("{model}.args"));
        let env = fixture.read(&format!("{model}.env"));
        for arg in [
            "arg=<-p>",
            "arg=<--model>",
            "arg=<--json-schema>",
            "arg=<--output-format>",
            "arg=<--no-session-persistence>",
            "arg=<--strict-mcp-config>",
            "arg=<--settings>",
            "arg=<Bash(git diff:*)>",
            "arg=<--restricted>",
            "arg=<--permission-mode>",
            "arg=<dontAsk>",
            "arg=<Bash(git diff *--output*)>",
        ] {
            assert!(
                args.lines().any(|l| l == arg),
                "{model} missing {arg}: {args}"
            );
        }
        assert!(!args.contains("--plugin-dir"), "{args}");
        assert!(
            !args.contains("Edit") && !args.contains("WebFetch"),
            "{args}"
        );
        // Reads need no rule inside the working directories; a bare rule would allow any path.
        for bare in ["arg=<Read>", "arg=<Grep>", "arg=<Glob>"] {
            assert!(
                !args.lines().any(|l| l == bare),
                "{model} has {bare}: {args}"
            );
        }
        for line in [
            "panel=1",
            "CLAUDECODE=unset",
            "CLAUDE_CODE_ENTRYPOINT=unset",
            "CLAUDE_CODE_SESSION_ID=unset",
            "CLAUDE_CODE_PLUGIN_DIRS=unset",
            "CLAUDE_PID=unset",
            &format!("base=http://127.0.0.1:{}", port()),
            &format!("cwd={}", work.display()),
        ] {
            assert!(
                env.lines().any(|l| l == line),
                "{model} missing {line}: {env}"
            );
        }
    }
    let panelist = fixture.read("openai_model-a.args");
    assert!(panelist.contains("arg=<Bash(make check:*)>"), "{panelist}");
    assert!(panelist.contains("uncertainties"), "{panelist}");
    assert_eq!(
        fixture.read("openai_model-a.prompt"),
        "Is there a race in the pool?"
    );
    assert!(
        fixture
            .read("openai_model-a.env")
            .contains("model_env=openai/model-a")
    );
    let judge_args = fixture.read("zai_judge.args");
    assert!(judge_args.contains("agreement"), "{judge_args}");
    assert!(!judge_args.contains("WebSearch"), "{judge_args}");
    // The judge sees labels, not model names, and only successful reports.
    let judge_prompt = fixture.read("zai_judge.prompt");
    assert!(judge_prompt.contains("## Panelist A") && judge_prompt.contains("## Panelist B"));
    assert!(!judge_prompt.contains("## Panelist C"), "{judge_prompt}");
    for name in ["model-a", "model-b", "openai", "kimi", "groq"] {
        assert!(
            !judge_prompt.contains(name),
            "judge saw {name}: {judge_prompt}"
        );
    }
    for needle in [
        "openai/model-a found the race; kimi/model-b agreed.",
        "- There is a race (openai/model-a, kimi/model-b)",
        "- openai/model-a: lock",
        "Assessment: kimi/model-b is simpler",
        "- kimi/model-b: Second caller Assessment: Verified",
        "Confidence: medium",
        "| openai/model-a | Panelist A | ok |",
        "| groq/fails | Panelist C | failed:",
        "| zai/judge | Judge | ok |",
        "1.5k/200 | plan |",
    ] {
        assert!(stdout.contains(needle), "missing {needle:?}:\n{stdout}");
    }
    assert!(stderr.contains("panel: C groq/fails failed"), "{stderr}");
    let dir = fixture.panel_dir(&output);
    let record = read_json(&dir.join("panel.json"));
    assert_eq!(record["mode"], "opinion");
    assert_eq!(record["question"], "Is there a race in the pool?");
    assert_eq!(record["members"][0]["model"], "openai/model-a");
    assert_eq!(record["members"][0]["status"], "ok");
    assert_eq!(record["members"][0]["input_tokens"], 1000);
    assert_eq!(record["members"][0]["cache_read_tokens"], 500);
    assert_eq!(record["members"][0]["cost_usd_estimate"], "plan");
    assert_eq!(record["members"][0]["report"]["answer"], "Answer a");
    assert_eq!(record["members"][2]["status"], "failed");
    assert_eq!(record["judge"]["model"], "zai/judge");
    assert_eq!(
        record["verdict"]["agreement"][0]["panelists"],
        serde_json::json!(["openai/model-a", "kimi/model-b"])
    );
    assert!(dir.join("A.out.json").exists());
    let id = dir.file_name().unwrap().to_str().unwrap().to_owned();
    let list = fixture
        .command(&work)
        .args(["panel", "list"])
        .output()
        .unwrap();
    assert!(list.status.success());
    assert!(String::from_utf8_lossy(&list.stdout).contains(&id));
    let show = fixture
        .command(&work)
        .args(["panel", "show", &id])
        .output()
        .unwrap();
    assert!(show.status.success());
    assert!(String::from_utf8_lossy(&show.stdout).contains("Use an atomic."));
    let show = fixture
        .command(&work)
        .args(["panel", "show", &id, "--json"])
        .output()
        .unwrap();
    let shown: Value = serde_json::from_slice(&show.stdout).unwrap();
    assert_eq!(shown["id"], id.as_str());

    // Too few reports: no judge, non-zero exit.
    let output = fixture
        .command(&work)
        .args([
            "panel",
            "--models",
            "openai/model-a,groq/fails",
            "--judge",
            "zai/second-judge",
            "q",
        ])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("needs two to judge"));
    assert!(!fixture.record.join("zai_second-judge.args").exists());

    // The question from stdin, a member past the timeout, a failing judge, and --json.
    let mut child = fixture
        .command(&work)
        .args([
            "panel",
            "--models",
            "openai/model-a,kimi/model-b,zai/slow",
            "--judge",
            "groq/judge",
            "--timeout",
            "2",
            "--json",
            "-",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"  Which lock guards idle?\n").unwrap();
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "a failed judge must fail the run");
    assert!(stderr.contains("the judge groq/judge failed"), "{stderr}");
    let record: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| {
        panic!(
            "--json output: {e}: {}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(record["question"], "Which lock guards idle?");
    assert_eq!(
        fixture.read("openai_model-a.prompt"),
        "Which lock guards idle?"
    );
    assert_eq!(record["members"][0]["status"], "ok");
    assert_eq!(record["members"][2]["model"], "zai/slow");
    assert_eq!(record["members"][2]["status"], "timeout");
    assert_eq!(record["judge"]["status"], "failed");
    assert_eq!(record["verdict"], Value::Null);
    // The timeout kills everything the member started, not just its claude.
    for name in ["zai_slow.sleep", "zai_slow.detached"] {
        let pid = fixture.read(name);
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(
            !alive,
            "{name}: process {} survived the timeout",
            pid.trim()
        );
    }

    // Attempt panel in a repository with uncommitted and untracked changes.
    let repo = fixture.root.join("repo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    fs::write(repo.join("file.txt"), "base\n").unwrap();
    git(&repo, &["add", "file.txt"]);
    git(&repo, &["commit", "-qm", "init"]);
    fs::write(repo.join("file.txt"), "base\nlocal\n").unwrap();
    fs::write(repo.join("untracked.txt"), "x\n").unwrap();
    // byoclaude's git sees this config; patches must still come out in `git apply` format.
    fs::write(
        fixture.home.join(".gitconfig"),
        "[diff]\n\tnoprefix = true\n\tmnemonicPrefix = true\n\texternal = /bin/false\n[color]\n\tui = always\n",
    )
    .unwrap();
    let output = fixture
        .command(&repo)
        .args([
            "panel",
            "--attempt",
            "--models",
            "openai/model-a,kimi/model-b",
            "--judge",
            "zai/judge",
            "--test",
            "grep -q local file.txt && echo tests-ran",
            "Add notes",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stderr.contains("untracked files are not copied"),
        "{stderr}"
    );
    let args = fixture.read("openai_model-a.args");
    for arg in [
        "arg=<--restricted>",
        "arg=<--permission-mode>",
        "arg=<acceptEdits>",
        "arg=<Read,Grep,Glob,Bash,Edit,Write,WebSearch>",
        "arg=<--add-dir>",
    ] {
        assert!(args.lines().any(|l| l == arg), "missing {arg}: {args}");
    }
    // acceptEdits confines edits to the working directories; a bare rule would not.
    for bare in ["arg=<Edit>", "arg=<Write>"] {
        assert!(!args.lines().any(|l| l == bare), "has {bare}: {args}");
    }
    assert!(!args.contains("--plugin-dir"));
    let env = fixture.read("openai_model-a.env");
    assert!(env.contains("untracked=absent"), "{env}");
    assert!(env.contains("local=present"), "{env}");
    assert!(env.contains("/wt/A"), "{env}");
    assert!(stdout.contains("Winner: openai/model-a"), "{stdout}");
    assert!(stdout.contains("tests passed"), "{stdout}");
    let dir = fixture.panel_dir(&output);
    let patch = fs::read_to_string(dir.join("A.patch")).unwrap();
    assert!(patch.contains("+change by openai/model-a"), "{patch}");
    assert!(
        patch.contains("diff --git a/file.txt b/file.txt") && !patch.contains('\u{1b}'),
        "patch must be in git apply format: {patch}"
    );
    assert!(
        !patch.contains("+local"),
        "patch must start from the uncommitted state: {patch}"
    );
    assert_eq!(fs::read_to_string(dir.join("B.patch")).unwrap(), "");
    let record = read_json(&dir.join("panel.json"));
    assert_eq!(record["mode"], "attempt");
    assert_eq!(record["members"][0]["test"]["exit_code"], 0);
    assert!(
        record["members"][0]["test"]["output_tail"]
            .as_str()
            .unwrap()
            .contains("tests-ran")
    );
    assert_eq!(record["members"][0]["patch_stats"]["files"], 2);
    assert_eq!(record["verdict"]["winner"], "openai/model-a");
    // Worktrees are gone, from disk and from git.
    assert!(!dir.join("wt").exists());
    assert_eq!(git(&repo, &["worktree", "list"]).lines().count(), 1);
    assert!(!repo.join("notes.txt").exists());

    let id = dir.file_name().unwrap().to_str().unwrap().to_owned();
    let show = fixture
        .command(&repo)
        .args(["panel", "show", &id])
        .output()
        .unwrap();
    let shown = String::from_utf8_lossy(&show.stdout);
    assert!(
        shown.contains("```diff") && shown.contains("+change by openai/model-a"),
        "show prints each patch: {shown}"
    );
    let empty = fixture
        .command(&repo)
        .args(["panel", "apply", &id, "B"])
        .output()
        .unwrap();
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("made no changes"));
    let applied = fixture
        .command(&repo)
        .args(["panel", "apply", &id])
        .output()
        .unwrap();
    assert!(applied.status.success(), "{applied:?}");
    assert_eq!(
        fs::read_to_string(repo.join("notes.txt")).unwrap(),
        "change by openai/model-a\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("file.txt")).unwrap(),
        "base\nlocal\nedit by openai/model-a\n"
    );
    let record = read_json(&dir.join("panel.json"));
    assert_eq!(record["applied"]["model"], "openai/model-a");
    assert_eq!(record["applied"]["label"], "A");
    assert_eq!(record["applied"]["method"], "direct");

    // The tree moved on: the direct apply fails and a three-way merge takes over.
    let reset = |file: &str| {
        git(&repo, &["reset", "-q"]);
        let _ = fs::remove_file(repo.join("notes.txt"));
        fs::write(repo.join("file.txt"), file).unwrap();
        git(&repo, &["add", "file.txt"]);
    };
    reset("BASE\nlocal\n");
    let merged = fixture
        .command(&repo)
        .args(["panel", "apply", &id, "A"])
        .output()
        .unwrap();
    assert!(merged.status.success(), "{merged:?}");
    assert!(String::from_utf8_lossy(&merged.stdout).contains("staged"));
    assert_eq!(
        fs::read_to_string(repo.join("file.txt")).unwrap(),
        "BASE\nlocal\nedit by openai/model-a\n"
    );
    assert_eq!(
        read_json(&dir.join("panel.json"))["applied"]["method"],
        "3way"
    );

    // A three-way merge that conflicts modifies the tree, says so, and fails.
    reset("base\nLOCAL\n");
    let conflicted = fixture
        .command(&repo)
        .args(["panel", "apply", &id, "A"])
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&conflicted.stderr);
    assert!(!conflicted.status.success());
    assert!(stderr.contains("conflicts in file.txt"), "{stderr}");
    assert!(
        fs::read_to_string(repo.join("file.txt"))
            .unwrap()
            .contains("<<<<<<<")
    );
    assert_eq!(
        read_json(&dir.join("panel.json"))["applied"]["method"],
        "3way-conflicts"
    );
}
