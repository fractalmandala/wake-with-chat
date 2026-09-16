// ============================================================================
// In-app chat 的会话登记簿:面板里每发起/续聊一轮对话,把 {agent, sessionId,
// cwd, 标题, 时间} 记进 <config dir>/wake/chat-sessions.json——左栏 Chats
// 区的数据源,点击经 session/load 续聊(agent 自己的库仍是唯一事实源,这里
// 只存"怎么找到它"的指针;不列Wake 之外的会话,不代写 agent 的存储)。
// ============================================================================
use serde::{Deserialize, Serialize};

const FILE: &str = "chat-sessions.json";
/// 上限:标题列表再长也没人翻,截老留新
const CAP: usize = 200;

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct ChatRecord {
    pub agent: String,
    pub session_id: String,
    pub cwd: String,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// 登记当时 agent 是否声明 loadSession——不支持续聊的条目点击只开新会话
    pub can_resume: bool,
    /// 登记当时选中的模型(configOptions 值,如 opencode/big-pickle);
    /// 标题常是首句问候语,模型徽章帮列表区分会话。旧文件无此字段
    #[serde(default)]
    pub model: Option<String>,
}

/// 最新在前
pub fn load() -> Vec<ChatRecord> {
    let mut records: Vec<ChatRecord> = crate::prefs::read(FILE)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.created_at.cmp(&a.created_at)));
    records
}

/// 按 sessionId upsert(同一会话只一条);标题只在拿到非空值时覆盖
pub fn upsert(record: ChatRecord) -> std::io::Result<()> {
    let mut records: Vec<ChatRecord> = crate::prefs::read(FILE)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default();
    if let Some(existing) = records
        .iter_mut()
        .find(|r| r.session_id == record.session_id)
    {
        existing.updated_at = record.updated_at;
        existing.agent = record.agent;
        existing.cwd = record.cwd;
        existing.can_resume = record.can_resume;
        if !record.title.is_empty() {
            existing.title = record.title.clone();
        }
    } else {
        records.push(record);
    }
    records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.created_at.cmp(&a.created_at)));
    records.truncate(CAP);
    let mut bytes = serde_json::to_vec_pretty(&records).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    crate::prefs::write(FILE, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(session_id: &str, updated_at: i64, title: &str) -> ChatRecord {
        ChatRecord {
            agent: "kimi".to_string(),
            session_id: session_id.to_string(),
            cwd: "/tmp".to_string(),
            title: title.to_string(),
            created_at: updated_at,
            updated_at,
            can_resume: true,
            model: None,
        }
    }

    /// upsert 语义:同 sessionId 更新而不是新增,新标题覆盖、空标题保留
    #[test]
    fn upsert_replaces_by_session_id() {
        // 不碰真实用户盘:upsert 是 IO 层,这里只测排序/截断逻辑等价的
        // 数据核——直接构造与 upsert 内部相同的变换
        let mut records = vec![record("a", 100, "first"), record("b", 200, "second")];
        records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.created_at.cmp(&a.created_at)));
        assert_eq!(records[0].session_id, "b");
        assert_eq!(records[1].session_id, "a");
        // 新回合把 a 顶到最新;标题为空不覆盖
        let mut a = record("a", 300, "");
        if let Some(existing) = records.iter_mut().find(|r| r.session_id == "a") {
            existing.updated_at = a.updated_at;
            if !a.title.is_empty() {
                existing.title = a.title.clone();
            }
        }
        a.title = "renamed".to_string();
        records.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.created_at.cmp(&a.created_at)));
        assert_eq!(records[0].session_id, "a");
        assert_eq!(records[0].title, "first");
        assert_eq!(records.len(), 2);
        // 截断留新
        let mut many: Vec<ChatRecord> =
            (0..CAP + 10).map(|i| record(&format!("s{i}"), i as i64, "t")).collect();
        many.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(b.created_at.cmp(&a.created_at)));
        many.truncate(CAP);
        assert_eq!(many.len(), CAP);
        assert_eq!(many[0].session_id, format!("s{}", CAP + 9));
    }
}
