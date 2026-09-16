//! 跨 agent 接力(Continue with…):把当前会话导出成一份"交接稿"Markdown,
//! 写进 `~/.wake/handoffs/`,再在目标 agent 的 CLI 里以初始 prompt 拉起交互
//! 模式——目标 agent 读稿拿到上下文,从原会话停下的地方继续。与 resume
//! (同 agent 原地续)互补:resume 复用会话自身的存储,handoff 是**新会话 +
//! 文件形式的上下文**,不依赖目标 agent 认得源 agent 的会话格式。
//!
//! 交互式初始 prompt 形制逐家实测(help 输出为据,2026-09):
//! - 位置参数:claude `[prompt]`、codex `[PROMPT]`、gemini `[query]`、
//!   grok `[PROMPT]`、cursor-agent `[prompt...]` ——一键直开
//! - agy `-i`(--prompt-interactive)——一键直开
//! - kimi / opencode 实测无交互式初始 prompt 形制(-p/--print 与
//!   `run` 都是非交互);其余未验证形制的 agent 一律走兜底:起 TUI、
//!   prompt 进剪贴板,用户粘一次即接上。兜底不依赖各家 CLI 演进,
//!   永不变成死菜单项

use crate::adapters::AgentAdapter;
use crate::models::{AgentId, SessionMeta};
use anyhow::Context as _;
use std::path::{Path, PathBuf};

use super::{exporter, terminal};

/// 目标 agent 的交互式初始 prompt 形制
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptForm {
    /// `cli "<prompt>"`——位置参数给初始 prompt 并进交互模式
    Arg,
    /// `cli -i "<prompt>"`(antigravity 的 --prompt-interactive)
    InteractiveFlag,
    /// CLI 无交互式初始 prompt 形制:起交互 TUI,prompt 进剪贴板,用户粘贴
    ClipboardOnly,
}

/// 一个可作接力目标的 agent 及其 prompt 形制。"能不能接力"只看 CLI 在不在
/// PATH(resolve 有缓存,菜单每次展开重查不付代价);装好 CLI 无需重启
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandoffTarget {
    pub agent: AgentId,
    pub form: PromptForm,
}

/// 逐家实测过的交互式初始 prompt 形制;没实测过的落 ClipboardOnly 兜底
fn prompt_form(agent: AgentId) -> PromptForm {
    match agent {
        AgentId::ClaudeCode
        | AgentId::Codex
        | AgentId::Gemini
        | AgentId::Grok
        | AgentId::Cursor => PromptForm::Arg,
        AgentId::Antigravity => PromptForm::InteractiveFlag,
        _ => PromptForm::ClipboardOnly,
    }
}

/// 接力目标清单:除源 agent 外,凡 CLI 在 PATH 上的 agent 都列(WorkBuddy/
/// Cowork 无 CLI 自然出局)。目标开的是**新会话**,resume 形制不参与判断
/// ——OpenClaw 这类 resume 不可行的 agent 照样能接别人的上下文
pub fn handoff_targets(exclude: AgentId) -> Vec<HandoffTarget> {
    AgentId::ALL
        .iter()
        .copied()
        .filter(|a| *a != exclude)
        .filter(|a| terminal::agent_bin(*a).is_some())
        .filter(|a| terminal::cli_path(*a).is_some())
        .map(|agent| HandoffTarget {
            agent,
            form: prompt_form(agent),
        })
        .collect()
}

// ---------------------------------------------------------------- 交接稿文档

/// 交接稿字符预算。截头保尾:开头是原始任务,结尾是"从哪继续",都保;
/// 中段超预算整块省略并打标记。400KB 对主流 CLI 的上下文是"一次读完还
/// 剩得出干活余量"的量级
const HANDOFF_BUDGET: usize = 400 * 1024;

/// 交接稿的初始 prompt:短句带文件绝对路径,正文交给目标 agent 自己读
/// (prompt 走 argv,长度受限;文件不受限)
pub fn handoff_prompt(path: &Path) -> String {
    format!(
        "Read {} — it is the transcript of a prior agent conversation you are \
taking over. Continue that work from where it left off.",
        path.display()
    )
}

/// 导出并写交接稿到 `~/.wake/handoffs/`,返回文件路径。文件名与导出同构
/// (agent-标题-日期),前缀 handoff- 与用户自己的导出区分开
pub fn write_handoff(adapter: &dyn AgentAdapter, meta: &SessionMeta) -> anyhow::Result<PathBuf> {
    let dir = dirs::home_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join(".wake")
        .join("handoffs");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create handoff dir {}", dir.display()))?;
    let path = dir.join(format!(
        "handoff-{}",
        exporter::default_file_name(meta, "md")
    ));
    std::fs::write(&path, handoff_doc(adapter, meta)?)
        .with_context(|| format!("cannot write handoff file {}", path.display()))?;
    Ok(path)
}

