//! What Claude learns about byoclaude: the guide printed by `byoclaude --skill`, and the
//! session plugin `byoclaude run` loads (a pointer skill plus one agent per model).
use std::path::PathBuf;

use anyhow::Result;

/// The agent contract for this version, printed by `byoclaude --skill`.
pub const GUIDE: &str = r#"# byoclaude in this session

This Claude Code session runs through byoclaude, a local router. Besides Claude, it can
run models from other providers the user signed in to: OpenAI GPT on a ChatGPT plan,
Z.ai GLM, Kimi, MiniMax, DeepSeek, OpenRouter, local Ollama or LM Studio models, and
others. The installed binary is the authority; this guide matches its version.

## See what is available

Run `byoclaude models --json`. It returns the live roster:

- `models[].id`: the model ID (`provider/model`, or a native Claude ID like `claude-opus-5-5`).
- `models[].agent`: the subagent type that runs on that model (`byoclaude:<name>`).
- `models[].status`: `ready`, `capped` (plan or rate limit hit recently), `no-key`, or `offline`.
- Context window, max output, reasoning, tool and image support, effort levels, and price
  per million tokens (an API-price estimate; plan usage is not billed per token).
- `roles`: the session's main, background and subagent models.

Re-run it when a model fails; statuses change.

## Run work on another model

Use the Agent tool with `subagent_type` set to the model's `agent` value. That agent is a
general-purpose agent with your tools, running on that model. The Agent tool's own
`model` field only accepts Claude aliases; do not put other model IDs there.

Good reasons to use another model:

- A second opinion or an independent review: a different model family catches different mistakes.
- Parallel or bulk work where a fast, cheap model is enough.
- Very long inputs that need a larger context window.
- The user asked for a specific model or provider.

Do not switch models without a reason; the main model keeps the session's context.

## Delegate well

- Give the subagent the objective, the files or scope it may touch, constraints, and what
  to report back. It does not see this conversation.
- Treat its answer as evidence to verify, not a verdict. Say which model produced it and
  where you disagree.
- Skip models whose status is not `ready`. If a model fails with a plan or rate limit,
  pick another and tell the user.
- Keep overlapping edits to one agent at a time; parallel agents share the working tree.
- Mention when you used a costly model for a large job.

## Ask a panel

A panel puts one question to several models at once. Each answers independently as a
headless Claude Code agent that can read the repository and run commands; a judge from
another provider compares the anonymized reports and returns a verdict: agreement,
conflicts, unique findings, blind spots, a recommendation.

Worth the cost when being wrong is expensive:

- Reviews of risky changes: security, data loss, auth, billing, migrations.
- Competing hypotheses when debugging.
- Design decisions with real trade-offs.
- The user asks for a panel or several opinions.

Not for routine edits, simple questions, or anything latency-sensitive. A panel takes
minutes and runs several models.

Run it with Bash in the background:

```
byoclaude panel "<self-contained question with paths and constraints>"
```

- Panelists do not see this conversation. Make the question stand alone: the goal, the
  files and paths, constraints, and what a good answer contains.
- `--attempt` has each panelist try the change in its own git worktree; byoclaude collects
  each patch and runs the test command when one is set. Untracked files are not copied
  into the worktrees.
- `--models a,b,c` picks the panel, `--judge <id>` the judge, `--size <N>` the panel size,
  `--test "<cmd>"` the test command. By default byoclaude picks ready models from
  distinct providers.

Read the verdict as evidence, not a ruling. Conflicts and blind spots matter more than
the majority; check the cited evidence before acting on it.

After an attempt panel, review the patch with `byoclaude panel show <id>` before applying
it, then run `byoclaude panel apply <id> [panelist]` (the winner when no panelist is
named). Nothing is applied automatically.

Tell the user which models sat on the panel, which judged, and what it cost (the ledger
table at the end of the verdict).
"#;

const SKILL: &str = r#"---
name: byoclaude
description: Use other AI models in this session through byoclaude - GPT, GLM, Kimi, MiniMax, DeepSeek, local models and more. Load before choosing a model for a subagent, delegating to a non-Claude model, getting a second opinion from another model family, when the user names a model or provider, or before asking a panel of several models for second opinions, a review, or competing attempts at a change.
---

Run `byoclaude --skill` once per session and follow it. It is the guide for the
installed byoclaude version. Then use `byoclaude models --json` for the live roster.
"#;

