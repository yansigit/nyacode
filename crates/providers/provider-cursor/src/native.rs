//! Native execution codecs. These requests are translated into host calls;
//! this module has no filesystem/process/network execution primitives.
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use codex_provider_api::ProviderError;
use codex_provider_api::Result;
use codex_provider_api::ToolCall;
use codex_provider_api::ToolResult;
use prost::Message;
use serde_json::Value;
use serde_json::json;

use crate::wire;

pub const SHELL: &str = "cursor_shell";
pub const READ: &str = "cursor_read_file";
pub const WRITE: &str = "cursor_write_file";
pub const DELETE: &str = "cursor_delete_file";
pub const GREP: &str = "cursor_grep";
pub const LS: &str = "cursor_ls";

#[derive(Clone)]
pub(crate) enum Operation {
    Mcp,
    Shell {
        command: String,
        cwd: String,
        stream: bool,
    },
    Read {
        path: String,
    },
    Write {
        path: String,
    },
    Delete {
        path: String,
    },
    Grep {
        pattern: String,
        path: String,
        mode: String,
    },
    Ls {
        path: String,
    },
}

pub(crate) fn call(exec: &wire::ExecServer) -> Result<Option<(ToolCall, Operation)>> {
    let (name, id, mut arguments, operation) = if let Some(shell) =
        exec.shell.as_ref().or(exec.shell_stream.as_ref())
    {
        if shell.background {
            return Err(ProviderError::Unsupported(
                "Cursor background shell requires an executable host lifecycle adapter".into(),
            ));
        }
        (
            SHELL,
            &shell.call_id,
            json!({"command":shell.command, "working_directory":shell.cwd, "timeout_ms":shell.hard_timeout.filter(|timeout| *timeout > 0)}),
            Operation::Shell {
                command: shell.command.clone(),
                cwd: shell.cwd.clone(),
                stream: exec.shell_stream.is_some(),
            },
        )
    } else if let Some(read) = &exec.read {
        (
            READ,
            &read.call_id,
            json!({"path":read.path, "offset":read.offset, "limit":read.limit}),
            Operation::Read {
                path: read.path.clone(),
            },
        )
    } else if let Some(write) = &exec.write {
        if !write.bytes.is_empty() {
            return Err(ProviderError::Unsupported(
                "Cursor binary write requires an executable host adapter".into(),
            ));
        }
        (
            WRITE,
            &write.call_id,
            json!({"path":write.path, "content":write.text}),
            Operation::Write {
                path: write.path.clone(),
            },
        )
    } else if let Some(delete) = &exec.delete {
        (
            DELETE,
            &delete.call_id,
            json!({"path":delete.path}),
            Operation::Delete {
                path: delete.path.clone(),
            },
        )
    } else if let Some(grep) = &exec.grep {
        (
            GREP,
            &grep.call_id,
            json!({"pattern":grep.pattern,"path":grep.path,"glob":grep.glob,"output_mode":grep.output_mode,
            "context_before":grep.context_before,"context_after":grep.context_after,"context":grep.context,"case_insensitive":grep.case_insensitive,
            "type":grep.file_type,"head_limit":grep.head_limit,"multiline":grep.multiline,"sort":grep.sort,"sort_ascending":grep.sort_ascending,"offset":grep.offset}),
            Operation::Grep {
                pattern: grep.pattern.clone(),
                path: grep.path.clone().unwrap_or_default(),
                mode: grep.output_mode.clone().unwrap_or_else(|| "content".into()),
            },
        )
    } else if let Some(ls) = &exec.ls {
        (
            LS,
            &ls.call_id,
            json!({"path":ls.path,"ignore":ls.ignore}),
            Operation::Ls {
                path: ls.path.clone(),
            },
        )
    } else {
        return Ok(None);
    };
    if id.is_empty() {
        return Err(ProviderError::Protocol(
            "Cursor native tool request missing call ID".into(),
        ));
    }
    let fields = arguments.as_object_mut().ok_or_else(|| {
        ProviderError::Protocol("Cursor native arguments must be an object".into())
    })?;
    fields.retain(|_, value| !value.is_null());
    if name == SHELL && fields.get("working_directory").and_then(Value::as_str) == Some("") {
        fields.remove("working_directory");
    }
    Ok(Some((
        ToolCall {
            id: id.clone(),
            name: name.into(),
            arguments,
        },
        operation,
    )))
}

