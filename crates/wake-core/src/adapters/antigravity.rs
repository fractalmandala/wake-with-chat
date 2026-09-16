use super::agy_proto;
use super::parse_utils::*;
use super::sqlite_ro::{open_sqlite_ro, virtual_path};
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Antigravity CLI(Google,binary `agy`):会话正文是加密 .pb,唯一明文是
/// `~/.gemini/antigravity-cli/conversation_summaries.db`(WAL)——只能做
/// 元数据级会话卡片:标题在 preview 列(title 列基本为空)、时间、workspace。
/// 详情页由一条 System 消息承载 preview 与"正文加密"说明,FTS 只搜得到它。
/// 无每会话文件,SessionFileRef 用虚拟路径;打开一律走 sqlite_ro 三级梯度。
pub struct AntigravityAdapter {
    db: PathBuf,
    /// 全表很小(元数据行),按 db mtime 缓存一轮扫描内的重复调用
    rows_cache: MtimeCache<Vec<AgRow>>,
}

impl AntigravityAdapter {
    pub fn new() -> Self {
        Self {
            db: super::home_dir()
                .unwrap_or_default()
                .join(".gemini")
                .join("antigravity-cli")
                .join("conversation_summaries.db"),
            rows_cache: MtimeCache::new(),
        }
    }

    fn rows(&self) -> Option<Vec<AgRow>> {
        let mtime = super::sqlite_ro::db_cache_stamp(&self.db);
        self.rows_cache.get_or_try_build(mtime, || {
            let ro = open_sqlite_ro(&self.db, "antigravity")?;
            let mut stmt = ro
                .conn
                .prepare(
                    "SELECT conversation_id, title, preview, step_count, last_modified_time, workspace_uris
                     FROM conversation_summaries
                     WHERE parent_conversation_id = '' AND nesting_depth = 0",
                )
                .ok()?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(AgRow {
                        id: r.get(0)?,
                        title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        preview: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        step_count: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        modified_ms: sqlite_dt_ms(r.get::<_, Option<String>>(4)?.unwrap_or_default().trim()),
                        cwd: first_workspace(&r.get::<_, Option<String>>(5)?.unwrap_or_default()),
                    })
                })
                .ok()?
                .collect::<rusqlite::Result<Vec<_>>>()
                .ok()?;
            Some(rows)
        })
    }

    fn build_meta(&self, r: &SessionFileRef, row: &AgRow) -> SessionMeta {
        let title = Some(clean_title_candidate(&row.title))
            .filter(|t| !t.is_empty())
            .or_else(|| Some(clean_title_candidate(&row.preview)).filter(|t| !t.is_empty()))
            .unwrap_or_else(|| UNTITLED.to_string());
        // 库里只有 last_modified 一个时间,created/updated 同源
        let ts = if row.modified_ms > 0 {
            row.modified_ms
        } else {
            r.mtime_ms
        };
        SessionMeta {
            key: format!("antigravity:{}", row.id),
            host: String::new(),
            id: row.id.clone(),
            agent: AgentId::Antigravity,
            title,
            project_path: row.cwd.clone(),
            project_name: project_name_of(&row.cwd),
            file_path: r.file_path.clone(),
            created_at: ts,
            updated_at: ts,
            message_count: row.step_count,
            size_bytes: r.size,
            git_branch: None,
            model: None,
            tokens_used: None,
            archived: false,
            source: None,
            favorite: false,
            pinned: false,
        }
    }

    fn parse(&self, r: &SessionFileRef) -> Result<(SessionMeta, Vec<TranscriptMessage>)> {
        let rows = self
            .rows()
            .ok_or_else(|| anyhow!("cannot open antigravity summaries store"))?;
        let row = rows
            .iter()
            .find(|x| x.id == r.native_id)
            .ok_or_else(|| anyhow!("antigravity conversation {} not in store", r.native_id))?;

        // 正文加密不可读:一条 System 消息承载 preview,详情页与 FTS 都有着落
        let mut text = String::new();
        if !row.preview.trim().is_empty() {
            text.push_str(row.preview.trim());
            text.push_str("\n\n");
        }
        text.push_str("Antigravity stores conversation content encrypted — only this summary is available in Wake.");
        let mut messages = vec![text_msg(Role::System, &text, row.modified_ms)];
        assign_seq(&mut messages);
        Ok((self.build_meta(r, row), messages))
    }
}