const PANEL_COMMAND: &str = r#"---
description: Put a question to a panel of models from several providers and get a judged verdict
argument-hint: <question>
---

Ask a byoclaude panel about: $ARGUMENTS

1. Turn that into a self-contained question. The panelists do not see this conversation:
   include the goal, the relevant files and paths, constraints, and what a good answer
   contains. Add `--attempt` only if the user wants the change tried several ways.
2. Run `byoclaude panel "<question>"` with Bash in the background; it takes minutes.
   See `byoclaude --skill` for options.
3. When it finishes, report the verdict: the recommendation, conflicts and blind spots,
   and where you disagree. Name the models on the panel and the judge, and what it cost.
   Do not apply any patch without the user's go-ahead.
"#;

/// Agent name for a model ID: `openai/gpt-5.6-sol` becomes `openai-gpt-5-6-sol`.
pub fn agent_name(model: &str) -> String {
    let mut name = String::new();
    for c in model.to_ascii_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if !(c == '-' && name.ends_with('-')) {
            name.push(c);
        }
    }
    name.trim_matches('-').to_owned()
}

/// Subagent type Claude passes to the Agent tool for a model.
pub fn agent_type(model: &str) -> String {
    format!("byoclaude:{}", agent_name(model))
}

fn agent_file(row: &crate::launch::Row) -> String {
    let what = if row.description.is_empty() {
        row.label.clone()
    } else {
        format!("{} ({})", row.label, row.description)
    };
    format!(
        "---\nname: {name}\ndescription: General-purpose agent running on {what}, model {id}. Use for work you want done by this model; see the byoclaude skill for when.\nmodel: {id}\n---\n\nYou are a general-purpose agent running on {id}. Complete the task you are given using the available tools, verify your work, and report back concisely: what you did, what you found, and anything uncertain.\n",
        name = agent_name(&row.id),
        id = row.id,
    )
}

/// Write the session plugin and return its directory.
pub fn prepare(rows: &[crate::launch::Row]) -> Result<PathBuf> {
    let dir = crate::store::home()?.join("plugin");
    let write = |relative: &str, body: &str| -> Result<()> {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            crate::store::private_dir(parent)?;
        }
        crate::store::write_private(&path, body.as_bytes())
    };
    write(
        ".claude-plugin/plugin.json",
        &serde_json::json!({
            "name": "byoclaude",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Models from other providers in this Claude Code session",
        })
        .to_string(),
    )?;
    write("skills/byoclaude/SKILL.md", SKILL)?;
    write("commands/panel.md", PANEL_COMMAND)?;
    for row in rows {
        write(
            &format!("agents/{}.md", agent_name(&row.id)),
            &agent_file(row),
        )?;
    }
    // Remove agents for models no longer offered. Concurrent launches write the same files,
    // so a file another launch already removed is not an error.
    let current: Vec<String> = rows
        .iter()
        .map(|r| format!("{}.md", agent_name(&r.id)))
        .collect();
    if let Ok(entries) = std::fs::read_dir(dir.join("agents")) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".md") && !current.contains(&name) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_names_are_safe() {
        assert_eq!(agent_name("openai/gpt-5.6-sol"), "openai-gpt-5-6-sol");
        assert_eq!(
            agent_name("openrouter/qwen/Qwen3.7--Max"),
            "openrouter-qwen-qwen3-7-max"
        );
        assert_eq!(agent_type("zai/glm-5.3"), "byoclaude:zai-glm-5-3");
    }

    #[test]
    fn agent_file_names_its_model() {
        let row = crate::launch::Row {
            id: "kimi/k3".into(),
            label: "K3".into(),
            description: "Kimi For Coding".into(),
        };
        let file = agent_file(&row);
        assert!(file.contains("model: kimi/k3\n"));
        assert!(file.starts_with("---\nname: kimi-k3\n"));
    }

    #[test]
    fn guide_and_command_cover_panels() {
        assert!(GUIDE.contains("## Ask a panel"));
        assert!(GUIDE.contains("byoclaude panel apply <id> [panelist]"));
        assert!(SKILL.contains("panel"));
        assert!(PANEL_COMMAND.starts_with("---\ndescription: "));
        assert!(PANEL_COMMAND.contains("\nargument-hint: <question>\n---\n"));
        assert!(PANEL_COMMAND.contains("$ARGUMENTS"));
        assert!(PANEL_COMMAND.contains("byoclaude panel \""));
    }
}
