//! Authenticated, read-only SQL projection and exact scanning of application attachment chunks.
//! This is independent of the still-unimplemented native external-asset API.

use crate::bind::statement::TableArgument;
use crate::bind::CatalogColumn;
use crate::session::{QueryResult, Session, SessionError};
use bicdb_catalog::dict::ColTypeCode;
use bicdb_common::{seq::CommitSeq, sha256::Sha256};
use bicdb_exec::Value;
use bicdb_types::Number;
use std::time::{Duration, Instant};

const META: &str = "a.aid, a.user_id, a.workspace_id, a.sid, a.tid, a.seq, a.filename, a.file_type, a.media_type, a.purpose, a.origin, a.byte_len, a.sha256, a.created_ms";
const MAX_FILES: usize = 256;
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const MAX_HITS: usize = 1000;
const CHUNK: u64 = 8192;
const COLUMNS: &[(&str, ColTypeCode, u32)] = &[
    ("attachment_id", ColTypeCode::Varchar2, 128),
    ("user_id", ColTypeCode::Number, 0),
    ("workspace_id", ColTypeCode::Number, 0),
    ("session_id", ColTypeCode::Varchar2, 128),
    ("turn_id", ColTypeCode::Varchar2, 128),
    ("turn_seq", ColTypeCode::Number, 0),
    ("filename", ColTypeCode::Varchar2, 512),
    ("file_type", ColTypeCode::Varchar2, 64),
    ("media_type", ColTypeCode::Varchar2, 128),
    ("purpose", ColTypeCode::Varchar2, 1024),
    ("origin", ColTypeCode::Varchar2, 16),
    ("byte_len", ColTypeCode::Number, 0),
    ("sha256", ColTypeCode::Varchar2, 64),
    ("created_ms", ColTypeCode::Number, 0),
    ("searchable", ColTypeCode::Boolean, 0),
    ("search_mode", ColTypeCode::Varchar2, 32),
];
const HIT_COLUMNS: &[(&str, ColTypeCode, u32)] = &[
    ("line_number", ColTypeCode::Number, 0),
    ("byte_offset", ColTypeCode::Number, 0),
    ("snippet", ColTypeCode::Varchar2, 1024),
    ("scan_snapshot", ColTypeCode::Number, 0),
];

pub(crate) fn columns(name: &str) -> Option<Vec<CatalogColumn>> {
    if name != "attachment$" && name != "attachment_grep" {
        return None;
    }
    let hit = if name == "attachment_grep" {
        HIT_COLUMNS
    } else {
        &[]
    };
    Some(
        COLUMNS
            .iter()
            .chain(hit)
            .enumerate()
            .map(|(index, (name, code, length))| CatalogColumn {
                col: index as u32 + 1,
                name: (*name).into(),
                type_code: *code as u32,
                length: *length,
                nullable: *name == "turn_id",
            })
            .collect(),
    )
}

fn fail(message: &str) -> SessionError {
    SessionError::State(format!("ATTACHMENT_SEARCH: {message}"))
}
fn text(value: &Value) -> Result<&str, SessionError> {
    match value {
        Value::Bytes(bytes) => {
            std::str::from_utf8(bytes).map_err(|_| fail("non-UTF8 metadata/argument"))
        }
        _ => Err(fail("text value required")),
    }
}
fn integer(value: &Value) -> Result<u64, SessionError> {
    match value {
        Value::Number(number) => number
            .to_string()
            .parse()
            .map_err(|_| fail("invalid unsigned metadata number")),
        _ => Err(fail("numeric value required")),
    }
}
fn number(value: u64) -> Value {
    Value::Number(Number::parse(&value.to_string()).expect("u64 number"))
}
fn bytes(value: &str) -> Value {
    Value::Bytes(value.as_bytes().to_vec())
}
fn hex(sha: &[u8]) -> String {
    sha.iter().map(|b| format!("{b:02x}")).collect()
}
fn snippet(mut fragment: &[u8]) -> Result<String, SessionError> {
    // A bounded byte window may begin/end inside a valid UTF-8 code point.
    // Trim only incomplete boundary bytes; preserve real U+FFFD characters.
    while fragment.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
        fragment = &fragment[1..];
    }
    match std::str::from_utf8(fragment) {
        Ok(text) => Ok(text.into()),
        Err(error) if error.error_len().is_none() => {
            Ok(std::str::from_utf8(&fragment[..error.valid_up_to()])
                .map_err(|_| fail("invalid snippet UTF-8"))?
                .into())
        }
        Err(_) => Err(fail("invalid snippet UTF-8")),
    }
}
fn chunk_key(id: &str, ordinal: u64) -> String {
    let mut sha = Sha256::new();
    sha.update(b"bicbot-key-v1\0");
    for part in ["attachment-chunk", id, &ordinal.to_string()] {
        sha.update(&(part.len() as u64).to_be_bytes());
        sha.update(part.as_bytes());
    }
    hex(&sha.finalize())
}
fn eligible(row: &[Value]) -> Result<bool, SessionError> {
    let ext = text(&row[7])?;
    let mime = text(&row[8])?;
    Ok(matches!(
        ext,
        "htm" | "html" | "md" | "markdown" | "txt" | "log" | "sql" | "csv" | "json" | "xml" | "svg"
    ) && (mime.starts_with("text/")
        || matches!(
            mime,
            "application/json" | "application/xml" | "image/svg+xml"
        )))
}