#[derive(Clone)]
struct AgRow {
    id: String,
    title: String,
    preview: String,
    step_count: i64,
    modified_ms: i64,
    cwd: String,
}

/// workspace_uris JSON 数组("[\"file:///Users/…\"]")首项 → 本地路径
fn first_workspace(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return String::new();
    };
    let Some(uri) = v
        .as_array()
        .and_then(|a| a.first())
        .and_then(|x| x.as_str())
    else {
        return String::new();
    };
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    percent_decode(path)
}

/// file:// URI 的最小 percent-decode(路径含空格/中文时是 %XX 编码)
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

impl AgentAdapter for AntigravityAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Antigravity
    }

    /// 同 conversation_id 在桌面端(conversations/*.db,可解正文)与 CLI 端
    /// (summaries,仅元数据)都出现时,桌面端的完整转录胜出
    fn dedup_rank(&self) -> u8 {
        1
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let Some(rows) = self.rows() else {
            return Ok(Vec::new());
        };
        Ok(rows
            .into_iter()
            .map(|row| SessionFileRef {
                agent: AgentId::Antigravity,
                native_id: row.id.clone(),
                file_path: virtual_path(&self.db, &row.id),
                mtime_ms: row.modified_ms,
                // 正文不可读,标题/preview 长度即内容指纹(dirty 判断用)
                size: (row.title.len() + row.preview.len()) as i64,
            })
            .collect())
    }

    fn quick_meta(&self, refs: &[SessionFileRef]) -> Option<HashMap<String, SessionMeta>> {
        let rows = self.rows()?;
        let by_id: HashMap<&str, &AgRow> = rows.iter().map(|r| (r.id.as_str(), r)).collect();
        let mut out = HashMap::new();
        for r in refs {
            if let Some(row) = by_id.get(r.native_id.as_str()) {
                out.insert(r.file_path.clone(), self.build_meta(r, row));
            }
        }
        Some(out)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (meta, messages) = self.parse(r)?;
        let units = units_from_messages(&messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: 0,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (meta, messages) = self.parse(r)?;
        Ok(ParsedTranscript {
            meta,
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count: 0,
        })
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 选中 `~/.gemini` 形态(含 antigravity-cli/)、库所在目录,或直接
        // 给到库文件路径都认(Codex review)
        let nested = dir
            .join("antigravity-cli")
            .join("conversation_summaries.db");
        let db = if dir.is_file() {
            dir
        } else if nested.is_file() {
            nested
        } else {
            dir.join("conversation_summaries.db")
        };
        Box::new(Self {
            db,
            rows_cache: MtimeCache::new(),
        })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.db.clone()]
    }
}

/// Antigravity 桌面端(`~/.gemini/antigravity/conversations/<uuid>.db`):
/// 一库一会话,`steps` 表每行一步,载荷是 protobuf(无官方 schema,走
/// agy_proto 的启发式解码)。step_type 14 = 用户输入,15 = planner 回复,
/// 其余 kind 的可展示字符串也可作为助手消息。gen_metadata 给模型与 token。
/// 与 CLI 端共用 AgentId::Antigravity:CLI 的 summaries 只有元数据,同 id
/// 相遇时由 dedup_rank 让位于本实例的完整转录。
pub struct AntigravityDesktopAdapter {
    root: PathBuf,
}

impl AntigravityDesktopAdapter {
    pub fn new() -> Self {
        Self {
            root: super::home_dir()
                .unwrap_or_default()
                .join(".gemini")
                .join("antigravity")
                .join("conversations"),
        }
    }

