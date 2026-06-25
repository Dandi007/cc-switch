//! Payload Logger - 请求/响应全量记录 + 结构化提取
//!
//! 将过滤后的 request body 和原始 response body 写入 proxy_request_payloads 表，
//! 并提取结构化字段（用户消息、助手回复、工具调用、thinking）支持后续搜索和分析。

use crate::database::Database;
use crate::error::AppError;
use serde_json::Value;
use std::sync::Arc;

/// 一次请求的完整 payload 记录
pub struct PayloadLog {
    pub request_id: String,
    pub request_body: Value,
    pub response_body: Option<Value>,
    pub request_headers: Option<Value>,
    pub response_headers: Option<Value>,
    pub created_at: i64,
}

/// 从 payload 中提取的结构化字段
struct ExtractedFields {
    user_message: Option<String>,
    assistant_message: Option<String>,
    tools: Option<String>,
    thinking: Option<String>,
}

/// Payload 记录器
pub struct PayloadLogger {
    db: Arc<Database>,
}

impl PayloadLogger {
    pub fn new(db: &Arc<Database>) -> Self {
        Self { db: db.clone() }
    }

    /// 写入一条 payload 记录到数据库（同步，调用方需自行 spawn）
    pub fn log_payload(&self, log: PayloadLog) -> Result<(), AppError> {
        let extracted = extract_fields(&log.request_body, log.response_body.as_ref());

        let request_body_str =
            serde_json::to_string(&log.request_body).unwrap_or_else(|_| "{}".to_string());
        let response_body_str = log
            .response_body
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        let request_headers_str = log
            .request_headers
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());
        let response_headers_str = log
            .response_headers
            .as_ref()
            .and_then(|v| serde_json::to_string(v).ok());

        let payload_size =
            request_body_str.len() + response_body_str.as_ref().map(|s| s.len()).unwrap_or(0);

        let conn = crate::database::lock_conn!(self.db.conn);

        conn.execute(
            "INSERT OR REPLACE INTO proxy_request_payloads (
                request_id, request_body, response_body,
                request_headers, response_headers,
                extracted_user_message, extracted_assistant_message,
                extracted_tools, extracted_thinking,
                payload_size_bytes, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                log.request_id,
                request_body_str,
                response_body_str,
                request_headers_str,
                response_headers_str,
                extracted.user_message,
                extracted.assistant_message,
                extracted.tools,
                extracted.thinking,
                payload_size as i64,
                log.created_at,
            ],
        )
        .map_err(|e| AppError::Database(format!("payload 写入失败: {e}")))?;

        // 更新 FTS5 索引
        if extracted.user_message.is_some()
            || extracted.assistant_message.is_some()
            || extracted.thinking.is_some()
        {
            let rowid: Option<i64> = conn
                .query_row(
                    "SELECT rowid FROM proxy_request_payloads WHERE request_id = ?1",
                    [&log.request_id],
                    |row| row.get(0),
                )
                .ok();

            if let Some(rowid) = rowid {
                let _ = conn.execute(
                    "INSERT INTO proxy_payload_fts(rowid, extracted_user_message, extracted_assistant_message, extracted_thinking)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![
                        rowid,
                        extracted.user_message,
                        extracted.assistant_message,
                        extracted.thinking,
                    ],
                );
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// 字段提取
// ---------------------------------------------------------------------------

fn extract_fields(request: &Value, response: Option<&Value>) -> ExtractedFields {
    let user_message = extract_last_user_message(request);
    let (assistant_message, tools, thinking) = response
        .map(|r| extract_response_fields(r))
        .unwrap_or((None, None, None));

    ExtractedFields {
        user_message,
        assistant_message,
        tools,
        thinking,
    }
}

/// 从 messages 数组中提取最后一条 user content
fn extract_last_user_message(request: &Value) -> Option<String> {
    let messages = request.get("messages")?.as_array()?;
    let last_user = messages
        .iter()
        .rev()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))?;

    let content = last_user.get("content")?;
    // 纯文本
    if let Some(text) = content.as_str() {
        return Some(truncate_str(text, 10000));
    }
    // Anthropic array-of-blocks 格式
    if let Some(blocks) = content.as_array() {
        let texts: Vec<&str> = blocks
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                    b.get("text").and_then(|t| t.as_str())
                } else {
                    None
                }
            })
            .collect();
        if !texts.is_empty() {
            return Some(truncate_str(&texts.join("\n"), 10000));
        }
    }
    None
}

/// 从 response JSON 中提取 assistant message、tool 调用和 thinking 内容
///
/// 按 API 格式分两路：
/// - **OpenAI** (`/v1/chat/completions`): `choices[0].message`
/// - **Anthropic** (`/v1/messages`): `content[]` 数组（text / tool_use / thinking block）
fn extract_response_fields(response: &Value) -> (Option<String>, Option<String>, Option<String>) {
    // 尝试 OpenAI 格式
    if let Some(choices) = response.get("choices").and_then(|c| c.as_array()) {
        if let Some(choice) = choices.first() {
            let msg = choice.get("message").unwrap_or(choice);
            let text = msg
                .get("content")
                .and_then(|c| c.as_str())
                .map(|s| truncate_str(s, 10000));
            let thinking = msg
                .get("reasoning_content")
                .and_then(|c| c.as_str())
                .map(|s| truncate_str(s, 5000));
            let tools = msg
                .get("tool_calls")
                .and_then(|t| t.as_array())
                .map(|calls| {
                    let summaries: Vec<Value> = calls
                        .iter()
                        .filter_map(|c| {
                            let name = c
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())?;
                            Some(serde_json::json!({"name": name}))
                        })
                        .collect();
                    serde_json::to_string(&summaries).unwrap_or_default()
                });
            return (text, tools, thinking);
        }
    }

    // Anthropic 格式
    if let Some(content) = response.get("content").and_then(|c| c.as_array()) {
        let mut texts = Vec::new();
        let mut tool_summaries = Vec::new();
        let mut thinking_parts = Vec::new();

        for block in content {
            match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                        texts.push(t);
                    }
                }
                Some("tool_use") => {
                    if let Some(name) = block.get("name").and_then(|n| n.as_str()) {
                        tool_summaries.push(serde_json::json!({"name": name}));
                    }
                }
                Some("thinking") => {
                    if let Some(t) = block.get("thinking").and_then(|t| t.as_str()) {
                        thinking_parts.push(t);
                    }
                }
                _ => {}
            }
        }

        let text = if texts.is_empty() {
            None
        } else {
            Some(truncate_str(&texts.join("\n"), 10000))
        };
        let tools = if tool_summaries.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&tool_summaries).unwrap_or_default())
        };
        let thinking = if thinking_parts.is_empty() {
            None
        } else {
            Some(truncate_str(&thinking_parts.join("\n"), 5000))
        };
        return (text, tools, thinking);
    }

    (None, None, None)
}

/// 截断字符串，保证 UTF-8 边界安全
fn truncate_str(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...[truncated]", &s[..end])
    }
}
