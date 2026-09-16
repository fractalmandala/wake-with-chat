use super::claude::parse_claude_jsonl;
use super::parse_utils::*;
use super::{units_from_messages, AgentAdapter};
use crate::models::*;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Claude Desktop 的 Cowork(local-agent-mode-sessions)。每个 Cowork 会话在
/// 自己的沙盒里带一棵 `.claude/projects/`,布局与 Claude Code 的家目录同构:
///
///   local-agent-mode-sessions/<workspaceId>/<taskId>/<localId>/
///     └── .claude/projects/<编码cwd>/<uuid>.jsonl
///
/// 数据行是 claude-format JSONL,解析核心直接复用 claude::parse_claude_jsonl。
/// 会话 cwd 落在沙盒内(local_…/outputs)或是虚拟路径(/sessions/<slug>),
/// 不指向真实项目目录:沙盒内路径不进 project(避免整组叫 outputs),
/// /sessions/<slug> 的 slug 是 Cowork 自己起的会话名,保留作项目名。
pub struct CoworkAdapter {
    root: PathBuf,
}

impl CoworkAdapter {
    pub fn new() -> Self {
        let root = super::home_dir()
            .unwrap_or_default()
            .join("Library/Application Support/Claude/local-agent-mode-sessions");
        Self { root }
    }

    /// 会话 cwd 是否落在 Cowork 沙盒内。沙盒路径对本机无意义,不当项目
    fn is_sandbox_cwd(cwd: &str) -> bool {
        cwd.contains("/local-agent-mode-sessions/")
    }
}

/// 项目路径归一:沙盒 cwd 置空,/sessions/<slug> 是 Cowork 的会话命名,保留
fn project_path_of(cwd: &str) -> String {
    if CoworkAdapter::is_sandbox_cwd(cwd) {
        return String::new();
    }
    cwd.to_string()
}

fn build_meta(r: &SessionFileRef, p: &super::claude::ParseResult) -> SessionMeta {
    let project_path = project_path_of(&p.cwd);
    SessionMeta {
        key: format!("cowork:{}", r.native_id),
        host: String::new(),
        id: r.native_id.clone(),
        agent: AgentId::Cowork,
        title: p.title.clone(),
        project_path: project_path.clone(),
        project_name: project_name_of(&project_path),
        file_path: r.file_path.clone(),
        created_at: if p.created_at > 0 {
            p.created_at
        } else {
            r.mtime_ms
        },
        updated_at: if p.updated_at > 0 {
            p.updated_at
        } else {
            r.mtime_ms
        },
        message_count: p
            .messages
            .iter()
            .filter(|m| m.kind == MessageKind::Text)
            .count() as i64,
        size_bytes: r.size,
        git_branch: p.git_branch.clone(),
        model: p.model.clone(),
        tokens_used: if p.tokens_used > 0 {
            Some(p.tokens_used)
        } else {
            None
        },
        archived: false,
        source: None,
        favorite: false,
        pinned: false,
    }
}

impl AgentAdapter for CoworkAdapter {
    fn agent(&self) -> AgentId {
        AgentId::Cowork
    }

    fn list_session_files(&self) -> Result<Vec<SessionFileRef>> {
        // 沙盒树深且含 uploads/skills 等杂项;只认 .claude/projects 子树里的
        // JSONL,其余边车一律不收
        let mut refs = Vec::new();
        for entry in jsonl_entries(&self.root) {
            let path = entry.path();
            let owns = path
                .ancestors()
                .any(|a| a.file_name().is_some_and(|n| n == "projects"))
                && path
                    .ancestors()
                    .any(|a| a.file_name().is_some_and(|n| n == ".claude"));
            if !owns {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() == 0 {
                continue;
            }
            refs.push(SessionFileRef {
                agent: AgentId::Cowork,
                native_id: entry
                    .file_name()
                    .to_string_lossy()
                    .strip_suffix(".jsonl")
                    .unwrap_or_default()
                    .to_string(),
                file_path: path.to_string_lossy().to_string(),
                mtime_ms: mtime_ms(&meta),
                size: meta.len() as i64,
            });
        }
        Ok(refs)
    }

    fn file_ref(&self, path: &Path) -> Option<SessionFileRef> {
        // Cowork 沙盒里的会话主文件必经 .claude/projects;tasks/、skills 等
        // 其他 JSONL 不是会话
        if !path
            .ancestors()
            .any(|a| a.file_name().is_some_and(|n| n == ".claude"))
        {
            return None;
        }
        default_file_ref(self.agent(), path)
    }

    fn parse_session(&self, r: &SessionFileRef) -> Result<ParsedSession> {
        let parsed = parse_claude_jsonl(Path::new(&r.file_path), false, false)?;
        let meta = build_meta(r, &parsed);
        let units = units_from_messages(&parsed.messages);
        Ok(ParsedSession {
            meta,
            units,
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn parse_transcript(&self, r: &SessionFileRef) -> Result<ParsedTranscript> {
        let parsed = parse_claude_jsonl(Path::new(&r.file_path), false, true)?;
        Ok(ParsedTranscript {
            meta: build_meta(r, &parsed),
            mainline: parsed.messages,
            sidechains: Vec::new(),
            unknown_line_count: parsed.unknown_lines,
        })
    }

    fn with_custom_root(&self, dir: PathBuf) -> Box<dyn AgentAdapter> {
        // 选中 Application Support/Claude 形态(含 local-agent-mode-sessions/)
        // 或直接选中该目录都认
        let nested = dir.join("local-agent-mode-sessions");
        let root = if nested.is_dir() {
            nested
        } else {
            dir
        };
        Box::new(Self { root })
    }

    fn data_roots(&self) -> Vec<PathBuf> {
        vec![self.root.clone()]
    }
}