impl Session<'_, '_, '_, '_> {
    // Normal SQL compiler/executor, same connection, snapshot and own transaction.
    // Restore the outer parameter vector even on errors (JOIN/UNION depend on it).
    fn attachment_read(
        &mut self,
        sql: &str,
        parameters: &[(&str, Value)],
    ) -> Result<Vec<Vec<Value>>, SessionError> {
        let outer = std::mem::take(&mut self.exec_params);
        let result = self.execute_with_params(sql, parameters);
        self.exec_params = outer;
        let mut result = result?;
        match result.pop() {
            Some(QueryResult::Rows { rows, .. }) => Ok(rows),
            _ => Err(fail("internal read returned no rows")),
        }
    }

    fn attachment_scope(&mut self) -> Result<(u64, u64), SessionError> {
        let user = self
            .identity()
            .filter(|identity| !identity.is_expired())
            .map(|identity| identity.user_id())
            .ok_or_else(|| fail("requires named authentication in the user's private workspace"))?;
        if self.on_public_workspace() {
            return Err(fail("private workspace required"));
        }
        let rows = self.attachment_read("SELECT version, principal_id, workspace_id, status FROM ag_schema_version WHERE id = 1", &[])?;
        let row = rows
            .first()
            .filter(|_| rows.len() == 1)
            .ok_or_else(|| fail("session schema not ready"))?;
        let workspace = integer(&row[2])?;
        let id = bicdb_workspace::WorkspaceId::from_raw(workspace)
            .ok_or_else(|| fail("invalid workspace"))?;
        if integer(&row[0])? != 1
            || integer(&row[1])? != user
            || text(&row[3])? != "ready"
            || bicdb_workspace::workspace_ref(id) != self.ws
        {
            return Err(fail("schema identity/workspace mismatch"));
        }
        let rows = self.attachment_read(
            "SELECT version, status FROM ag_attachment_schema WHERE id = 1",
            &[],
        )?;
        if rows.len() != 1 || integer(&rows[0][0])? != 1 || text(&rows[0][1])? != "ready" {
            return Err(fail("attachment schema not ready"));
        }
        Ok((user, workspace))
    }

    pub(crate) fn attachment_virtual_rows(
        &mut self,
        name: &str,
        arguments: Option<&[TableArgument]>,
        snapshot: CommitSeq,
    ) -> Result<Vec<Vec<Value>>, SessionError> {
        let mut inputs = Vec::new();
        if name == "attachment_grep" {
            for arg in arguments.ok_or_else(|| fail("missing grep arguments"))? {
                let value = match arg {
                    TableArgument::Literal(bytes) => Value::Bytes(bytes.clone()),
                    TableArgument::Parameter(index) => self
                        .exec_params
                        .get(*index)
                        .cloned()
                        .ok_or_else(|| fail("missing parameter"))?,
                };
                inputs.push(text(&value)?.to_owned());
            }
            if inputs.is_empty()
                || inputs.len() > 3
                || inputs[0].is_empty()
                || inputs[0].len() > 512
                || inputs[0].contains('\0')
                || inputs.iter().skip(1).any(|input| input.len() > 128)
            {
                return Err(fail("pattern must be 1..512 UTF-8 bytes; optional session/attachment IDs at most 128 bytes"));
            }
        }
        let (user, workspace) = self.attachment_scope()?;
        let mut sql = format!("SELECT {META} FROM ag_attachment a JOIN ag_session s ON a.sid = s.sid WHERE a.user_id = :user AND a.workspace_id = :workspace");
        let mut parameters = vec![("user", number(user)), ("workspace", number(workspace))];
        if let Some(sid) = inputs.get(1).filter(|sid| !sid.is_empty()) {
            sql.push_str(" AND a.sid = :sid");
            parameters.push(("sid", bytes(sid)));
        }
        if let Some(aid) = inputs.get(2).filter(|aid| !aid.is_empty()) {
            sql.push_str(" AND a.aid = :aid");
            parameters.push(("aid", bytes(aid)));
        }
        sql.push_str(" ORDER BY a.aid LIMIT 10001");
        let rows = self.attachment_read(&sql, &parameters)?;
        if rows.len() > 10000 {
            return Err(fail(
                "directory exceeds 10000 attachments; narrow the scope",
            ));
        }
        let mut output = Vec::new();
        let started = Instant::now();
        let mut files = 0;
        let mut scanned = 0;
        for mut row in rows {
            let can_search = eligible(&row)?;
            if text(&row[4])?.is_empty() {
                row[4] = Value::Null;
            }
            row.push(Value::Bool(can_search));
            row.push(bytes(if can_search {
                "literal-scan"
            } else {
                "unsupported-format"
            }));
            if name == "attachment$" {
                output.push(row);
                continue;
            }
            if !can_search {
                if inputs.get(2).is_some_and(|id| !id.is_empty()) {
                    return Err(fail("selected attachment format is not supported; PDF/OCR and archive extraction are not implemented"));
                }
                continue;
            }
            files += 1;
            let size = integer(&row[11])?;
            scanned += size;
            if files > MAX_FILES || size > MAX_BYTES || scanned > MAX_BYTES {
                return Err(fail("scan budget exceeded (256 files / 256 MiB); specify session_id and attachment_id"));
            }
            let matches =
                self.attachment_scan(&row, inputs[0].as_bytes(), started, MAX_HITS - output.len())?;
            for (line, offset, snippet) in matches {
                let mut hit = row.clone();
                hit.extend([
                    number(line),
                    number(offset),
                    bytes(&snippet),
                    number(snapshot.as_raw()),
                ]);
                output.push(hit);
            }
        }
        Ok(output)
    }

