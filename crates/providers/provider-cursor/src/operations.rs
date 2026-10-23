//! Cursor protocol names and sandboxed host-operation result mapping.
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_provider_api::HostOperation;
use codex_provider_api::NativeOperations;
use codex_provider_api::ProviderError;
use codex_provider_api::Result;
use codex_provider_api::ToolCall;
use codex_provider_api::ToolDefinition;
use serde_json::Value;
use serde_json::json;

const MARKER: &str = "CODEX_CURSOR_RESULT_V1:";

#[derive(Default)]
pub struct CursorOperations;

impl NativeOperations for CursorOperations {
    fn definitions(&self) -> Vec<ToolDefinition> {
        definitions()
    }
    fn operation(&self, call: &ToolCall) -> Result<HostOperation> {
        let args = call.arguments.clone();
        match call.name.as_str() {
            "cursor_shell" => Ok(HostOperation::Shell(args)),
            "cursor_read_file" => Ok(HostOperation::ReadFile(args)),
            "cursor_write_file" => Ok(HostOperation::WriteFile(args)),
            "cursor_delete_file" => Ok(HostOperation::DeleteFile(args)),
            "cursor_ls" => Ok(HostOperation::ListDirectory(args)),
            "cursor_grep" => {
                // Preserve Cursor's protocol diagnostics before entering the
                // provider-neutral host command translator.
                if let Some(sort) = args.get("sort").and_then(Value::as_str)
                    && !sort.contains('\0')
                    && !matches!(sort, "path" | "none")
                {
                    return Err(ProviderError::Unsupported(format!(
                        "Cursor grep sort mode {sort:?} is unsupported"
                    )));
                }
                if let Some(mode) = args.get("output_mode").and_then(Value::as_str)
                    && !mode.contains('\0')
                    && !matches!(mode, "content" | "files_with_matches" | "count")
                {
                    return Err(ProviderError::Unsupported(format!(
                        "Cursor grep output mode {mode:?} is unsupported"
                    )));
                }
                Ok(HostOperation::Search(args))
            }
            _ => Err(ProviderError::Unsupported(
                "unknown Cursor operation".into(),
            )),
        }
    }
    fn result(&self, call: &ToolCall, structured: &Value) -> Result<Value> {
        result(&call.name, &call.arguments, structured)
    }
}

