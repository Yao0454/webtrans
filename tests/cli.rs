use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;

const HTML: &str = r#"<!doctype html>
<html lang="en"><head><title>Welcome</title>
<style>.card::before { content: "Hello"; }</style>
<script>const message = "Hello"; const tag = "<p>JS text</p>";</script>
</head><body><p>Hello</p><p>Hello</p>
<button title="Open menu">Open</button></body></html>
"#;

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_webtrans"))
        .args(args)
        .current_dir(dir)
        .env_remove("OPENAI_API_KEY")
        .env_remove("WEBTRANS_API_KEY")
        .env_remove("WEBTRANS_MODEL")
        .env_remove("WEBTRANS_BASE_URL")
        .output()
        .expect("run webtrans")
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fixture(html: &str) -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input.html"), html).unwrap();
    success(&run(
        dir.path(),
        &["extract", "input.html", "-o", "table.json"],
    ));
    dir
}

fn table(dir: &Path) -> Value {
    serde_json::from_slice(&fs::read(dir.join("table.json")).unwrap()).unwrap()
}

fn save_table(dir: &Path, table: &Value) {
    fs::write(
        dir.join("table.json"),
        serde_json::to_vec_pretty(table).unwrap(),
    )
    .unwrap();
}

fn translate_all(table: &mut Value, translation: impl Fn(&str) -> String) {
    for entry in table["entries"].as_object_mut().unwrap().values_mut() {
        let translated = translation(entry["source"].as_str().unwrap());
        entry["translation"] = Value::String(translated);
    }
}

#[test]
fn help_exposes_pipeline_commands() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(dir.path(), &["--help"]);
    success(&output);
    let help = String::from_utf8(output.stdout).unwrap();
    for command in ["extract", "translate", "apply", "run"] {
        assert!(help.contains(command), "missing command {command}");
        success(&run(dir.path(), &[command, "--help"]));
    }
}

#[test]
fn terminal_logs_show_real_table_schema_and_verbose_expands_the_preview() {
    let html: String = (1..=7).map(|n| format!("<p>Entry {n}</p>")).collect();
    let dir = fixture(&html);
    let preview = run(
        dir.path(),
        &["extract", "input.html", "-o", "table.json", "--force"],
    );
    success(&preview);
    let log = String::from_utf8(preview.stderr).unwrap();
    for field in [
        "source_sha256",
        "entries",
        "source",
        "translation",
        "context",
        "start",
        "end",
        "placeholders",
    ] {
        assert!(
            log.contains(&format!("\"{field}\"")),
            "missing logged field {field}"
        );
    }
    assert!(log.contains("u000005"));
    assert!(!log.contains("u000006"));
    assert!(log.contains("2 entries omitted"));
    assert_eq!(table(dir.path())["entries"].as_object().unwrap().len(), 7);

    let full = run(
        dir.path(),
        &[
            "extract",
            "input.html",
            "-o",
            "table.json",
            "--force",
            "--verbose",
        ],
    );
    success(&full);
    let log = String::from_utf8(full.stderr).unwrap();
    assert!(log.contains("u000007"));
    assert!(!log.contains("entries omitted"));
}

