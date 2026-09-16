// ============================================================================
// In-app chat 的 agent 凭证配置(Settings → Agent access 的落盘层)。
// 每个 ACP agent 一条:{api_key, base_url, extra_env},存
// `<config dir>/wake/agent-auth.json`(0600,明文本机盘——Wake 不碰
// agent 自己的凭证库,这里只放用户显式交给 Wake 的)。
// 注入路径:ChatPanel 每次启动会话时读盘 → acp::AcpSession::spawn 的
// envs;所以 Settings 里改完,下一次开面板/换 agent 即生效,无需重启。
// 通用出口:任意 Anthropic 兼容网关 = claude 的 base_url + api_key;
// 其它 provider(如 opencode 的模型)走 extra_env 的 NAME=VALUE 行。
// ============================================================================
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use wake_core::models::AgentId;
use wake_core::services::acp;

/// key = agent.as_str();BTreeMap 让落盘文件按 agent 名稳定排序
pub type AgentAuthMap = BTreeMap<String, AgentAuthEntry>;

#[derive(Serialize, Deserialize, Default, Clone, Debug)]
pub struct AgentAuthEntry {
    #[serde(default)]
    pub api_key: String,
    #[serde(default)]
    pub base_url: String,
    /// 额外环境变量,一行一条 NAME=VALUE(# 开头的行跳过)
    #[serde(default)]
    pub extra_env: String,
}

const FILE: &str = "agent-auth.json";

pub fn load() -> AgentAuthMap {
    crate::prefs::read(FILE)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

pub fn save(map: &AgentAuthMap) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec_pretty(map).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    crate::prefs::write(FILE, &bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = crate::prefs::dir().map(|d| d.join(FILE));
        if let Some(path) = path {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

/// 该 agent 这次启动要注入的环境变量(每次启动会话现读盘)。
/// extra_env 最后注入:同名变量覆盖默认映射,用户说最后一句话
pub fn env_for(agent: AgentId) -> Vec<(String, String)> {
    let map = load();
    env_for_map(agent, &map)
}

/// env_for 的纯函数核(测试不经用户盘)
pub(crate) fn env_for_map(agent: AgentId, map: &AgentAuthMap) -> Vec<(String, String)> {
    let Some(dialect) = acp::acp_dialect(agent) else {
        return Vec::new();
    };
    let entry = map.get(agent.as_str());
    let mut env = Vec::new();
    if let Some(entry) = entry {
        let key = entry.api_key.trim();
        let base = entry.base_url.trim();
        if let (Some(name), true) = (dialect.auth_key_env, !key.is_empty()) {
            env.push((name.to_string(), key.to_string()));
        }
        if let (Some(name), true) = (dialect.auth_base_env, !base.is_empty()) {
            env.push((name.to_string(), base.to_string()));
        }
        for line in entry.extra_env.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some((name, value)) = line.split_once('=') {
                let name = name.trim();
                if !name.is_empty() {
                    env.push((name.to_string(), value.trim().to_string()));
                }
            }
        }
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 空配置 = 无环境变量
    #[test]
    fn env_for_empty_map_is_empty() {
        assert!(env_for_map(AgentId::ClaudeCode, &AgentAuthMap::new()).is_empty());
    }

    /// key/端点按方言表映射进各自的环境变量名;空白值不注入
    #[test]
    fn key_and_base_url_map_to_dialect_env_names() {
        let mut map = AgentAuthMap::new();
        map.insert(
            AgentId::ClaudeCode.as_str().to_string(),
            AgentAuthEntry {
                api_key: " sk-anthropic ".to_string(),
                base_url: "https://api.moonshot.ai/anthropic".to_string(),
                extra_env: String::new(),
            },
        );
        assert_eq!(
            env_for_map(AgentId::ClaudeCode, &map),
            vec![
                ("ANTHROPIC_API_KEY".to_string(), "sk-anthropic".to_string()),
                (
                    "ANTHROPIC_BASE_URL".to_string(),
                    "https://api.moonshot.ai/anthropic".to_string()
                ),
            ]
        );
        // 登录制 agent:就算误填了 key 也没有映射目标,不注入
        map.insert(
            AgentId::Cursor.as_str().to_string(),
            AgentAuthEntry {
                api_key: "whatever".to_string(),
                base_url: String::new(),
                extra_env: String::new(),
            },
        );
        assert!(env_for_map(AgentId::Cursor, &map).is_empty());
    }

    /// extra_env 解析:NAME=VALUE 切分、跳过空行与注释、trim 两端、
    /// 无等号的行整行跳过
    #[test]
    fn extra_env_lines_parse() {
        let mut map = AgentAuthMap::new();
        map.insert(
            AgentId::Opencode.as_str().to_string(),
            AgentAuthEntry {
                api_key: String::new(),
                base_url: String::new(),
                extra_env: "OPENROUTER_API_KEY=sk-or-v1 abc\n  # comment\n\nDEEPSEEK_BASE_URL = https://api.deepseek.com\nbroken line\n".to_string(),
            },
        );
        assert_eq!(
            env_for_map(AgentId::Opencode, &map),
            vec![
                ("OPENROUTER_API_KEY".to_string(), "sk-or-v1 abc".to_string()),
                (
                    "DEEPSEEK_BASE_URL".to_string(),
                    "https://api.deepseek.com".to_string()
                ),
            ]
        );
    }
}