    /// 主库指纹:db 与 -wal 的 mtime/size 取最大——活动会话只写 WAL,
    /// 只看主库会漏更新
    fn fingerprint(db: &Path) -> (i64, i64) {
        let mut size = 0i64;
        let mut mtime = 0i64;
        for path in [db.to_path_buf(), PathBuf::from(format!("{}-wal", db.display()))] {
            if let Ok(meta) = std::fs::metadata(&path) {
                size += meta.len() as i64;
                mtime = mtime.max(mtime_ms(&meta));
            }
        }
        (mtime, size)
    }

    /// steps 表 → (消息, tokens, model)。解码失败的行静默丢弃(AV 同策略)
    fn parse_db(&self, db: &Path) -> Result<(Vec<TranscriptMessage>, i64, Option<String>)> {
        let ro = open_sqlite_ro(db, "antigravity")
            .ok_or_else(|| anyhow!("cannot open antigravity conversations db {}", db.display()))?;
        let mut steps: Vec<(i64, i64, Vec<u8>)> = Vec::new();
        {
            let mut stmt = ro
                .conn
                .prepare("SELECT idx, step_type, step_payload FROM steps ORDER BY idx")?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, Option<Vec<u8>>>(2)?.unwrap_or_default(),
                ))
            })?;
            for row in rows {
                steps.push(row?);
            }
        }

        let mut messages: Vec<TranscriptMessage> = Vec::new();
        // gen_metadata:模型 + token 用量。model 归到最后一条含模型名的行;
        // token 逐条累加(usage_tokens 同口径:三项之和)
        let mut tokens_used = 0i64;
        let mut model: Option<String> = None;
        if let Ok(mut stmt) = ro.conn.prepare("SELECT data FROM gen_metadata ORDER BY idx") {
            if let Ok(rows) = stmt.query_map([], |r| r.get::<_, Option<Vec<u8>>>(0)) {
                for row in rows.flatten().flatten() {
                    if let Some(block) = agy_proto::extract_generation_usage(&row) {
                        tokens_used += block.uncached_input + block.total_output + block.cache_read;
                        if !block.model.is_empty() {
                            model = Some(block.model);
                        }
                    }
                }
            }
        }

        for (idx, step_type, payload) in steps {
            if payload.is_empty() {
                continue;
            }
            let fields = agy_proto::parse(&payload);
            if fields.is_empty() {
                continue;
            }
            // kind 优先取载荷 field 1(CortexStepType),回退 step_type 列
            let kind = match agy_proto::find(&fields, 1) {
                Some(f) if f.wire == agy_proto::WIRE_VARINT => f.varint as i64,
                _ => step_type,
            };
            let is_user = kind == 14;
            let ts = agy_proto::earliest_timestamp_ms(&fields);
            if is_user {
                // 用户步:全量候选里挑最佳 prompt(路径/JSON/纯 ID 都是噪声)
                let candidates = agy_proto::step_strings(&fields, true);
                let best = agy_proto::best_user_prompt(&candidates)
                    .or_else(|| candidates.first().cloned());
                if let Some(text) = best {
                    messages.push(text_msg(Role::User, &text, ts));
                }
                continue;
            }
            let text = candidates_join(agy_proto::step_strings(&fields, false));
            let mut calls = Vec::new();
            for (name, id, input) in agy_proto::extract_tool_calls(idx, &fields) {
                calls.push(ToolCallView {
                    id,
                    name,
                    input_preview: input.as_deref().unwrap_or_default().to_string(),
                    input: input,
                    output: None,
                    is_error: false,
                    sidechain_ref: None,
                });
            }
            if text.is_empty() && calls.is_empty() {
                continue;
            }
            let (clipped, truncated) = clip(&text, MAX_MSG_TEXT);
            messages.push(TranscriptMessage {
                seq: 0,
                role: Role::Assistant,
                kind: MessageKind::Text,
                text: clipped,
                truncated,
                tool_calls: calls,
                thinking: None,
                timestamp: (ts > 0).then_some(ts),
                model: None,
                images: Vec::new(),
            });
        }
        assign_seq(&mut messages);
        Ok((messages, tokens_used, model))
    }

    fn build_meta(
        &self,
        r: &SessionFileRef,
        messages: &[TranscriptMessage],
        tokens_used: i64,
        model: &Option<String>,
    ) -> SessionMeta {
        let title = title_from_messages(messages).unwrap_or_else(|| UNTITLED.to_string());
        let msg_min = messages.iter().filter_map(|m| m.timestamp).min().unwrap_or(0);
        let msg_max = messages.iter().filter_map(|m| m.timestamp).max().unwrap_or(0);
        // 桌面端步骤载荷不携带项目路径(AV 同样留空),不猜
        SessionMeta {
            key: format!("antigravity:{}", r.native_id),
            host: String::new(),
            id: r.native_id.clone(),
            agent: AgentId::Antigravity,
            title,
            project_path: String::new(),
            project_name: "Unknown project".to_string(),
            file_path: r.file_path.clone(),
            created_at: if msg_min > 0 { msg_min } else { r.mtime_ms },
            updated_at: if msg_max > 0 { msg_max } else { r.mtime_ms },
            message_count: messages
                .iter()
                .filter(|m| m.kind == MessageKind::Text)
                .count() as i64,
            size_bytes: r.size,
            git_branch: None,
            model: model.clone(),
            tokens_used: (tokens_used > 0).then_some(tokens_used),
            archived: false,
            source: None,
            favorite: false,
            pinned: false,
        }
    }
}