#[test]
fn extract_and_apply_preserve_code_and_distinguish_repeated_text() {
    let dir = fixture(HTML);
    let mut table = table(dir.path());
    assert_eq!(table["version"], 1);
    assert_eq!(table["source_sha256"].as_str().unwrap().len(), 64);
    let entries = table["entries"].as_object_mut().unwrap();
    let hello_ids: Vec<String> = entries
        .iter()
        .filter(|(_, entry)| entry["source"] == "Hello")
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(hello_ids.len(), 2, "occurrences need separate IDs");
    for entry in entries.values() {
        let source = entry["source"].as_str().unwrap();
        assert!(!source.contains("const message"));
        assert!(!source.contains("content:"));
        assert!(!source.contains("JS text"));
    }
    translate_all(&mut table, |s| match s {
        "Welcome" => "欢迎".into(),
        "Open menu" => "打开菜单".into(),
        "Open" => "打开".into(),
        _ => s.to_owned(),
    });
    table["entries"][&hello_ids[0]]["translation"] = Value::String("你好".into());
    table["entries"][&hello_ids[1]]["translation"] = Value::String("您好".into());
    save_table(dir.path(), &table);
    success(&run(
        dir.path(),
        &["apply", "input.html", "table.json", "-o", "output.html"],
    ));
    let output = fs::read_to_string(dir.path().join("output.html")).unwrap();
    assert!(output.contains("<p>你好</p>"));
    assert!(output.contains("<p>您好</p>"));
    assert!(output.contains("title=\"打开菜单\""));
    assert!(output.contains(r#"<style>.card::before { content: "Hello"; }</style>"#));
    assert!(
        output
            .contains(r#"<script>const message = "Hello"; const tag = "<p>JS text</p>";</script>"#)
    );
    assert!(output.contains("<html lang=\"en\">"));
    assert_eq!(
        fs::read_to_string(dir.path().join("input.html")).unwrap(),
        HTML
    );
}

#[test]
fn unchanged_translations_round_trip_exact_source_bytes() {
    let html = "<!DOCTYPE html>\r\n<HTML><body>\n<p class='x'>  Fish &amp; Chips\t</p>\n<p>Hello <strong data-x=1>world</strong>!</p>\n</body></HTML>\n";
    let dir = fixture(html);
    let mut table = table(dir.path());
    translate_all(&mut table, str::to_owned);
    save_table(dir.path(), &table);
    success(&run(
        dir.path(),
        &["apply", "input.html", "table.json", "-o", "output.html"],
    ));
    assert_eq!(
        fs::read(dir.path().join("output.html")).unwrap(),
        html.as_bytes()
    );
}

#[test]
fn inline_markup_survives_translation() {
    let dir = fixture("<p>Hello <strong class='x'>world</strong>!</p>");
    let mut table = table(dir.path());
    let has_inline_markers = table["entries"]
        .as_object()
        .unwrap()
        .values()
        .any(|entry| !entry["placeholders"].as_object().unwrap().is_empty());
    assert!(
        has_inline_markers,
        "inline tags need protected placeholders"
    );
    translate_all(&mut table, |s| {
        s.replace("Hello", "你好").replace("world", "世界")
    });
    save_table(dir.path(), &table);
    success(&run(
        dir.path(),
        &["apply", "input.html", "table.json", "-o", "output.html"],
    ));
    assert_eq!(
        fs::read_to_string(dir.path().join("output.html")).unwrap(),
        "<p>你好 <strong class='x'>世界</strong>!</p>"
    );
}

#[test]
fn missing_translations_fail_without_creating_output() {
    let dir = fixture("<p>Hello</p><p>World</p>");
    let output = run(
        dir.path(),
        &["apply", "input.html", "table.json", "-o", "output.html"],
    );
    assert!(!output.status.success());
    assert!(!dir.path().join("output.html").exists());
}

#[test]
fn allow_missing_preserves_untranslated_occurrences() {
    let dir = fixture("<p>Hello</p><p>World</p>");
    let mut table = table(dir.path());
    for entry in table["entries"].as_object_mut().unwrap().values_mut() {
        if entry["source"] == "Hello" {
            entry["translation"] = Value::String("你好".into());
        }
    }
    save_table(dir.path(), &table);
    success(&run(
        dir.path(),
        &[
            "apply",
            "input.html",
            "table.json",
            "-o",
            "output.html",
            "--allow-missing",
        ],
    ));
    assert_eq!(
        fs::read_to_string(dir.path().join("output.html")).unwrap(),
        "<p>你好</p><p>World</p>"
    );
}

#[test]
fn changed_source_is_rejected_even_if_the_text_still_exists() {
    let dir = fixture("<p>Hello</p>");
    let mut table = table(dir.path());
    translate_all(&mut table, |_| "你好".into());
    save_table(dir.path(), &table);
    fs::write(dir.path().join("input.html"), "<!-- edited --><p>Hello</p>").unwrap();
    let output = run(
        dir.path(),
        &["apply", "input.html", "table.json", "-o", "output.html"],
    );
    assert!(!output.status.success());
    assert!(!dir.path().join("output.html").exists());
}

#[test]
fn altered_metadata_is_rejected_before_writing_output() {
    let dir = fixture("<p>Hello</p>");
    let mut table = table(dir.path());
    translate_all(&mut table, |_| "你好".into());
    let entry = table["entries"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    entry["start"] = Value::from(0);
    save_table(dir.path(), &table);
    let output = run(
        dir.path(),
        &["apply", "input.html", "table.json", "-o", "output.html"],
    );
    assert!(!output.status.success());
    assert!(!dir.path().join("output.html").exists());
}

#[test]
fn existing_output_requires_force() {
    let dir = fixture("<p>Hello</p>");
    let original_table = fs::read(dir.path().join("table.json")).unwrap();
    let output = run(dir.path(), &["extract", "input.html", "-o", "table.json"]);
    assert!(!output.status.success());
    assert_eq!(
        fs::read(dir.path().join("table.json")).unwrap(),
        original_table
    );
    success(&run(
        dir.path(),
        &["extract", "input.html", "-o", "table.json", "--force"],
    ));

    let mut table = table(dir.path());
    translate_all(&mut table, |_| "你好".into());
    save_table(dir.path(), &table);
    fs::write(dir.path().join("output.html"), "existing user content").unwrap();
    let output = run(
        dir.path(),
        &["apply", "input.html", "table.json", "-o", "output.html"],
    );
    assert!(!output.status.success());
    assert_eq!(
        fs::read_to_string(dir.path().join("output.html")).unwrap(),
        "existing user content"
    );
    success(&run(
        dir.path(),
        &[
            "apply",
            "input.html",
            "table.json",
            "-o",
            "output.html",
            "--force",
        ],
    ));
    assert_eq!(
        fs::read_to_string(dir.path().join("output.html")).unwrap(),
        "<p>你好</p>"
    );
}

#[test]
fn run_checkpoints_batches_and_resume_translates_only_unfinished_entries() {
    run_checkpoint_scenario(false, false);
}

#[test]
fn run_reads_config_json_without_api_flags_and_file_overrides_environment() {
    run_checkpoint_scenario(true, false);
}

#[test]
fn explicit_config_path_and_cli_overrides_take_precedence() {
    run_checkpoint_scenario(true, true);
}

fn run_checkpoint_scenario(use_config: bool, cli_override: bool) {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input.html"), "<p>Hello</p><p>World</p>").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let server = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut requested_ids = Vec::new();
        for status in [200, 500, 200] {
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "mock API timed out");
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => panic!("mock API accept failed: {error}"),
                }
            };
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let (body_start, content_length) = loop {
                let mut buffer = [0; 4096];
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "request ended before HTTP headers");
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
                    let header = std::str::from_utf8(&bytes[..index]).unwrap();
                    assert!(header.starts_with("POST /v1/chat/completions HTTP/1.1"));
                    assert!(header.lines().any(|line| {
                        line.eq_ignore_ascii_case("authorization: Bearer local-test-key")
                    }));
                    let content_length: usize = header
                        .lines()
                        .filter_map(|line| line.split_once(':'))
                        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                        .expect("request content-length")
                        .1
                        .trim()
                        .parse()
                        .unwrap();
                    break (index + 4, content_length);
                }
            };
            while bytes.len() < body_start + content_length {
                let mut buffer = [0; 4096];
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0, "request ended before complete JSON body");
                bytes.extend_from_slice(&buffer[..read]);
            }
            let request: Value =
                serde_json::from_slice(&bytes[body_start..body_start + content_length]).unwrap();
            assert_eq!(request["model"], "mock");
            let user_data: Value = serde_json::from_str(
                request["messages"][1]["content"]
                    .as_str()
                    .expect("user JSON"),
            )
            .unwrap();
            let entries = user_data["entries"].as_object().unwrap();
            assert_eq!(entries.len(), 1, "one entry per requested batch");
            let (id, entry) = entries.iter().next().unwrap();
            requested_ids.push(id.clone());
            let translated = match entry["source"].as_str().unwrap() {
                "Hello" => "你好",
                "World" => "世界",
                source => panic!("unexpected source {source}"),
            };
            let response = if status == 200 {
                let translations = json!({ id: translated });
                json!({"choices": [{"finish_reason": "stop", "message": {
                    "role": "assistant", "content": translations.to_string()
                }}]})
            } else {
                json!({"error": {"message": "temporary mock error"}})
            };
            let body = response.to_string();
            write!(stream, "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                if status == 200 { "OK" } else { "Internal Server Error" }, body.len()).unwrap();
            stream.flush().unwrap();
        }
        requested_ids
    });

    if use_config {
        let config = if cli_override {
            json!({"base_url": "http://127.0.0.1:1/wrong", "api_key": "unused-file-key", "model": "unused-file-model"})
        } else {
            json!({"base_url": base_url, "api_key": "local-test-key", "model": "mock"})
        };
        let name = if cli_override {
            "provider.json"
        } else {
            "config.json"
        };
        fs::write(dir.path().join(name), serde_json::to_vec(&config).unwrap()).unwrap();
        if cli_override {
            // Explicit --config must not also try to parse the automatic file.
            fs::write(dir.path().join("config.json"), "invalid automatic config").unwrap();
        }
    }

    let invoke = |resume: bool| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_webtrans"));
        command
            .current_dir(dir.path())
            .env("WEBTRANS_CLI_TEST_KEY", "local-test-key")
            .env("OPENAI_API_KEY", "unused-environment-key")
            .env("WEBTRANS_MODEL", "unused-environment-model")
            .env("WEBTRANS_BASE_URL", "http://127.0.0.1:1/wrong")
            .args([
                "run",
                "input.html",
                "-o",
                "translated.html",
                "--to",
                "zh-CN",
                "--batch-size",
                "1",
                "--retries",
                "0",
                "--timeout",
                "5",
            ]);
        if !use_config || cli_override {
            command.args([
                "--model",
                "mock",
                "--base-url",
                &base_url,
                "--api-key-env",
                "WEBTRANS_CLI_TEST_KEY",
            ]);
        }
        if cli_override {
            command.args(["--config", "provider.json"]);
        }
        if resume {
            command.arg("--resume");
        }
        command.output().unwrap()
    };
    let failed = invoke(false);
    assert!(!failed.status.success());
    let log = String::from_utf8_lossy(&failed.stderr);
    assert!(log.contains("[api / 发送翻译]"));
    assert!(log.contains("batch-saved"));
    assert!(log.contains("你好"));
    assert!(!log.contains("local-test-key"));
    assert!(!log.contains("unused-file-key"));
    assert!(!log.contains("unused-environment-key"));
    if use_config {
        assert!(log.contains("[config / 配置]"));
    }
    assert!(!dir.path().join("translated.html").exists());
    let checkpoint_path = dir.path().join("translated.translations.json");
    let checkpoint: Value = serde_json::from_slice(&fs::read(&checkpoint_path).unwrap()).unwrap();
    assert!(!checkpoint.to_string().contains("local-test-key"));
    assert_eq!(checkpoint["target_language"], "zh-CN");
    assert_eq!(
        checkpoint["entries"]
            .as_object()
            .unwrap()
            .values()
            .filter(|entry| entry["translation"].is_string())
            .count(),
        1
    );

    let resumed = invoke(true);
    success(&resumed);
    let log = String::from_utf8_lossy(&resumed.stderr);
    assert!(log.contains("世界"));
    assert!(log.contains("[apply / 回填]"));
    assert!(!log.contains("local-test-key"));
    let requested_ids = server.join().unwrap();
    assert_eq!(requested_ids.len(), 3);
    assert_ne!(requested_ids[0], requested_ids[1]);
    assert_eq!(
        requested_ids[1], requested_ids[2],
        "only failed batch should be retried"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("translated.html")).unwrap(),
        "<p>你好</p><p>世界</p>"
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("input.html")).unwrap(),
        "<p>Hello</p><p>World</p>"
    );
}