/// 交接稿全文 = 使用说明题头 + 会话转录(超预算截头保尾)
fn handoff_doc(adapter: &dyn AgentAdapter, meta: &SessionMeta) -> anyhow::Result<String> {
    let body = exporter::render_markdown(adapter, meta)?;
    let project = if meta.project_path.is_empty() {
        "no recorded project directory".to_string()
    } else {
        format!("project `{}`", meta.project_path)
    };
    let mut out = format!(
        "# Handoff — continue this work\n\n\
         > You are taking over a conversation previously run by **{}** in {}.\n\
         > The full transcript follows: read it, then continue the work from \
         where it left off — the latest request at the end is the task.\n\
         > A middle section may be marked omitted when the transcript exceeded \
         the handoff size budget.

---

",
        meta.agent.display_name(),
        project
    );
    out.push_str(&truncate_transcript(&body, HANDOFF_BUDGET));
    Ok(out)
}

/// 转录超预算时的截头保尾:块边界取 `\n### `(每条消息的标题行,ASCII,
/// 字节下标必在字符边界上)。首块前的部分是导出器自己的题头(会话元信息),
/// 恒保留。子会话段(`## ⑂`)粘在相邻消息块里,随块取舍——粗粒度即可,
/// 标记里给的是省略条数,读者知道中间缺了什么
fn truncate_transcript(body: &str, budget: usize) -> String {
    if body.len() <= budget {
        return body.to_string();
    }
    let starts: Vec<usize> = body.match_indices("\n### ").map(|(i, _)| i + 1).collect();
    if starts.is_empty() {
        // 整篇没有消息块(退化转录):按字符边界硬切尾部
        return cut_tail(body, budget);
    }
    let head = &body[..starts[0]];
    let head_budget = budget / 5; // 头部五分之一,其余全给尾部(最近进展优先)
    let mut out = String::with_capacity(budget + 256);
    out.push_str(head);
    let mut taken = head.len();
    let mut first_kept = 0;
    for (n, &s) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(body.len());
        let block = &body[s..end];
        if taken + block.len() > head_budget {
            first_kept = n;
            break;
        }
        out.push_str(block);
        taken += block.len();
        first_kept = n + 1;
    }
    // 从尾部倒着收块
    let mut kept_tail: Vec<&str> = Vec::new();
    let mut tail_taken = 0;
    let mut first_from_end = starts.len();
    for n in (first_kept..starts.len()).rev() {
        let end = starts.get(n + 1).copied().unwrap_or(body.len());
        let block = &body[starts[n]..end];
        if tail_taken + block.len() > budget - head_budget {
            break;
        }
        tail_taken += block.len();
        first_from_end = n;
        kept_tail.push(block);
    }
    let omitted = first_from_end - first_kept;
    if omitted > 0 {
        out.push_str(&format!(
            "\n\n> ⋯ {omitted} messages omitted here (handoff size budget) ⋯\n\n"
        ));
        if kept_tail.is_empty() {
            // 连一个尾部块都放不下(最后一条消息本身超预算):保底给最后
            // 一块的尾部——cut_tail 保留的是字符串末段,"最新的请求"必在
            let last = &body[starts[starts.len() - 1]..];
            out.push_str(&cut_tail(last, budget - head_budget));
        }
    }
    kept_tail.reverse();
    for block in kept_tail {
        out.push_str(block);
    }
    out
}

/// 字符边界安全地从后往前截到 max 字节
fn cut_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n\n> ⋯ truncated (handoff size budget) ⋯\n", &s[..end])
}

// ---------------------------------------------------------------- 计划与执行

/// 一次接力拉起的全部材料:UI 层拿到即可启动,无后续回调
pub struct HandoffPlan {
    pub path: PathBuf,
    /// 交给目标 agent 的初始 prompt(Arg/InteractiveFlag 走 argv,
    /// ClipboardOnly 进剪贴板)
    pub prompt: String,
    /// 解析好的目标 CLI 绝对路径
    cli: String,
    args: Vec<String>,
    cwd: Option<String>,
}

/// 拼装接力启动件。cwd 取会话项目目录(在才 cd——目录消失时退化为
/// 在默认目录开新会话,上下文在稿子里,照样接得上)
fn build_plan(
    meta: &SessionMeta,
    target: HandoffTarget,
    path: &Path,
) -> anyhow::Result<HandoffPlan> {
    let cli = terminal::cli_path(target.agent).ok_or_else(|| {
        anyhow::anyhow!(
            "Agent CLI `{}` for {} was not found on PATH",
            terminal::agent_bin(target.agent).unwrap_or("?"),
            target.agent.display_name()
        )
    })?;
    let prompt = handoff_prompt(path);
    // dsh 的 bin 是 npx,包名必须作首参(resume 同款约束)
    let mut args = match target.agent {
        AgentId::Dsh => vec!["@deepseek-ai/dsh".to_string(), "web".to_string()],
        _ => Vec::new(),
    };
    match target.form {
        PromptForm::Arg => args.push(prompt.clone()),
        PromptForm::InteractiveFlag => {
            args.push("-i".to_string());
            args.push(prompt.clone());
        }
        PromptForm::ClipboardOnly => {}
    }
    let cwd = (!meta.project_path.is_empty() && Path::new(&meta.project_path).is_dir())
        .then(|| meta.project_path.clone());
    Ok(HandoffPlan {
        path: path.to_path_buf(),
        prompt,
        cli,
        args,
        cwd,
    })
}

