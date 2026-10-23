//! Translate neutral native operations into ordinary sandboxed Codex tools.
//! This module generates commands; all execution stays in the host tool runtime.
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_provider_api::HostOperation;
use codex_provider_api::ProviderError;
use codex_provider_api::Result;
use serde_json::Value;
use serde_json::json;

const MARKER: &str = "CODEX_CURSOR_RESULT_V1:";

pub(crate) fn available() -> bool {
    cfg!(target_os = "linux")
        && std::path::Path::new("/bin/bash").is_file()
        && [
            "base64", "find", "head", "mktemp", "od", "rg", "rm", "rmdir", "sed", "tail",
            "timeout", "tr", "wc",
        ]
        .iter()
        .all(|name| which::which(name).is_ok())
}

fn text<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args[name]
        .as_str()
        .filter(|value| !value.contains('\0'))
        .ok_or_else(|| ProviderError::Host(format!("missing or invalid {name}")))
}
fn quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

fn integer(args: &Value, name: &str, default: u64, min: u64, max: u64) -> Result<u64> {
    let Some(value) = args.get(name) else {
        return Ok(default);
    };
    let value = value
        .as_u64()
        .ok_or_else(|| ProviderError::Host(format!("{name} must be a non-negative integer")))?;
    if !(min..=max).contains(&value) {
        return Err(ProviderError::Host(format!(
            "{name} must be between {min} and {max}"
        )));
    }
    Ok(value)
}

fn optional_text<'a>(args: &'a Value, name: &str) -> Result<Option<&'a str>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|value| !value.contains('\0'))
            .map(Some)
            .ok_or_else(|| ProviderError::Host(format!("{name} must be a string without NUL"))),
    }
}

fn bool_option(args: &Value, name: &str) -> Result<bool> {
    match args.get(name) {
        None => Ok(false),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| ProviderError::Host(format!("{name} must be a boolean"))),
    }
}

fn line_count_script() -> &'static str {
    r#"lines=$(wc -l <"$p")
bytes=$(wc -c <"$p")
if [ "$bytes" -gt 0 ] && [ "$(tail -c 1 -- "$p" | od -An -t x1 | tr -d ' \n')" != 0a ]; then lines=$((lines + 1)); fi"#
}