#[test]
fn config_errors_never_echo_secret_values_or_create_outputs() {
    let secret = "config-secret-never-print-this";
    let cases = [
        format!(r#"{{"api_key":"{secret}", "model":}}"#),
        format!(r#"{{"api_key":"{secret}", "model":"mock", "{secret}":true}}"#),
        format!(r#"{{"api_key":{{"{secret}":true}}, "model":"mock"}}"#),
    ];
    for config in cases {
        let dir = fixture("<p>Hello</p>");
        fs::write(dir.path().join("config.json"), config).unwrap();
        let output = run(
            dir.path(),
            &["translate", "table.json", "-o", "out.json", "--to", "zh-CN"],
        );
        assert!(!output.status.success());
        let log = String::from_utf8_lossy(&output.stderr);
        assert!(log.contains("Invalid API config"));
        assert!(!log.contains(secret));
        assert!(!dir.path().join("out.json").exists());
    }
}

#[test]
fn explicit_missing_config_is_an_error_and_empty_key_is_actionable() {
    let dir = fixture("<p>Hello</p>");
    fs::write(
        dir.path().join("config.json"),
        r#"{"model":"mock","api_key":""}"#,
    )
    .unwrap();
    let missing = run(
        dir.path(),
        &[
            "translate",
            "table.json",
            "-o",
            "out.json",
            "--to",
            "zh-CN",
            "--config",
            "missing.json",
        ],
    );
    assert!(!missing.status.success());
    assert!(
        String::from_utf8_lossy(&missing.stderr).contains("Cannot read API config missing.json")
    );
    let empty = run(
        dir.path(),
        &["translate", "table.json", "-o", "out.json", "--to", "zh-CN"],
    );
    assert!(!empty.status.success());
    assert!(String::from_utf8_lossy(&empty.stderr).contains("fill api_key in config.json"));
    assert!(!dir.path().join("out.json").exists());
}

#[test]
fn outputs_cannot_overwrite_config_even_with_force() {
    let dir = fixture("<p>Hello</p>");
    let config =
        r#"{"base_url":"http://127.0.0.1:1/v1","api_key":"local-test-key","model":"mock"}"#;
    fs::write(dir.path().join("config.json"), config).unwrap();
    for args in [
        vec![
            "run",
            "input.html",
            "-o",
            "config.json",
            "--to",
            "zh-CN",
            "--force",
        ],
        vec![
            "run",
            "input.html",
            "-o",
            "out.html",
            "--table",
            "config.json",
            "--to",
            "zh-CN",
            "--force",
        ],
        vec![
            "translate",
            "table.json",
            "-o",
            "config.json",
            "--to",
            "zh-CN",
            "--force",
        ],
    ] {
        let output = run(dir.path(), &args);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("Output must not overwrite the API config file")
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("config.json")).unwrap(),
            config
        );
    }
}
