#![cfg(unix)]

use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const BIN: &str = env!("CARGO_BIN_EXE_byoclaude");

/// A port free at startup, shared by every launcher in this test binary.
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

fn command(home: &std::path::Path, bin: &std::path::Path) -> Command {
    let mut command = Command::new(BIN);
    command
        .env_clear()
        .env("BYOCLAUDE_HOME", home)
        .env("HOME", home)
        .env("PATH", bin)
        .env("BYOCLAUDE_PORT", port().to_string());
    command
}

#[test]
fn help_and_unconfigured_status_are_local_and_safe() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("absent");
    let output = command(&home, temp.path()).arg("--help").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("run"));
    let output = command(&home, temp.path()).arg("status").output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Bridge is not running"));
    assert!(!home.exists());
}

// All shared-port scenarios live in one test so they cannot race each other.
#[tokio::test]
async fn launcher_process_contract() {
    const PORT: fn() -> u16 = port;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("state");
    let bin = temp.path().join("bin");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&bin).unwrap();
    let fake = bin.join("claude");
    fs::write(&fake, r##"#!/bin/sh
printf 'arg=<%s>\n' "$@"
printf 'bedrock=%s\nvertex=%s\nfoundry=%s\nopenai=%s\neditor=%s\npath=%s\n' "${CLAUDE_CODE_USE_BEDROCK-unset}" "${CLAUDE_CODE_USE_VERTEX-unset}" "${CLAUDE_CODE_USE_FOUNDRY-unset}" "${OPENAI_API_KEY-unset}" "$EDITOR" "$PATH"
printf 'base=%s\ntoken=%s\nmodel=%s\nopus=%s\nsonnet=%s\nhaiku=%s\ntraffic=%s\napi=%s\n' "$ANTHROPIC_BASE_URL" "$ANTHROPIC_AUTH_TOKEN" "$ANTHROPIC_MODEL" "$ANTHROPIC_DEFAULT_OPUS_MODEL" "$ANTHROPIC_DEFAULT_SONNET_MODEL" "$ANTHROPIC_DEFAULT_HAIKU_MODEL" "$CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC" "${ANTHROPIC_API_KEY-unset}"
"##).unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).unwrap();
    let catalog = r#"[{"slug":"configured-model","display_name":"","description":"","context_window":0,"effort_levels":[],"default_effort":null,"listed":true}]"#;
    fs::write(home.join("models.json"), catalog).unwrap();
    let upstream = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    upstream.set_nonblocking(true).unwrap();
    fs::write(
        home.join("config.json"),
        serde_json::json!({
            "model": "configured-model",
            "upstream_base_url": format!("http://{}/v1", upstream.local_addr().unwrap())
        })
        .to_string(),
    )
    .unwrap();

    // Start the bridge ourselves, never a detached launcher-owned daemon.
    let mut bridge = OwnedChild(
        command(&home, &bin)
            .args(["bridge", "--port", &PORT().to_string()])
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
    let url = format!("http://127.0.0.1:{}/health", PORT());
    let deadline = Instant::now() + Duration::from_secs(5);
    let key = loop {
        assert!(
            bridge.0.try_wait().unwrap().is_none(),
            "owned bridge exited before readiness"
        );
        if let Ok(key) = fs::read_to_string(home.join("bridge.key"))
            && let Ok(response) = client.get(&url).bearer_auth(key.trim()).send().await
            && response.status() == 200
        {
            break key.trim().to_owned();
        }
        assert!(
            Instant::now() < deadline,
            "owned bridge readiness timed out"
        );
        tokio::time::sleep(Duration::from_millis(30)).await;
    };
    assert_eq!(client.get(&url).send().await.unwrap().status(), 401);
    assert_eq!(
        client
            .get(&url)
            .bearer_auth("wrong-key")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    let status = command(&home, &bin).arg("status").output().unwrap();
    assert!(status.status.success(), "{status:?}");
    assert!(String::from_utf8_lossy(&status.stdout).contains("ready"));

    let mut runs = Vec::new();
    for model in [None, Some("model-one"), Some("model-two")] {
        let mut launch = command(&home, &bin);
        launch.arg("run");
        if let Some(model) = model {
            launch.arg(model);
        }
        launch
            .args(["--", "--print", "two words", "", "--model=forwarded"])
            .env("ANTHROPIC_API_KEY", "must-be-removed")
            .env("ANTHROPIC_MODEL", "must-be-overridden")
            .env("CLAUDE_CODE_USE_BEDROCK", "1")
            .env("CLAUDE_CODE_USE_VERTEX", "1")
            .env("CLAUDE_CODE_USE_FOUNDRY", "1")
            .env("OPENAI_API_KEY", "synthetic-unused")
            .env("EDITOR", "synthetic-editor")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        runs.push((
            model.unwrap_or("configured-model"),
            OwnedChild(launch.spawn().unwrap()),
        ));
    }
    for (model, mut child) in runs {
        // wait_with_output consumes Child; keep ownership guarded until it exits.
        let deadline = Instant::now() + Duration::from_secs(5);
        while child.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "fake Claude timed out");
            std::thread::sleep(Duration::from_millis(20));
        }
        use std::io::Read;
        let mut stdout = String::new();
        child
            .0
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        assert!(child.0.wait().unwrap().success());
        let mut lines = stdout.lines();
        assert_eq!(lines.next(), Some("arg=<--settings>"), "{stdout}");
        let settings = lines.next().unwrap();
        assert!(
            settings.contains("modelPicker") && settings.contains(model),
            "{settings}"
        );
        assert!(
            stdout.contains("arg=<--print>\narg=<two words>\narg=<>\narg=<--model=forwarded>\n"),
            "{stdout}"
        );
        for line in [
            format!("base=http://127.0.0.1:{}", PORT()),
            format!("token={key}"),
            format!("model={model}"),
            format!("opus={model}"),
            format!("sonnet={model}"),
            format!("haiku={model}"),
            "traffic=1".into(),
            "api=unset".into(),
            "bedrock=unset".into(),
            "vertex=unset".into(),
            "foundry=unset".into(),
            "openai=unset".into(),
            "editor=synthetic-editor".into(),
            format!("path={}", bin.display()),
        ] {
            assert!(
                stdout.lines().any(|actual| actual == line),
                "missing {line:?}: {stdout}"
            );
        }
    }
    let wrong_home = temp.path().join("wrong-state");
    fs::create_dir(&wrong_home).unwrap();
    fs::write(wrong_home.join("bridge.key"), "b".repeat(64)).unwrap();
    fs::write(wrong_home.join("models.json"), catalog).unwrap();
    fs::set_permissions(
        wrong_home.join("bridge.key"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let status = command(&wrong_home, &bin).arg("status").output().unwrap();
    assert!(!status.status.success());
    let rejected = command(&wrong_home, &bin)
        .args(["run", "rejected-model"])
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("refusing to launch Claude"));
    assert!(
        rejected.stdout.is_empty(),
        "fake Claude must not run without authenticated readiness"
    );
    assert_eq!(
        upstream.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "launch/readiness must not contact the configured upstream"
    );
}