    fn attachment_scan(
        &mut self,
        metadata: &[Value],
        pattern: &[u8],
        started: Instant,
        remaining: usize,
    ) -> Result<Vec<(u64, u64, String)>, SessionError> {
        let id = text(&metadata[0])?;
        let size = integer(&metadata[11])?;
        let mut sha = Sha256::new();
        let mut carry = Vec::new();
        let mut utf8_tail = Vec::new();
        let mut total_lines = 1u64;
        let mut total = 0u64;
        let mut hits = Vec::new();
        for ordinal in 0..size.div_ceil(CHUNK) {
            if started.elapsed() > Duration::from_secs(10) {
                return Err(fail(
                    "scan exceeded 10 seconds; narrow the scope (no partial results returned)",
                ));
            }
            let rows = self.attachment_read(
                "SELECT aid, ordinal, data FROM ag_attachment_chunk WHERE ckey = :key",
                &[("key", bytes(&chunk_key(id, ordinal)))],
            )?;
            let row = rows
                .first()
                .filter(|_| rows.len() == 1)
                .ok_or_else(|| fail("missing/duplicate attachment chunk"))?;
            let data = match &row[2] {
                Value::Bytes(data) => data,
                _ => return Err(fail("invalid attachment chunk bytes")),
            };
            if text(&row[0])? != id
                || integer(&row[1])? != ordinal
                || data.len() as u64 != (size - ordinal * CHUNK).min(CHUNK)
            {
                return Err(fail("attachment chunk identity/length mismatch"));
            }
            sha.update(data);
            utf8_tail.extend_from_slice(data);
            match std::str::from_utf8(&utf8_tail) {
                Ok(_) => utf8_tail.clear(),
                Err(error) if error.error_len().is_none() => { utf8_tail = utf8_tail[error.valid_up_to()..].to_vec(); },
                Err(_) => return Err(fail("selected text-format attachment contains invalid UTF-8; binary/other encodings are not searchable")),
            }
            if data.contains(&0) {
                return Err(fail(
                    "selected text-format attachment contains binary NUL bytes",
                ));
            }
            let start = total - carry.len() as u64;
            let mut line = total_lines - carry.iter().filter(|byte| **byte == b'\n').count() as u64;
            let mut window = std::mem::take(&mut carry);
            window.extend_from_slice(data);
            for position in 0..window.len() {
                if position + pattern.len() <= window.len()
                    && start + position as u64 + pattern.len() as u64 > total
                    && window[position..].starts_with(pattern)
                {
                    if hits.len() >= remaining {
                        return Err(fail("more than 1000 matches; narrow the scope (no partial results returned)"));
                    }
                    let from = position.saturating_sub(80);
                    let to = (position + pattern.len() + 80).min(window.len());
                    hits.push((line, start + position as u64, snippet(&window[from..to])?));
                }
                if window[position] == b'\n' {
                    line += 1;
                }
            }
            total_lines += data.iter().filter(|byte| **byte == b'\n').count() as u64;
            total += data.len() as u64;
            let keep = (pattern.len() + 80).min(window.len());
            carry = window[window.len() - keep..].to_vec();
        }
        if !utf8_tail.is_empty() || total != size || hex(&sha.finalize()) != text(&metadata[12])? {
            return Err(fail(
                "attachment checksum/length/UTF-8 verification failed; no results returned",
            ));
        }
        Ok(hits)
    }
}