#[derive(Clone, PartialEq, Message)]
struct ResultEnvelope {
    #[prost(bytes = "vec", optional, tag = "1")]
    success: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "2")]
    error: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "3")]
    read_rejected: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "4")]
    shell_rejected: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "5")]
    write_error: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "6")]
    file_rejected: Option<Vec<u8>>,
    #[prost(bytes = "vec", optional, tag = "7")]
    delete_error: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct PathError {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    error: String,
}

#[derive(Clone, PartialEq, Message)]
struct ReadSuccess {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    content: String,
    #[prost(int32, tag = "3")]
    total_lines: i32,
    #[prost(int64, tag = "4")]
    file_size: i64,
    #[prost(bool, tag = "6")]
    truncated: bool,
    #[prost(bool, tag = "8")]
    range_applied: bool,
}

#[derive(Clone, PartialEq, Message)]
struct WriteSuccess {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(int32, tag = "2")]
    lines: i32,
    #[prost(int32, tag = "3")]
    size: i32,
}

#[derive(Clone, PartialEq, Message)]
struct DeleteSuccess {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(string, tag = "2")]
    deleted: String,
}

#[derive(Clone, PartialEq, Message)]
struct ShellSuccess {
    #[prost(string, tag = "1")]
    command: String,
    #[prost(string, tag = "2")]
    cwd: String,
    #[prost(int32, tag = "3")]
    code: i32,
    #[prost(string, tag = "5")]
    stdout: String,
    #[prost(string, tag = "6")]
    stderr: String,
}

#[derive(Clone, PartialEq, Message)]
struct ShellStream {
    #[prost(message, optional, tag = "1")]
    stdout: Option<wire::Text>,
    #[prost(message, optional, tag = "2")]
    stderr: Option<wire::Text>,
    #[prost(message, optional, tag = "3")]
    exit: Option<ShellExit>,
    #[prost(bytes = "vec", optional, tag = "4")]
    start: Option<Vec<u8>>,
}

#[derive(Clone, PartialEq, Message)]
struct ShellRejected {
    #[prost(string, tag = "1")]
    command: String,
    #[prost(string, tag = "2")]
    cwd: String,
    #[prost(string, tag = "3")]
    reason: String,
}

#[derive(Clone, PartialEq, Message)]
struct ShellExit {
    #[prost(uint32, tag = "1")]
    code: u32,
    #[prost(string, tag = "2")]
    cwd: String,
}

