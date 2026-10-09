//! What Claude learns about byom: the guide printed by `byom --skill`, and the
//! session plugin `byom run` loads (a pointer skill plus one agent per model).
use std::path::PathBuf;

use anyhow::Result;

/// The agent contract for this version, printed by `byom --skill`.
pub const GUIDE: &str = r#"# byom in this session

This Claude Code session runs through byom, a local router. Besides Claude, it can
run models from other providers the user signed in to: OpenAI GPT on a ChatGPT plan,
Z.ai GLM, Kimi, MiniMax, DeepSeek, OpenRouter, local Ollama or LM Studio models, and
others. The installed binary is the authority; this guide matches its version.

## See what is available

Run `byom models --json`. It returns the live roster:

- `models[].id`: the model ID (`provider/model`, or a native Claude ID like `claude-opus-5-5`).
- `models[].agent`: the subagent type that runs on that model (`byom:<name>`); null for Claude models, which the Agent tool names directly.
- `models[].status`: `ready`, `capped` (plan or rate limit hit in the last five hours), `no-key`, or `signed-out`.
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

## Run a team of models

When the user turned on teams (`byom config set teams true`), Claude Code's agent teams
are on, and a teammate runs on whatever model its agent type names. Spawn a teammate with
a `name` and `subagent_type` set to a model's `agent` value, such as
`byom:openai-gpt-5-6-sol`.

- Give each teammate a role that fits its model: a strong reasoning model to review or
  plan, a fast or cheap one for bulk reading, tests or log scans, a long-context one for
  large inputs. `byom models --json` shows price, context and status.
- Each teammate owns different files; two teammates editing one file overwrite each other.
- Have teammates message each other to challenge findings, not only report to you.
- Three or four teammates is usually enough; every teammate is a full session and costs
  its own tokens on its provider.
"#;

const SKILL: &str = r#"---
name: byom
description: Use other AI models in this session through byom - GPT, GLM, Kimi, MiniMax, DeepSeek, local models and more. Load before choosing a model for a subagent, delegating to a non-Claude model, getting a second opinion from another model family, or when the user names a model or provider.
---

Run `byom --skill` once per session and follow it. It is the guide for the
installed byom version. Then use `byom models --json` for the live roster.
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
    format!("byom:{}", agent_name(model))
}

fn agent_file(row: &crate::launch::Row) -> String {
    let what = if row.description.is_empty() {
        row.label.clone()
    } else {
        format!("{} ({})", row.label, row.description)
    };
    format!(
        "---\nname: {name}\ndescription: General-purpose agent running on {what}, model {id}. Use for work you want done by this model; see the byom skill for when.\nmodel: {id}\n---\n\nYou are a general-purpose agent running on {id}. Complete the task you are given using the available tools, verify your work, and report back concisely: what you did, what you found, and anything uncertain.\n",
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
            "name": "byom",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "Models from other providers in this Claude Code session",
        })
        .to_string(),
    )?;
    write("skills/byom/SKILL.md", SKILL)?;
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
        assert_eq!(agent_type("zai/glm-5.3"), "byom:zai-glm-5-3");
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
}