/// 执行一次接力:写稿 → (兜底形制)prompt 进剪贴板 → 起终端。
/// 返回成功提示文案;任何一步失败整条报错。启动管线与 resume 共用
/// terminal::launch_in(深链层不参与:接力只发生在 CLI 型 agent 之间)
pub fn continue_with(
    adapter: &dyn AgentAdapter,
    meta: &SessionMeta,
    target: HandoffTarget,
    term: terminal::TerminalApp,
) -> Result<String, String> {
    let path = write_handoff(adapter, meta).map_err(|e| format!("Handoff export failed: {e}"))?;
    let plan =
        build_plan(meta, target, &path).map_err(|e| format!("Handoff launch failed: {e}"))?;
    let command = terminal::compose_in(term, &plan.cli, &plan.args, plan.cwd.as_deref());
    if target.form == PromptForm::ClipboardOnly && !terminal::copy_to_clipboard(&plan.prompt) {
        return Err(format!(
            "Couldn't copy the handoff prompt to the clipboard. It reads: {}",
            plan.prompt
        ));
    }
    terminal::launch_in(term, &plan.cli, &plan.args, plan.cwd.as_deref())
        .map_err(|e| format!("Couldn't open terminal ({e}). Run manually: {command}"))?;
    Ok(match target.form {
        PromptForm::ClipboardOnly => format!(
            "Handoff saved to {}. {} is open — paste the clipboard prompt to continue.",
            path.display(),
            target.agent.display_name()
        ),
        _ => format!(
            "Continuing with {}: {}",
            target.agent.display_name(),
            command
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// prompt_form 表:实测过的形制不许漂移
    #[test]
    fn prompt_form_matches_verified_clis() {
        assert_eq!(prompt_form(AgentId::ClaudeCode), PromptForm::Arg);
        assert_eq!(prompt_form(AgentId::Codex), PromptForm::Arg);
        assert_eq!(prompt_form(AgentId::Gemini), PromptForm::Arg);
        assert_eq!(prompt_form(AgentId::Grok), PromptForm::Arg);
        assert_eq!(prompt_form(AgentId::Cursor), PromptForm::Arg);
        assert_eq!(
            prompt_form(AgentId::Antigravity),
            PromptForm::InteractiveFlag
        );
        assert_eq!(prompt_form(AgentId::Kimi), PromptForm::ClipboardOnly);
        assert_eq!(prompt_form(AgentId::Opencode), PromptForm::ClipboardOnly);
        assert_eq!(prompt_form(AgentId::Dsh), PromptForm::ClipboardOnly);
    }

    fn block(n: usize, fill: &str) -> String {
        format!("### 👤 msg {n}\n\n{}\n\n", fill.repeat(40))
    }

    /// 超预算:题头保留、头尾块保留、中段省略带标记、总长不超预算太多
    #[test]
    fn truncate_keeps_head_and_tail_with_marker() {
        let mut body = String::from("# title\n\n> meta header\n\n---\n\n");
        for n in 0..100 {
            body.push_str(&block(n, "content "));
        }
        let out = truncate_transcript(&body, 8 * 1024);
        assert!(out.starts_with("# title"));
        assert!(out.contains("### 👤 msg 0")); // 原始任务在头部
        assert!(out.contains("### 👤 msg 99")); // 最近进展在尾部
        assert!(out.contains("messages omitted here"));
        assert!(out.len() < 8 * 1024 + 4096);
    }

    /// 预算内原样返回
    #[test]
    fn truncate_noop_under_budget() {
        let body = block(0, "small ");
        assert_eq!(truncate_transcript(&body, 8 * 1024), body);
    }

    /// 单块超预算(退化转录):字符边界安全截尾
    #[test]
    fn truncate_single_huge_block_cuts_tail() {
        let body = format!("# t\n\n### 👤 big\n\n{}\n", "x".repeat(100_000));
        let out = truncate_transcript(&body, 1024);
        assert!(out.contains("truncated"));
        assert!(out.len() < 2048);
    }

    /// 中文内容下块边界切分不出错(ASCII 边界必落在字符边界上)
    #[test]
    fn truncate_handles_multibyte_content() {
        let mut body = String::from("# 标题\n\n---\n\n");
        for n in 0..20 {
            body.push_str(&format!("### 👤 消息 {n}\n\n{}\n\n", "代码".repeat(50)));
        }
        let out = truncate_transcript(&body, 6 * 1024);
        assert!(out.contains("### 👤 消息 0"));
        assert!(out.contains("### 👤 消息 19"));
    }
}