pub(crate) fn command(operation: &HostOperation) -> Result<Value> {
    let args = operation.arguments();
    let script = match operation {
        HostOperation::Shell(_) => {
            let command = quote(text(args, "command")?);
            let seconds = integer(args, "timeout_ms", 120_000, 1, 3_600_000)?.div_ceil(1000);
            // A private temp directory captures streams separately and is cleaned by the shell.
            format!(
                "d=$(mktemp -d) || exit 1\ntrap 'rm -- \"$d/out\" \"$d/err\"; rmdir -- \"$d\"' EXIT\ntimeout --signal=TERM --kill-after=5s {seconds}s /bin/bash -lc {command} >\"$d/out\" 2>\"$d/err\"\ns=$?\nprintf '{MARKER}%s\\n' \"$s\"\nbase64 -w0 <\"$d/out\"\nprintf '\\n'\nbase64 -w0 <\"$d/err\"\nprintf '\\n'"
            )
        }
        HostOperation::ReadFile(_) => {
            let path = quote(text(args, "path")?);
            let start = integer(args, "offset", 1, 1, i32::MAX as u64)?;
            let limit = integer(args, "limit", 2000, 1, 10000)?;
            let end = start.saturating_add(limit - 1);
            format!(
                "p={path}\ntest -f \"$p\" || exit 1\n{}\nprintf '{MARKER}%s %s\\n' \"$lines\" \"$bytes\"\nsed -n '{start},{end}p' -- \"$p\" | head -c 65536 | base64 -w0\nprintf '\\n'",
                line_count_script()
            )
        }
        HostOperation::WriteFile(_) => {
            let path = quote(text(args, "path")?);
            let content = text(args, "content")?;
            if content.len() > 1024 * 1024 {
                return Err(ProviderError::Host("native write exceeds 1 MiB".into()));
            }
            let encoded = STANDARD.encode(content);
            format!(
                "p={path}\nprintf '%s' '{encoded}' | base64 -d >\"$p\" || exit 1\n{}\nprintf '{MARKER}%s %s\\n' \"$lines\" \"$bytes\"",
                line_count_script()
            )
        }
        HostOperation::DeleteFile(_) => {
            let path = quote(text(args, "path")?);
            format!("test -f {path} && rm -- {path} && printf '{MARKER}ok\\n'")
        }
        HostOperation::ListDirectory(_) => {
            let path = quote(text(args, "path")?);
            let ignores = match args.get("ignore") {
                None => Vec::new(),
                Some(Value::Array(values)) => values
                    .iter()
                    .map(|value| {
                        value
                            .as_str()
                            .filter(|s| !s.contains('\0'))
                            .map(quote)
                            .ok_or_else(|| {
                                ProviderError::Host(
                                    "ignore entries must be strings without NUL".into(),
                                )
                            })
                    })
                    .collect::<Result<Vec<_>>>()?,
                _ => {
                    return Err(ProviderError::Host(
                        "ignore must be an array of strings".into(),
                    ));
                }
            };
            let mut predicates = String::new();
            for ignore in ignores {
                predicates.push_str(&format!(" ! -name {ignore}"));
            }
            format!(
                "p={path}\nb=$(cd -- \"$p\" && pwd -P) || exit 1\nprintf '{MARKER}ls\\n%s\\n' \"$b\"\nfind \"$p\" -mindepth 1 -maxdepth 1{predicates} -printf '%y\\t%f\\0' | base64 -w0\nprintf '\\n'"
            )
        }
        HostOperation::Search(_) => {
            let pattern = quote(text(args, "pattern")?);
            let path = quote(optional_text(args, "path")?.unwrap_or("."));
            let glob = optional_text(args, "glob")?
                .map(|value| format!(" --glob {}", quote(value)))
                .unwrap_or_default();
            let kind = optional_text(args, "type")?
                .map(|value| format!(" --type {}", quote(value)))
                .unwrap_or_default();
            let case = if bool_option(args, "case_insensitive")? {
                " --ignore-case"
            } else {
                ""
            };
            let multiline = if bool_option(args, "multiline")? {
                " --multiline"
            } else {
                ""
            };
            let context = args
                .get("context")
                .map(|_| integer(args, "context", 0, 0, i32::MAX as u64))
                .transpose()?;
            let before = args
                .get("context_before")
                .map(|_| integer(args, "context_before", 0, 0, i32::MAX as u64))
                .transpose()?
                .or(context);
            let after = args
                .get("context_after")
                .map(|_| integer(args, "context_after", 0, 0, i32::MAX as u64))
                .transpose()?
                .or(context);
            let before = before
                .map(|value| format!(" --before-context {value}"))
                .unwrap_or_default();
            let after = after
                .map(|value| format!(" --after-context {value}"))
                .unwrap_or_default();
            let sort = match optional_text(args, "sort")? {
                None | Some("path") => " --sort path",
                Some("none") => "",
                Some(other) => {
                    return Err(ProviderError::Unsupported(format!(
                        "native search sort mode {other:?} is unsupported"
                    )));
                }
            };
            let sort_ascending = match args.get("sort_ascending") {
                None => true,
                Some(value) => value.as_bool().ok_or_else(|| {
                    ProviderError::Host("sort_ascending must be a boolean".into())
                })?,
            };
            if sort.is_empty() && !sort_ascending {
                return Err(ProviderError::Unsupported(
                    "sort_ascending requires path sorting".into(),
                ));
            }
            let _ = integer(args, "offset", 0, 0, i32::MAX as u64)?;
            let _ = integer(args, "head_limit", i32::MAX as u64, 0, i32::MAX as u64)?;
            let output_mode = optional_text(args, "output_mode")?.unwrap_or("content");
            if !matches!(output_mode, "content" | "files_with_matches" | "count") {
                return Err(ProviderError::Unsupported(format!(
                    "native search output mode {output_mode:?} is unsupported"
                )));
            }
            format!(
                "printf '{MARKER}grep\\n'\nrg --json{case}{multiline}{before}{after}{glob}{kind}{sort} -- {pattern} {path}\ns=$?\ntest \"$s\" -le 1"
            )
        }
    };
    let mut result = json!({"cmd":script,"shell":"/bin/bash","login":false,"yield_time_ms":1000,"max_output_tokens":30000});
    if let Some(directory) = optional_text(args, "working_directory")? {
        result["workdir"] = json!(directory);
    }
    Ok(result)
}

#[cfg(all(test, feature = "provider-cursor"))]
mod tests {
    use super::MARKER;
    use codex_provider_api::NativeOperations;
    use codex_provider_api::ToolCall;
    use codex_provider_cursor::operations::CursorOperations;
    fn command(name: &str, args: &Value) -> codex_provider_api::Result<Value> {
        let call = ToolCall {
            id: "test".into(),
            name: name.into(),
            arguments: args.clone(),
        };
        super::command(&CursorOperations.operation(&call)?)
    }
    fn result(name: &str, args: &Value, structured: &Value) -> codex_provider_api::Result<Value> {
        CursorOperations.result(
            &ToolCall {
                id: "test".into(),
                name: name.into(),
                arguments: args.clone(),
            },
            structured,
        )
    }
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::Value;
    use serde_json::json;
    use std::process::Command;

