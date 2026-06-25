use crate::error::AppError;
use crate::store::AppState;
use serde::Serialize;
use serde_json::Value;
use std::io::Write;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadSearchResult {
    pub request_id: String,
    pub created_at: i64,
    pub app_type: String,
    pub model: String,
    pub session_id: Option<String>,
    pub user_message: Option<String>,
    pub assistant_message: Option<String>,
    pub tools_used: Option<Value>,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_cost_usd: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadDetail {
    pub request_id: String,
    pub request_body: Value,
    pub response_body: Option<Value>,
    pub request_headers: Option<Value>,
    pub response_headers: Option<Value>,
    pub extracted_user_message: Option<String>,
    pub extracted_assistant_message: Option<String>,
    pub extracted_tools: Option<String>,
    pub extracted_thinking: Option<String>,
    pub payload_size_bytes: i64,
    pub created_at: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PayloadStatGroup {
    pub key: String,
    pub request_count: u32,
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_cost_usd: String,
    pub total_payload_bytes: u64,
}

pub struct PayloadService;

impl PayloadService {
    pub fn search(
        state: &AppState,
        query: &str,
        start: Option<i64>,
        end: Option<i64>,
        app_filter: Option<&str>,
    ) -> Result<Vec<PayloadSearchResult>, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        let mut sql = String::from(
            "SELECT p.request_id, p.created_at,
                    COALESCE(l.app_type, '') as app_type,
                    COALESCE(l.model, '') as model,
                    l.session_id,
                    p.extracted_user_message,
                    p.extracted_assistant_message,
                    p.extracted_tools,
                    COALESCE(l.input_tokens, 0),
                    COALESCE(l.output_tokens, 0),
                    COALESCE(l.total_cost_usd, '0')
             FROM proxy_payload_fts f
             JOIN proxy_request_payloads p ON p.rowid = f.rowid
             LEFT JOIN proxy_request_logs l ON l.request_id = p.request_id
             WHERE proxy_payload_fts MATCH ?1",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = vec![Box::new(query.to_string())];
        let mut param_idx = 2;

        if let Some(s) = start {
            sql.push_str(&format!(" AND p.created_at >= ?{param_idx}"));
            params.push(Box::new(s));
            param_idx += 1;
        }
        if let Some(e) = end {
            sql.push_str(&format!(" AND p.created_at <= ?{param_idx}"));
            params.push(Box::new(e));
            param_idx += 1;
        }
        if let Some(app) = app_filter {
            sql.push_str(&format!(" AND l.app_type = ?{param_idx}"));
            params.push(Box::new(app.to_string()));
            // param_idx += 1; // not needed after last param
        }

        sql.push_str(" ORDER BY p.created_at DESC LIMIT 50");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(PayloadSearchResult {
                    request_id: row.get(0)?,
                    created_at: row.get(1)?,
                    app_type: row.get(2)?,
                    model: row.get(3)?,
                    session_id: row.get(4)?,
                    user_message: row.get(5)?,
                    assistant_message: row.get(6)?,
                    tools_used: row
                        .get::<_, Option<String>>(7)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    input_tokens: row.get(8)?,
                    output_tokens: row.get(9)?,
                    total_cost_usd: row.get(10)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn get(state: &AppState, request_id: &str) -> Result<PayloadDetail, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        conn.query_row(
            "SELECT request_id, request_body, response_body,
                    request_headers, response_headers,
                    extracted_user_message, extracted_assistant_message,
                    extracted_tools, extracted_thinking,
                    payload_size_bytes, created_at
             FROM proxy_request_payloads WHERE request_id = ?1",
            [request_id],
            |row| {
                let request_body_str: String = row.get(1)?;
                let response_body_str: Option<String> = row.get(2)?;
                let request_headers_str: Option<String> = row.get(3)?;
                let response_headers_str: Option<String> = row.get(4)?;

                Ok(PayloadDetail {
                    request_id: row.get(0)?,
                    request_body: serde_json::from_str(&request_body_str).unwrap_or(Value::Null),
                    response_body: response_body_str.and_then(|s| serde_json::from_str(&s).ok()),
                    request_headers: request_headers_str
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    response_headers: response_headers_str
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    extracted_user_message: row.get(5)?,
                    extracted_assistant_message: row.get(6)?,
                    extracted_tools: row.get(7)?,
                    extracted_thinking: row.get(8)?,
                    payload_size_bytes: row.get(9)?,
                    created_at: row.get(10)?,
                })
            },
        )
        .map_err(|e| AppError::Database(format!("payload not found: {e}")))
    }

    pub fn session(
        state: &AppState,
        session_id: &str,
    ) -> Result<Vec<PayloadSearchResult>, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        let mut stmt = conn
            .prepare(
                "SELECT p.request_id, p.created_at,
                        COALESCE(l.app_type, ''), COALESCE(l.model, ''),
                        l.session_id,
                        p.extracted_user_message, p.extracted_assistant_message,
                        p.extracted_tools,
                        COALESCE(l.input_tokens, 0), COALESCE(l.output_tokens, 0),
                        COALESCE(l.total_cost_usd, '0')
                 FROM proxy_request_payloads p
                 LEFT JOIN proxy_request_logs l ON l.request_id = p.request_id
                 WHERE l.session_id = ?1
                 ORDER BY p.created_at ASC",
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows = stmt
            .query_map([session_id], |row| {
                Ok(PayloadSearchResult {
                    request_id: row.get(0)?,
                    created_at: row.get(1)?,
                    app_type: row.get(2)?,
                    model: row.get(3)?,
                    session_id: row.get(4)?,
                    user_message: row.get(5)?,
                    assistant_message: row.get(6)?,
                    tools_used: row
                        .get::<_, Option<String>>(7)?
                        .and_then(|s| serde_json::from_str(&s).ok()),
                    input_tokens: row.get(8)?,
                    output_tokens: row.get(9)?,
                    total_cost_usd: row.get(10)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn stats(
        state: &AppState,
        start: Option<i64>,
        end: Option<i64>,
        group_by: &str,
    ) -> Result<Vec<PayloadStatGroup>, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        let group_col = match group_by {
            "tool" => "p.extracted_tools",
            "app" => "l.app_type",
            _ => "l.model",
        };

        let mut sql = format!(
            "SELECT COALESCE({group_col}, 'unknown'),
                    COUNT(*),
                    SUM(COALESCE(l.input_tokens, 0)),
                    SUM(COALESCE(l.output_tokens, 0)),
                    PRINTF('%.6f', SUM(CAST(COALESCE(l.total_cost_usd, '0') AS REAL))),
                    SUM(COALESCE(p.payload_size_bytes, 0))
             FROM proxy_request_payloads p
             LEFT JOIN proxy_request_logs l ON l.request_id = p.request_id
             WHERE 1=1"
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(s) = start {
            sql.push_str(&format!(" AND p.created_at >= ?{idx}"));
            params.push(Box::new(s));
            idx += 1;
        }
        if let Some(e) = end {
            sql.push_str(&format!(" AND p.created_at <= ?{idx}"));
            params.push(Box::new(e));
            // idx += 1;
        }

        sql.push_str(&format!(" GROUP BY {group_col} ORDER BY COUNT(*) DESC"));

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let rows = stmt
            .query_map(param_refs.as_slice(), |row| {
                Ok(PayloadStatGroup {
                    key: row.get(0)?,
                    request_count: row.get(1)?,
                    total_input_tokens: row.get(2)?,
                    total_output_tokens: row.get(3)?,
                    total_cost_usd: row.get(4)?,
                    total_payload_bytes: row.get(5)?,
                })
            })
            .map_err(|e| AppError::Database(e.to_string()))?;

        rows.collect::<Result<Vec<_>, _>>()
            .map_err(|e| AppError::Database(e.to_string()))
    }

    pub fn export(
        state: &AppState,
        start: Option<i64>,
        end: Option<i64>,
        output_path: &str,
    ) -> Result<u64, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        let mut sql = String::from(
            "SELECT p.request_id, p.request_body, p.response_body,
                    p.extracted_user_message, p.extracted_assistant_message,
                    p.extracted_tools, p.extracted_thinking,
                    p.payload_size_bytes, p.created_at,
                    l.app_type, l.model, l.session_id,
                    l.input_tokens, l.output_tokens, l.total_cost_usd
             FROM proxy_request_payloads p
             LEFT JOIN proxy_request_logs l ON l.request_id = p.request_id
             WHERE 1=1",
        );

        let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let mut idx = 1;

        if let Some(s) = start {
            sql.push_str(&format!(" AND p.created_at >= ?{idx}"));
            params.push(Box::new(s));
            idx += 1;
        }
        if let Some(e) = end {
            sql.push_str(&format!(" AND p.created_at <= ?{idx}"));
            params.push(Box::new(e));
            // idx += 1;
        }

        sql.push_str(" ORDER BY p.created_at ASC");

        let param_refs: Vec<&dyn rusqlite::types::ToSql> =
            params.iter().map(|p| p.as_ref()).collect();
        let mut stmt = conn
            .prepare(&sql)
            .map_err(|e| AppError::Database(e.to_string()))?;

        let mut file = std::fs::File::create(output_path)
            .map_err(|e| AppError::Message(format!("failed to create export file: {e}")))?;

        let mut count: u64 = 0;
        let mut rows = stmt
            .query(param_refs.as_slice())
            .map_err(|e| AppError::Database(e.to_string()))?;

        while let Some(row) = rows.next().map_err(|e| AppError::Database(e.to_string()))? {
            let entry = serde_json::json!({
                "request_id": row.get::<_, String>(0).unwrap_or_default(),
                "request_body": row.get::<_, String>(1).ok().and_then(|s| serde_json::from_str::<Value>(&s).ok()),
                "response_body": row.get::<_, Option<String>>(2).ok().flatten().and_then(|s| serde_json::from_str::<Value>(&s).ok()),
                "extracted_user_message": row.get::<_, Option<String>>(3).unwrap_or(None),
                "extracted_assistant_message": row.get::<_, Option<String>>(4).unwrap_or(None),
                "extracted_tools": row.get::<_, Option<String>>(5).unwrap_or(None),
                "extracted_thinking": row.get::<_, Option<String>>(6).unwrap_or(None),
                "payload_size_bytes": row.get::<_, i64>(7).unwrap_or(0),
                "created_at": row.get::<_, i64>(8).unwrap_or(0),
                "app_type": row.get::<_, Option<String>>(9).unwrap_or(None),
                "model": row.get::<_, Option<String>>(10).unwrap_or(None),
                "session_id": row.get::<_, Option<String>>(11).unwrap_or(None),
                "input_tokens": row.get::<_, Option<u32>>(12).unwrap_or(None),
                "output_tokens": row.get::<_, Option<u32>>(13).unwrap_or(None),
                "total_cost_usd": row.get::<_, Option<String>>(14).unwrap_or(None),
            });
            serde_json::to_writer(&mut file, &entry)
                .map_err(|e| AppError::Message(format!("write failed: {e}")))?;
            writeln!(file).map_err(|e| AppError::Message(format!("write newline failed: {e}")))?;
            count += 1;
        }

        Ok(count)
    }

    pub fn prune(state: &AppState, before_timestamp: i64, dry_run: bool) -> Result<u64, AppError> {
        let conn = crate::database::lock_conn!(state.db.conn);

        let count: u64 = conn
            .query_row(
                "SELECT COUNT(*) FROM proxy_request_payloads WHERE created_at < ?1",
                [before_timestamp],
                |row| row.get(0),
            )
            .map_err(|e| AppError::Database(e.to_string()))?;

        if dry_run || count == 0 {
            return Ok(count);
        }

        // Delete FTS entries first
        conn.execute(
            "DELETE FROM proxy_payload_fts WHERE rowid IN (
                SELECT rowid FROM proxy_request_payloads WHERE created_at < ?1
            )",
            [before_timestamp],
        )
        .map_err(|e| AppError::Database(format!("FTS cleanup failed: {e}")))?;

        conn.execute(
            "DELETE FROM proxy_request_payloads WHERE created_at < ?1",
            [before_timestamp],
        )
        .map_err(|e| AppError::Database(format!("payload cleanup failed: {e}")))?;

        Ok(count)
    }
}