fn definitions() -> Vec<ToolDefinition> {
    [
        ("cursor_shell", "Run a shell command under Codex permissions", json!({"command":{"type":"string"},"working_directory":{"type":"string"},"timeout_ms":{"type":"integer"}}), vec!["command"]),
        ("cursor_read_file", "Read a file under Codex permissions", json!({"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}}), vec!["path"]),
        ("cursor_write_file", "Write a file under Codex permissions", json!({"path":{"type":"string"},"content":{"type":"string"}}), vec!["path","content"]),
        ("cursor_delete_file", "Delete a file under Codex permissions", json!({"path":{"type":"string"}}), vec!["path"]),
        ("cursor_ls", "List a directory under Codex permissions", json!({"path":{"type":"string"},"ignore":{"type":"array","items":{"type":"string"}}}), vec!["path"]),
        ("cursor_grep", "Search files under Codex permissions", json!({"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"context":{"type":"integer"},"context_before":{"type":"integer"},"context_after":{"type":"integer"},"case_insensitive":{"type":"boolean"},"type":{"type":"string"},"head_limit":{"type":"integer"},"multiline":{"type":"boolean"},"sort":{"type":"string"},"sort_ascending":{"type":"boolean"},"offset":{"type":"integer"},"output_mode":{"type":"string"}}), vec!["pattern"]),
    ].into_iter().map(|(name,description,properties,required)| ToolDefinition { name:name.into(),description:description.into(),parameters:json!({"type":"object","properties":properties,"required":required}) }).collect()
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

fn result(name: &str, args: &Value, structured: &Value) -> Result<Value> {
    if structured["exit_code"]
        .as_i64()
        .is_some_and(|code| code != 0)
    {
        return Err(ProviderError::Host(
            "native operation failed inside Codex sandbox".into(),
        ));
    }
    let output = structured["output"]
        .as_str()
        .ok_or_else(|| ProviderError::Host("missing structured shell output".into()))?;
    let output = output.strip_prefix(MARKER).ok_or_else(|| {
        ProviderError::Host("native operation output was truncated or changed".into())
    })?;
    let mut lines = output.lines();
    let header = lines.next().unwrap_or_default();
    let decode = |text: &str| -> Result<String> {
        let data = STANDARD
            .decode(text)
            .map_err(|_| ProviderError::Host("invalid encoded native output".into()))?;
        String::from_utf8(data)
            .map_err(|_| ProviderError::Host("binary native output is unsupported".into()))
    };
    let decode_read = |text: &str| -> Result<String> {
        let bytes = STANDARD
            .decode(text)
            .map_err(|_| ProviderError::Host("invalid encoded native output".into()))?;
        match String::from_utf8(bytes) {
            Ok(text) => Ok(text),
            Err(error)
                if error.utf8_error().error_len().is_none() && error.as_bytes().len() == 65_536 =>
            {
                let valid_up_to = error.utf8_error().valid_up_to();
                String::from_utf8(error.into_bytes()[..valid_up_to].to_vec())
                    .map_err(|_| ProviderError::Host("invalid UTF-8 native file content".into()))
            }
            Err(_) => Err(ProviderError::Host(
                "invalid UTF-8 native file content".into(),
            )),
        }
    };
    match name {
        "cursor_shell" => Ok(
            json!({"exit_code":header.parse::<i32>().map_err(|_|ProviderError::Host("missing native shell status".into()))?,"stdout":decode(lines.next().unwrap_or_default())?,"stderr":decode(lines.next().unwrap_or_default())?}),
        ),
        "cursor_read_file" | "cursor_write_file" => {
            let mut sizes = header.split_whitespace();
            let count = sizes
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| ProviderError::Host("missing native file line count".into()))?;
            let size = sizes
                .next()
                .and_then(|v| v.parse::<u64>().ok())
                .ok_or_else(|| ProviderError::Host("missing native file size".into()))?;
            if name == "cursor_write_file" {
                return Ok(json!({"lines_created":count,"file_size":size}));
            }
            let content = decode_read(lines.next().unwrap_or_default())?;
            Ok(
                json!({"truncated":(content.len() as u64) < size,"content":content,"total_lines":count,"file_size":size,"range_applied":true}),
            )
        }
        "cursor_delete_file" => Ok(json!({})),
        "cursor_ls" => {
            if header != "ls" {
                return Err(ProviderError::Host(
                    "missing native directory header".into(),
                ));
            }
            let base = lines.next().unwrap_or_default();
            if !std::path::Path::new(base).is_absolute() {
                return Err(ProviderError::Host(
                    "native directory path is not absolute".into(),
                ));
            }
            let records = decode(lines.next().unwrap_or_default())?;
            let mut files = vec![];
            let mut directories = vec![];
            for record in records.split('\0').filter(|record| !record.is_empty()) {
                let Some((kind, name)) = record.split_once('\t') else {
                    return Err(ProviderError::Host("invalid native directory entry".into()));
                };
                if kind == "d" {
                    directories.push(
                        std::path::Path::new(base)
                            .join(name)
                            .to_string_lossy()
                            .to_string(),
                    );
                } else {
                    files.push(name.to_owned());
                }
            }
            Ok(json!({"files":files,"directories":directories}))
        }
        "cursor_grep" => {
            let mut matches = Vec::<(String, Vec<Value>)>::new();
            for line in lines {
                let value: Value = serde_json::from_str(line)
                    .map_err(|_| ProviderError::Host("invalid native search output".into()))?;
                if value["type"] == "match" || value["type"] == "context" {
                    let data = &value["data"];
                    let path = data["path"]["text"]
                        .as_str()
                        .ok_or_else(|| ProviderError::Host("non-UTF8 search path".into()))?;
                    let index = matches
                        .iter()
                        .position(|(file, _)| file == path)
                        .unwrap_or_else(|| {
                            matches.push((path.to_owned(), Vec::new()));
                            matches.len() - 1
                        });
                    matches[index].1.push(json!({"line_number":data["line_number"],"content":data["lines"]["text"],"is_context_line":value["type"]=="context"}));
                }
            }
            let sort = args["sort"].as_str().unwrap_or("path");
            if sort == "path" {
                matches.sort_by(|left, right| left.0.cmp(&right.0));
            }
            let ascending = match args.get("sort_ascending") {
                None => true,
                Some(value) => value.as_bool().ok_or_else(|| {
                    ProviderError::Host("sort_ascending must be a boolean".into())
                })?,
            };
            if sort == "path" && !ascending {
                matches.reverse();
            } else if sort == "none" && !ascending {
                return Err(ProviderError::Unsupported(
                    "sort_ascending requires path sorting".into(),
                ));
            }
            let offset = integer(args, "offset", 0, 0, i32::MAX as u64)? as usize;
            let limit = args
                .get("head_limit")
                .map(|_| integer(args, "head_limit", 0, 0, i32::MAX as u64))
                .transpose()?
                .map(|n| n as usize);
            let mode = args["output_mode"].as_str().unwrap_or("content");
            let mut remaining = limit.unwrap_or(usize::MAX);
            let mut selected = Vec::new();
            let mut index = 0usize;
            for (file, rows) in matches {
                let original_len = rows.len();
                let rows = rows
                    .into_iter()
                    .skip(offset.saturating_sub(index))
                    .take(remaining)
                    .collect::<Vec<_>>();
                index = index.saturating_add(original_len);
                remaining = remaining.saturating_sub(rows.len());
                if !rows.is_empty() {
                    selected.push((file, rows));
                }
                if remaining == 0 {
                    break;
                }
            }
            match mode {
                "files_with_matches" => {
                    Ok(json!({"files":selected.iter().map(|(file,_)|file).collect::<Vec<_>>()}))
                }
                "count" => Ok(
                    json!({"counts":selected.iter().map(|(file,rows)|json!({"file":file,"count":rows.iter().filter(|row| row["is_context_line"] == false).count()})).collect::<Vec<_>>()}),
                ),
                "content" => Ok(
                    json!({"matches":selected.into_iter().map(|(file,lines)|json!({"file":file,"lines":lines})).collect::<Vec<_>>()}),
                ),
                _ => Err(ProviderError::Unsupported(
                    "unsupported Cursor grep output mode".into(),
                )),
            }
        }
        _ => Err(ProviderError::Unsupported(
            "unknown native operation".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::MARKER;
    use super::result;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD;
    use serde_json::json;
    #[test]
    fn malformed_result_shapes_fail_closed() {
        assert!(
            result(
                "cursor_read_file",
                &json!({}),
                &json!({"output":"missing marker"})
            )
            .is_err()
        );
        let invalid = format!("{MARKER}grep\nnot-json\n");
        assert!(result("cursor_grep", &json!({}), &json!({"output":invalid})).is_err());
    }

    #[test]
    fn read_clips_only_an_incomplete_utf8_sequence_at_the_output_cap() {
        let mut capped = vec![b'a'; 65_535];
        capped.push(0xE2);
        let encoded = STANDARD.encode(capped);
        let structured = json!({"output":format!("{MARKER}1 65537\n{encoded}\n")});
        let parsed = result("cursor_read_file", &json!({}), &structured).unwrap();
        assert_eq!(parsed["content"].as_str().unwrap().len(), 65_535);

        let malformed = STANDARD.encode([0xFF]);
        let structured = json!({"output":format!("{MARKER}1 1\n{malformed}\n")});
        assert!(result("cursor_read_file", &json!({}), &structured).is_err());

        let mut malformed_at_cap = vec![b'a'; 65_535];
        malformed_at_cap.push(0xFF);
        let structured =
            json!({"output":format!("{MARKER}1 65536\n{}\n", STANDARD.encode(malformed_at_cap))});
        assert!(result("cursor_read_file", &json!({}), &structured).is_err());
    }
}