/// 助手步的展示正文:字符串段之间双换行(与 agentsview 的 join("\n\n") 同形)
fn candidates_join(strs: Vec<String>) -> String {
    strs.join("\n\n")
}

impl AgentAdapter for AntigravityDesktopAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Antigravity
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        let mut refs = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(refs);
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            // <uuid>.db 为主库;-wal/-shm 是边车
            let Some(id) = name.strip_suffix(".db") else {
                continue;
            };
            if id.len() != 36 || !id.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-') {
                continue;
            }
            let (mtime, size) = Self::fingerprint(&path);
            if size == 0 {
                continue;
            }
            refs.push(SessionFileRef {
                agent: AgentId::Antigravity,
                native_id: id.to_string(),
                file_path: path.to_string_lossy().to_string(),
                mtime_ms: mtime,
                size,
            });
        }
        Ok(refs)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        let name = path.file_name()?.to_string_lossy();
        let id = name.strip_suffix(".db")?;
        if id.len() != 36 {
            return None;
        }
        let parent = path.parent()?.file_name()?.to_string_lossy();
        if parent != "conversations" {
            return None;
        }
        let (mtime, size) = Self::fingerprint(path);
        Some(SessionFileRef {
            agent: AgentId::Antigravity,
            native_id: id.to_string(),
            file_path: path.to_string_lossy().to_string(),
            mtime_ms: mtime,
            size,
        })
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let (messages, tokens, model) = self.parse_db(Path::new(&r.file_path))?;
        let meta = self.build_meta(r, &messages, tokens, &model);
        let units = units_from_messages(&messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: 0,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let (messages, tokens, model) = self.parse_db(Path::new(&r.file_path))?;
        let meta = self.build_meta(r, &messages, tokens, &model);
        Ok(ParsedTranscript {
            meta,
            mainline: messages,
            sidechains: Vec::new(),
            unknown_line_count: 0,
        })
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 选 ~/.gemini 形态(含 antigravity/)、conversations/ 本身,或某个
        // .db 文件都认
        let root = if dir.is_file() {
            dir.parent().unwrap_or(&dir).to_path_buf()
        } else if dir.file_name().is_some_and(|n| n == "conversations") {
            dir
        } else if dir.join("antigravity").join("conversations").is_dir() {
            dir.join("antigravity").join("conversations")
        } else if dir.join("conversations").is_dir() {
            dir.join("conversations")
        } else {
            dir.join("conversations")
        };
        Box::new(Self { root })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}