    fn run_generated(
        name: &str,
        args: &Value,
        cwd: &std::path::Path,
    ) -> codex_provider_api::Result<Value> {
        let generated = command(name, args)?;
        let script = generated["cmd"].as_str().ok_or_else(|| {
            codex_provider_api::ProviderError::Host("fixture command has no script".into())
        })?;
        let output = Command::new("/bin/bash")
            .arg("-c")
            .arg(script)
            .current_dir(cwd)
            .output()
            .map_err(|error| codex_provider_api::ProviderError::Host(error.to_string()))?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout)
            .map_err(|error| codex_provider_api::ProviderError::Host(error.to_string()))?;
        Ok(json!({"exit_code":output.status.code(),"output":text}))
    }

    #[test]
    fn command_shell_quotes_arguments_and_validates_options() {
        let generated = command(
            "cursor_grep",
            &json!({
                "pattern":"a'b", "path":"src files", "glob":"*.rs", "case_insensitive":true,
                "context":2, "context_before":3, "context_after":4, "type":"rust",
                "head_limit":8, "multiline":true, "sort":"path", "offset":1
            }),
        )
        .expect("grep command");
        let script = generated["cmd"].as_str().unwrap();
        assert!(script.contains("'a'\"'\"'b'"));
        assert!(script.contains("--glob '*.rs'"));
        assert!(script.contains("--ignore-case"));
        assert!(script.contains("--multiline"));
        assert!(script.contains("--before-context 3"));
        assert!(script.contains("--after-context 4"));
        assert!(script.contains("--type 'rust'"));
        assert!(script.contains("--sort path"));
        assert!(command("cursor_grep", &json!({"pattern":"x", "sort":"modified"})).is_err());
        assert!(command("cursor_grep", &json!({"pattern":"x", "offset":-1})).is_err());
        assert!(command("cursor_read_file", &json!({"path":"bad\0path"})).is_err());
    }

    #[test]
    fn read_and_write_commands_count_unterminated_final_lines_and_bound_utf8() {
        let read = command(
            "cursor_read_file",
            &json!({"path":"notes.txt", "offset":1, "limit":3}),
        )
        .unwrap();
        let script = read["cmd"].as_str().unwrap();
        assert!(script.contains("lines=$((lines + 1))"));
        assert!(script.contains("head -c 65536 | base64 -w0"));
        let write = command(
            "cursor_write_file",
            &json!({"path":"notes.txt", "content":"end"}),
        )
        .unwrap();
        assert!(write["cmd"].as_str().unwrap().contains("tail -c 1"));
        assert!(command("cursor_read_file", &json!({"path":"x", "limit":0})).is_err());
    }

    #[test]
    fn ls_ignores_requested_names_and_returns_absolute_directory_paths() {
        let generated = command(
            "cursor_ls",
            &json!({"path":".", "ignore":[".git", "target"]}),
        )
        .unwrap();
        let script = generated["cmd"].as_str().unwrap();
        assert!(script.contains("pwd -P"));
        assert!(script.contains("! -name '.git' ! -name 'target'"));

        let entries = STANDARD.encode("d\tchild\0f\tfile.txt\0");
        let structured = json!({"output":format!("{MARKER}ls\n/work/tree\n{entries}\n")});
        let parsed = result("cursor_ls", &json!({"path":"."}), &structured).unwrap();
        assert_eq!(
            parsed,
            json!({"files":["file.txt"],"directories":["/work/tree/child"]})
        );
        assert!(command("cursor_ls", &json!({"path":".", "ignore":".git"})).is_err());
    }

    #[test]
    fn grep_parses_context_paginates_and_rejects_unknown_modes() {
        let records = [
            json!({"type":"match","data":{"path":{"text":"b.rs"},"line_number":2,"lines":{"text":"hit\n"}}}),
            json!({"type":"context","data":{"path":{"text":"b.rs"},"line_number":3,"lines":{"text":"after\n"}}}),
            json!({"type":"match","data":{"path":{"text":"a.rs"},"line_number":1,"lines":{"text":"other\n"}}}),
        ].iter().map(Value::to_string).collect::<Vec<_>>().join("\n");
        let structured = json!({"output":format!("{MARKER}grep\n{records}\n")});
        let parsed = result(
            "cursor_grep",
            &json!({"offset":1,"head_limit":1,"sort":"none"}),
            &structured,
        )
        .unwrap();
        assert_eq!(
            parsed,
            json!({"matches":[{"file":"b.rs","lines":[{"line_number":3,"content":"after\n","is_context_line":true}]}]})
        );
        let counted = result(
            "cursor_grep",
            &json!({"output_mode":"count", "sort":"none"}),
            &structured,
        )
        .unwrap();
        assert_eq!(counted["counts"][0]["count"], 1);

        let descending = result(
            "cursor_grep",
            &json!({"sort":"path", "sort_ascending":false}),
            &structured,
        )
        .unwrap();
        assert_eq!(descending["matches"][0]["file"], "b.rs");
        assert!(command("cursor_grep", &json!({"pattern":"x", "output_mode":"bad"})).is_err());
    }

    #[test]
    fn generated_read_and_write_commands_work_on_quoted_temp_paths() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("file'quoted.txt");
        std::fs::write(&path, b"one\nlast").expect("fixture");
        let args = json!({"path":path.to_string_lossy(), "offset":1, "limit":10});
        let structured = run_generated("cursor_read_file", &args, temp.path())
            .expect("run generated fixture command");
        let parsed = result("cursor_read_file", &args, &structured).expect("read result");
        assert_eq!(parsed["content"], "one\nlast");
        assert_eq!(parsed["total_lines"], 2);
        assert_eq!(parsed["file_size"], 8);

        let content = "written\nlast without newline";
        let write_args = json!({"path":path.to_string_lossy(), "content":content});
        let structured = run_generated("cursor_write_file", &write_args, temp.path())
            .expect("run generated fixture command");
        let parsed = result("cursor_write_file", &write_args, &structured).expect("write result");
        assert_eq!(
            std::fs::read_to_string(path).expect("written fixture"),
            content
        );
        assert_eq!(parsed["lines_created"], 2);
    }

    #[test]
    fn generated_read_handles_utf8_cap_and_rejects_binary_fixture() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("unicode.txt");
        let mut bytes = vec![b'a'; 65_535];
        bytes.extend_from_slice("é".as_bytes());
        std::fs::write(&path, &bytes).expect("unicode fixture");
        let args = json!({"path":path.to_string_lossy()});
        let structured = run_generated("cursor_read_file", &args, temp.path())
            .expect("run generated fixture command");
        let parsed = result("cursor_read_file", &args, &structured).expect("bounded utf8 read");
        assert_eq!(parsed["content"].as_str().unwrap().len(), 65_535);
        assert_eq!(parsed["truncated"], true);

        std::fs::write(&path, [b'a', 0xff, b'b']).expect("binary fixture");
        let structured = run_generated("cursor_read_file", &args, temp.path())
            .expect("run generated fixture command");
        assert!(result("cursor_read_file", &args, &structured).is_err());
    }

    #[test]
    fn generated_grep_and_ls_apply_options_on_temp_fixtures() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("one.rs");
        std::fs::write(&source, "before\nNeedle\nafter\n").expect("source fixture");
        std::fs::write(temp.path().join("ignored.txt"), "needle\n").expect("other fixture");
        let grep_args = json!({
            "pattern":"needle", "path":temp.path().to_string_lossy(), "glob":"*.rs",
            "context_before":1, "context_after":1, "case_insensitive":true, "type":"rust",
            "head_limit":10, "multiline":false, "sort":"path", "output_mode":"content"
        });
        let structured = run_generated("cursor_grep", &grep_args, temp.path())
            .expect("run generated fixture command");
        let parsed = result("cursor_grep", &grep_args, &structured).expect("grep result");
        assert_eq!(
            parsed["matches"][0]["file"],
            source.to_string_lossy().as_ref()
        );
        let rows = parsed["matches"][0]["lines"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[1]["is_context_line"], false);

        let count_args = json!({"pattern":"needle", "path":temp.path().to_string_lossy(), "glob":"*.rs", "context":1, "case_insensitive":true, "type":"rust", "output_mode":"count"});
        let structured = run_generated("cursor_grep", &count_args, temp.path())
            .expect("run generated fixture command");
        let parsed = result("cursor_grep", &count_args, &structured).expect("count result");
        assert_eq!(parsed["counts"][0]["count"], 1);

        std::fs::create_dir(temp.path().join("child")).expect("child directory");
        let ls_args = json!({"path":temp.path().to_string_lossy(), "ignore":["ignored.txt"]});
        let structured = run_generated("cursor_ls", &ls_args, temp.path())
            .expect("run generated fixture command");
        let parsed = result("cursor_ls", &ls_args, &structured).expect("ls result");
        assert!(
            !parsed["files"]
                .as_array()
                .unwrap()
                .contains(&json!("ignored.txt"))
        );
        assert!(parsed["directories"][0].as_str().unwrap().starts_with('/'));
    }
}