pub(crate) fn response(
    id: u32,
    exec_id: &str,
    operation: &Operation,
    result: &ToolResult,
) -> Result<Vec<wire::ClientMessage>> {
    if !result.is_error {
        let (texts, ints): (&[&str], &[&str]) = match operation {
            Operation::Shell { .. } => (&["stdout", "stderr"], &["exit_code"]),
            Operation::Read { .. } => (&["content"], &["total_lines", "file_size"]),
            Operation::Write { .. } => (&[], &["lines_created", "file_size"]),
            _ => (&[], &[]),
        };
        for field in texts {
            string(&result.output[*field], field)?;
        }
        for field in ints {
            let number = result.output[*field].as_i64().ok_or_else(|| {
                ProviderError::Protocol(format!("host Cursor result has invalid {field}"))
            })?;
            if *field != "exit_code" && number < 0 {
                return Err(ProviderError::Protocol(format!(
                    "host Cursor result has negative {field}"
                )));
            }
            if *field != "file_size" || matches!(operation, Operation::Write { .. }) {
                integer(&result.output[*field], field)?;
            }
        }
    }
    let text = |field: &str| {
        result
            .output
            .get(field)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let int = |field: &str| {
        result
            .output
            .get(field)
            .and_then(Value::as_i64)
            .unwrap_or(0)
    };
    let base = wire::ExecClient {
        id,
        exec_id: exec_id.into(),
        ..Default::default()
    };
    let mut response = base.clone();
    if result.is_error && result.output.get("rejected").and_then(Value::as_bool) == Some(true) {
        let reason = result
            .output
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("Host rejected execution")
            .to_string();
        match operation {
            Operation::Mcp => {
                // MCP success with is_error still records a denied host result.
                response.mcp = Some(wire::McpResult {
                    success: Some(wire::McpSuccess {
                        is_error: true,
                        content: vec![wire::McpContent {
                            text: Some(wire::Text { text: reason }),
                            ..Default::default()
                        }],
                    }),
                });
            }
            Operation::Shell {
                command,
                cwd,
                stream,
            } => {
                let rejected = ShellRejected {
                    command: command.clone(),
                    cwd: cwd.clone(),
                    reason,
                };
                response.shell = Some(
                    ResultEnvelope {
                        shell_rejected: Some(rejected.encode_to_vec()),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                );
                if *stream {
                    let mut messages =
                        shell_stream_messages(&base, cwd, String::new(), rejected.reason, 1);
                    messages.push(wire::ClientMessage {
                        exec: Some(response),
                        ..Default::default()
                    });
                    messages.push(stream_close(id));
                    return Ok(messages);
                }
            }
            Operation::Read { path } => {
                response.read = Some(
                    ResultEnvelope {
                        read_rejected: Some(
                            PathError {
                                path: path.clone(),
                                error: reason,
                            }
                            .encode_to_vec(),
                        ),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
            }
            Operation::Write { path } => {
                response.write = Some(
                    ResultEnvelope {
                        file_rejected: Some(
                            PathError {
                                path: path.clone(),
                                error: reason,
                            }
                            .encode_to_vec(),
                        ),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
            }
            Operation::Delete { path } => {
                response.delete = Some(
                    ResultEnvelope {
                        file_rejected: Some(
                            PathError {
                                path: path.clone(),
                                error: reason,
                            }
                            .encode_to_vec(),
                        ),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
            }
            Operation::Ls { path } => {
                response.ls = Some(
                    ResultEnvelope {
                        read_rejected: Some(
                            PathError {
                                path: path.clone(),
                                error: reason,
                            }
                            .encode_to_vec(),
                        ),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
            }
            Operation::Grep { .. } => {
                response.grep = Some(
                    ResultEnvelope {
                        error: Some(wire::Text { text: reason }.encode_to_vec()),
                        ..Default::default()
                    }
                    .encode_to_vec(),
                )
            }
        }
        return Ok(vec![wire::ClientMessage {
            exec: Some(response),
            ..Default::default()
        }]);
    }
    match operation {
        Operation::Mcp => {
            response.mcp = Some(wire::McpResult {
                success: Some(wire::McpSuccess {
                    is_error: result.is_error || result.output["isError"].as_bool() == Some(true),
                    content: mcp_content(&result.output)?,
                }),
            });
        }
        Operation::Shell {
            command,
            cwd,
            stream,
        } => {
            let code = if result.is_error && int("exit_code") == 0 {
                1
            } else {
                i32::try_from(int("exit_code"))
                    .map_err(|_| ProviderError::Protocol("invalid host shell exit code".into()))?
            };
            let stdout = text("stdout");
            let stderr = if result.is_error && text("stderr").is_empty() {
                result.output.to_string()
            } else {
                text("stderr")
            };
            let success = ShellSuccess {
                command: command.clone(),
                cwd: cwd.clone(),
                code,
                stdout: stdout.clone(),
                stderr: stderr.clone(),
            }
            .encode_to_vec();
            let envelope = if code == 0 {
                ResultEnvelope {
                    success: Some(success),
                    ..Default::default()
                }
            } else {
                ResultEnvelope {
                    error: Some(success),
                    ..Default::default()
                }
            };
            response.shell = Some(envelope.encode_to_vec());
            if *stream {
                let mut messages = shell_stream_messages(&base, cwd, stdout, stderr, code);
                messages.push(wire::ClientMessage {
                    exec: Some(response),
                    ..Default::default()
                });
                messages.push(stream_close(id));
                return Ok(messages);
            }
        }
        Operation::Read { path } => {
            let envelope = if result.is_error {
                ResultEnvelope {
                    error: Some(
                        PathError {
                            path: path.clone(),
                            error: result.output.to_string(),
                        }
                        .encode_to_vec(),
                    ),
                    ..Default::default()
                }
            } else {
                ResultEnvelope {
                    success: Some(
                        ReadSuccess {
                            path: path.clone(),
                            content: text("content"),
                            total_lines: int("total_lines") as i32,
                            file_size: int("file_size"),
                            truncated: result
                                .output
                                .get("truncated")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                            range_applied: result
                                .output
                                .get("range_applied")
                                .and_then(Value::as_bool)
                                .unwrap_or(false),
                        }
                        .encode_to_vec(),
                    ),
                    ..Default::default()
                }
            };
            response.read = Some(envelope.encode_to_vec());
        }
        Operation::Write { path } => {
            let envelope = if result.is_error {
                ResultEnvelope {
                    write_error: Some(
                        PathError {
                            path: path.clone(),
                            error: result.output.to_string(),
                        }
                        .encode_to_vec(),
                    ),
                    ..Default::default()
                }
            } else {
                ResultEnvelope {
                    success: Some(
                        WriteSuccess {
                            path: path.clone(),
                            lines: int("lines_created") as i32,
                            size: int("file_size") as i32,
                        }
                        .encode_to_vec(),
                    ),
                    ..Default::default()
                }
            };
            response.write = Some(envelope.encode_to_vec());
        }
        Operation::Delete { path } => {
            let envelope = if result.is_error {
                ResultEnvelope {
                    delete_error: Some(
                        PathError {
                            path: path.clone(),
                            error: result.output.to_string(),
                        }
                        .encode_to_vec(),
                    ),
                    ..Default::default()
                }
            } else {
                ResultEnvelope {
                    success: Some(
                        DeleteSuccess {
                            path: path.clone(),
                            deleted: path.clone(),
                        }
                        .encode_to_vec(),
                    ),
                    ..Default::default()
                }
            };
            response.delete = Some(envelope.encode_to_vec());
        }
        Operation::Ls { path } => {
            let envelope = if result.is_error {
                ResultEnvelope {
                    error: Some(
                        PathError {
                            path: path.clone(),
                            error: result.output.to_string(),
                        }
                        .encode_to_vec(),
                    ),
                    ..Default::default()
                }
            } else {
                let files = result
                    .output
                    .get("files")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        ProviderError::Protocol("host Cursor ls result missing files array".into())
                    })?;
                let directories = result
                    .output
                    .get("directories")
                    .and_then(|v| v.as_array())
                    .ok_or_else(|| {
                        ProviderError::Protocol(
                            "host Cursor ls result missing directories array".into(),
                        )
                    })?;
                let root = LsNode {
                    path: path.clone(),
                    files: files
                        .iter()
                        .map(|v| {
                            Ok(LsFile {
                                name: string(v, "ls filename")?,
                            })
                        })
                        .collect::<Result<_>>()?,
                    directories: directories
                        .iter()
                        .map(|v| {
                            Ok(LsNode {
                                path: string(v, "ls directory path")?,
                                ..Default::default()
                            })
                        })
                        .collect::<Result<_>>()?,
                    processed: true,
                };
                ResultEnvelope {
                    success: Some(LsSuccess { root: Some(root) }.encode_to_vec()),
                    ..Default::default()
                }
            };
            response.ls = Some(envelope.encode_to_vec());
        }
        Operation::Grep {
            pattern,
            path,
            mode,
        } => {
            let envelope = if result.is_error {
                ResultEnvelope {
                    error: Some(
                        wire::Text {
                            text: result.output.to_string(),
                        }
                        .encode_to_vec(),
                    ),
                    ..Default::default()
                }
            } else {
                let union = match mode.as_str() {
                    "files_with_matches" => {
                        let files = result
                            .output
                            .get("files")
                            .and_then(|v| v.as_array())
                            .ok_or_else(|| {
                                ProviderError::Protocol(
                                    "host Cursor grep result missing files array".into(),
                                )
                            })?;
                        let files = files
                            .iter()
                            .map(|v| string(v, "grep file path"))
                            .collect::<Result<Vec<_>>>()?;
                        GrepUnion {
                            files: Some(GrepFiles {
                                total: files.len() as i32,
                                files,
                            }),
                            ..Default::default()
                        }
                    }
                    "count" => {
                        let counts = result
                            .output
                            .get("counts")
                            .and_then(|v| v.as_array())
                            .ok_or_else(|| {
                                ProviderError::Protocol(
                                    "host Cursor grep result missing counts array".into(),
                                )
                            })?;
                        let counts = counts
                            .iter()
                            .map(|v| {
                                Ok(GrepFileCount {
                                    file: string(&v["file"], "grep file path")?,
                                    count: integer(&v["count"], "grep match count")?,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        GrepUnion {
                            count: Some(GrepCounts {
                                files: counts.len() as i32,
                                total: counts
                                    .iter()
                                    .try_fold(0i32, |sum, v| sum.checked_add(v.count))
                                    .ok_or_else(|| {
                                        ProviderError::Protocol(
                                            "host Cursor grep count overflow".into(),
                                        )
                                    })?,
                                counts,
                            }),
                            ..Default::default()
                        }
                    }
                    "content" => {
                        let matches = result
                            .output
                            .get("matches")
                            .and_then(|v| v.as_array())
                            .ok_or_else(|| {
                                ProviderError::Protocol(
                                    "host Cursor grep result missing matches array".into(),
                                )
                            })?;
                        let matches = matches
                            .iter()
                            .map(|v| {
                                let lines = v["lines"].as_array().ok_or_else(|| {
                                    ProviderError::Protocol(
                                        "host Cursor grep match missing lines".into(),
                                    )
                                })?;
                                Ok(GrepFileMatch {
                                    file: string(&v["file"], "grep file path")?,
                                    matches: lines
                                        .iter()
                                        .map(|line| {
                                            Ok(GrepLine {
                                                line: integer(
                                                    &line["line_number"],
                                                    "grep line number",
                                                )?,
                                                content: string(
                                                    &line["content"],
                                                    "grep line content",
                                                )?,
                                                context: line["is_context_line"]
                                                    .as_bool()
                                                    .unwrap_or(false),
                                            })
                                        })
                                        .collect::<Result<_>>()?,
                                })
                            })
                            .collect::<Result<Vec<_>>>()?;
                        let count = matches
                            .iter()
                            .try_fold(0i32, |sum, v| {
                                i32::try_from(v.matches.len())
                                    .ok()
                                    .and_then(|count| sum.checked_add(count))
                            })
                            .ok_or_else(|| {
                                ProviderError::Protocol(
                                    "host Cursor grep content count overflow".into(),
                                )
                            })?;
                        let matched = matches
                            .iter()
                            .flat_map(|v| &v.matches)
                            .filter(|line| !line.context)
                            .count();
                        GrepUnion {
                            content: Some(GrepContent {
                                matches,
                                total_lines: count,
                                matched_lines: i32::try_from(matched).map_err(|_| {
                                    ProviderError::Protocol(
                                        "host Cursor grep matched count overflow".into(),
                                    )
                                })?,
                            }),
                            ..Default::default()
                        }
                    }
                    _ => {
                        return Err(ProviderError::Unsupported(
                            "unsupported Cursor grep output mode".into(),
                        ));
                    }
                };
                ResultEnvelope {
                    success: Some(
                        GrepSuccess {
                            pattern: pattern.clone(),
                            path: path.clone(),
                            mode: mode.clone(),
                            workspaces: [(path.clone(), union)].into_iter().collect(),
                        }
                        .encode_to_vec(),
                    ),
                    ..Default::default()
                }
            };
            response.grep = Some(envelope.encode_to_vec());
        }
    }
    Ok(vec![wire::ClientMessage {
        exec: Some(response),
        ..Default::default()
    }])
}

pub(crate) fn mcp_content(output: &serde_json::Value) -> Result<Vec<wire::McpContent>> {
    let content = output
        .get("content")
        .and_then(serde_json::Value::as_array)
        .or_else(|| {
            output
                .as_array()
                .filter(|items| items.iter().any(|item| item.get("type").is_some()))
        });
    let Some(content) = content else {
        return Ok(vec![wire::McpContent {
            text: Some(wire::Text {
                text: output
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| output.to_string()),
            }),
            ..Default::default()
        }]);
    };
    content.iter().map(|item| {
        match item["type"].as_str() {
            Some("text" | "input_text" | "output_text") => Ok(wire::McpContent {text:Some(wire::Text {text:string(&item["text"], "MCP text content")?}),..Default::default()}),
            Some("image" | "input_image") => {
                let (encoded, mime) = if item["type"] == "image" {
                    (item["data"].as_str().ok_or_else(|| ProviderError::Protocol("MCP image data missing".into()))?,item["mimeType"].as_str().ok_or_else(|| ProviderError::Protocol("MCP image MIME type missing".into()))?)
                } else {
                    let url = item["image_url"].as_str().ok_or_else(|| ProviderError::Protocol("MCP image URL missing".into()))?;
                    let (header, encoded) = url.strip_prefix("data:").and_then(|value|value.split_once(',')).ok_or_else(|| ProviderError::Unsupported("Cursor tool result images require inline data URLs".into()))?;
                    let mime = header.strip_suffix(";base64").ok_or_else(|| ProviderError::Unsupported("Cursor tool result image data must be base64".into()))?;
                    (encoded,mime)
                };
                if !matches!(mime,"image/png" | "image/jpeg" | "image/webp" | "image/gif") { return Err(ProviderError::Unsupported("Cursor tool result image MIME type".into())); }
                if encoded.len() > 12 * 1024 * 1024 { return Err(ProviderError::Unsupported("Cursor tool result image exceeds 8 MiB".into())); }
                let data = STANDARD.decode(encoded).map_err(|_|ProviderError::Protocol("invalid Cursor tool result image base64".into()))?;
                if data.len() > 8 * 1024 * 1024 { return Err(ProviderError::Unsupported("Cursor tool result image exceeds 8 MiB".into())); }
                Ok(wire::McpContent {image:Some(wire::McpImage {data,mime:mime.into()}),..Default::default()})
            }
            _ => Err(ProviderError::Unsupported("Cursor tool result content must be text or an inline image; audio and resources require native adapters".into())),
        }
    }).collect()
}

fn stream_close(id: u32) -> wire::ClientMessage {
    wire::ClientMessage {
        exec_control: Some(wire::ExecClientControl {
            stream_close: Some(wire::ExecStreamClose { id }),
        }),
        ..Default::default()
    }
}

fn shell_stream_messages(
    base: &wire::ExecClient,
    cwd: &str,
    stdout: String,
    stderr: String,
    code: i32,
) -> Vec<wire::ClientMessage> {
    [
        ShellStream {
            start: Some(Vec::new()),
            ..Default::default()
        },
        ShellStream {
            stdout: Some(wire::Text { text: stdout }),
            ..Default::default()
        },
        ShellStream {
            stderr: Some(wire::Text { text: stderr }),
            ..Default::default()
        },
        ShellStream {
            exit: Some(ShellExit {
                code: code as u32,
                cwd: cwd.into(),
            }),
            ..Default::default()
        },
    ]
    .into_iter()
    .map(|part| {
        let mut exec = base.clone();
        exec.shell_stream = Some(part.encode_to_vec());
        wire::ClientMessage {
            exec: Some(exec),
            ..Default::default()
        }
    })
    .collect()
}

fn string(value: &serde_json::Value, field: &str) -> Result<String> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| ProviderError::Protocol(format!("host Cursor result has invalid {field}")))
}

fn integer(value: &serde_json::Value, field: &str) -> Result<i32> {
    value
        .as_i64()
        .and_then(|v| i32::try_from(v).ok())
        .ok_or_else(|| ProviderError::Protocol(format!("host Cursor result has invalid {field}")))
}

#[derive(Clone, PartialEq, Message)]
struct LsSuccess {
    #[prost(message, optional, tag = "1")]
    root: Option<LsNode>,
}
#[derive(Clone, PartialEq, Message)]
struct LsNode {
    #[prost(string, tag = "1")]
    path: String,
    #[prost(message, repeated, tag = "2")]
    directories: Vec<LsNode>,
    #[prost(message, repeated, tag = "3")]
    files: Vec<LsFile>,
    #[prost(bool, tag = "4")]
    processed: bool,
}
#[derive(Clone, PartialEq, Message)]
struct LsFile {
    #[prost(string, tag = "1")]
    name: String,
}
#[derive(Clone, PartialEq, Message)]
struct GrepSuccess {
    #[prost(string, tag = "1")]
    pattern: String,
    #[prost(string, tag = "2")]
    path: String,
    #[prost(string, tag = "3")]
    mode: String,
    #[prost(map = "string, message", tag = "4")]
    workspaces: std::collections::HashMap<String, GrepUnion>,
}
#[derive(Clone, PartialEq, Message)]
struct GrepUnion {
    #[prost(message, optional, tag = "1")]
    count: Option<GrepCounts>,
    #[prost(message, optional, tag = "2")]
    files: Option<GrepFiles>,
    #[prost(message, optional, tag = "3")]
    content: Option<GrepContent>,
}
#[derive(Clone, PartialEq, Message)]
struct GrepCounts {
    #[prost(message, repeated, tag = "1")]
    counts: Vec<GrepFileCount>,
    #[prost(int32, tag = "2")]
    files: i32,
    #[prost(int32, tag = "3")]
    total: i32,
}
#[derive(Clone, PartialEq, Message)]
struct GrepFileCount {
    #[prost(string, tag = "1")]
    file: String,
    #[prost(int32, tag = "2")]
    count: i32,
}
#[derive(Clone, PartialEq, Message)]
struct GrepFiles {
    #[prost(string, repeated, tag = "1")]
    files: Vec<String>,
    #[prost(int32, tag = "2")]
    total: i32,
}
#[derive(Clone, PartialEq, Message)]
struct GrepContent {
    #[prost(message, repeated, tag = "1")]
    matches: Vec<GrepFileMatch>,
    #[prost(int32, tag = "2")]
    total_lines: i32,
    #[prost(int32, tag = "3")]
    matched_lines: i32,
}
#[derive(Clone, PartialEq, Message)]
struct GrepFileMatch {
    #[prost(string, tag = "1")]
    file: String,
    #[prost(message, repeated, tag = "2")]
    matches: Vec<GrepLine>,
}
#[derive(Clone, PartialEq, Message)]
struct GrepLine {
    #[prost(int32, tag = "1")]
    line: i32,
    #[prost(string, tag = "2")]
    content: String,
    #[prost(bool, tag = "4")]
    context: bool,
}
