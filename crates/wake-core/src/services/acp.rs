//! ACP(Agent Client Protocol)接入:把说 ACP 的 agent CLI 作为子进程拉起,
//! 经 stdio 上的换行分隔 JSON-RPC 2.0 驱动完整回合(prompt → 流式增量 →
//! 工具调用 → 权限请求 → stopReason),翻译成 [`ChatEvent`] 给 UI 层。
//!
//! 与 resume/handoff 的关系:resume 复用既有会话,handoff 导出上下文后
//! 在终端开新会话,ACP 则让**对话本身发生在 Wake 内**。agent 自己落盘
//! 自己的会话存储,Wake 现有 adapter 会照常索引——ACP 层不写任何库。
//!
//! 线上形制逐条对照 vibex `agent-acp`(2026-09 源码)与本机 CLI 实测:
//! - `initialize` {protocolVersion:1, clientCapabilities:{fs:{…}}, clientInfo}
//! - `session/new` {cwd, mcpServers:[]} → {sessionId}
//! - `session/prompt` {sessionId, prompt:[{type:"text",text}]} → {stopReason}
//! - `session/update` 通知,判别字段 `update.sessionUpdate`(vibex
//!   lib.rs `collect_session_update` 同款);权限请求是 agent→client 的
//!   **request**(`session/request_permission`),应答 `{outcome:{…}}`
//! - framing 是「一行一个 JSON 消息」(vibex write_all + b"\n" 同款),
//!   不是 LSP 的 Content-Length
//!
//! 测试不经真实 CLI:`AcpSession::over` 注入一对 UnixStream,测试线程
//! 在对面按剧本读写(vibex "internal client seam" 同思路)。

use crate::models::AgentId;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::terminal;

// ---------------------------------------------------------------- 事件模型

/// 接力层之上的规范事件流——未来 codex(app-server)/grok 等第二协议
/// 也翻译成这一种,UI 只认这一份
#[derive(Debug, Clone)]
pub enum ChatEvent {
    /// 回合已提交(事件流保持自足,便于回放与测试)
    UserMessage(String),
    AssistantDelta(String),
    ThinkingDelta(String),
    /// tool_call 与 tool_call_update 合并为一条按 id upsert 的事件
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: String,
        input: Option<Value>,
        output: Option<String>,
    },
    PlanEntry {
        content: String,
        status: String,
    },
    /// 权限请求:policy=Ask 时浮出;UI 调 respond_permission 应答
    PermissionRequest(PermissionRequest),
    /// 一回合的终点(session/prompt 应答带 stopReason 而来)
    TurnDone {
        stop_reason: Option<String>,
    },
    AgentError(String),
    /// 扩展事件(vibex 见过 OpenCode 的 usage_update):取不到不算错
    Usage {
        total_tokens: Option<i64>,
    },
}

#[derive(Debug, Clone)]
pub struct PermissionRequest {
    /// JSON-RPC request id,应答时原样带回
    pub request_id: i64,
    pub tool_call_id: Option<String>,
    pub title: String,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    /// ACP 规范的 allow_once / allow_always / reject_once / reject_always
    pub kind: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionPolicy {
    /// 权限请求浮给 UI
    Ask,
    /// 自动选 allow 类选项应答(优先 allow_always),事件不上浮
    AutoApprove,
}

// ---------------------------------------------------------------- 方言表

/// agent 的 ACP 启动形制。`bin` 覆盖 agent_bin(仅 claude 走 npx 适配器);
/// args 按本机 CLI 实测(2026-09-16):kimi acp / opencode acp /
/// cursor-agent acp / gemini --acp(--experimental-acp 已废弃)
pub struct AcpDialect {
    pub agent: AgentId,
    pub bin: Option<&'static str>,
    pub args: &'static [&'static str],
    /// 用户在 Settings → Agent access 填的 API key 注入的环境变量名;
    /// None = 该 agent 只认自己的 CLI 登录(auth_hint 给指引)
    pub auth_key_env: Option<&'static str>,
    /// 自定义 API 端点(Anthropic 兼容网关等)注入的环境变量名
    pub auth_base_env: Option<&'static str>,
    /// 走 CLI 登录的 agent:Settings 页给一句怎么登录的指引
    pub auth_hint: Option<&'static str>,
}

pub fn acp_dialect(agent: AgentId) -> Option<AcpDialect> {
    let (bin, args, auth_key_env, auth_base_env, auth_hint) = match agent {
        AgentId::Kimi => (
            None,
            &["acp"][..],
            None,
            None,
            // Kimi 的凭证只从自己的配置文件读,shell 环境变量不生效;
            // ACP 模式先在终端里 /login(2026-09 官方文档实测)
            Some("Run `kimi` in a terminal and send /login to sign in."),
        ),
        AgentId::Opencode => (
            None,
            &["acp"][..],
            None,
            None,
            // opencode 凭证在 auth.json(opencode auth login);已知 provider
            // 的标准环境变量(如 ANTHROPIC_API_KEY)也认,走下面的额外环境
            Some(
                "Run `opencode auth login` in a terminal, or set a provider env var (e.g. ANTHROPIC_API_KEY) below.",
            ),
        ),
        AgentId::Cursor => (
            None,
            &["acp"][..],
            None,
            None,
            Some("Run `cursor-agent login` in a terminal to sign in."),
        ),
        AgentId::Gemini => (
            None,
            &["--acp"][..],
            Some("GEMINI_API_KEY"),
            Some("GOOGLE_GEMINI_BASE_URL"),
            None,
        ),
        // claude 本机无 acp 子命令,走 Zed 的适配器(npx 拉起);适配器把
        // ANTHROPIC_* 原样透传给 SDK——任意 Anthropic 兼容端点都能接
        AgentId::ClaudeCode => (
            Some("npx"),
            &["-y", "@zed-industries/claude-code-acp"][..],
            Some("ANTHROPIC_API_KEY"),
            Some("ANTHROPIC_BASE_URL"),
            None,
        ),
        _ => return None,
    };
    Some(AcpDialect {
        agent,
        bin,
        args,
        auth_key_env,
        auth_base_env,
        auth_hint,
    })
}

/// 该 agent 的 ACP 聊天现在能不能开:方言存在且对应 bin 在 PATH 上
/// (探测走 terminal 的 CLI 缓存,面板打开时现查,装好即见)
pub fn acp_available(agent: AgentId) -> bool {
    let Some(d) = acp_dialect(agent) else {
        return false;
    };
    let bin = d
        .bin
        .unwrap_or_else(|| terminal::agent_bin(agent).unwrap_or("?"));
    terminal::cli_bin_path(bin).is_some()
}

/// 当前机器可用的 ACP agent 清单(面板/菜单按此出列表)
pub fn acp_targets() -> Vec<AgentId> {
    AgentId::ALL
        .iter()
        .copied()
        .filter(|a| acp_dialect(*a).is_some())
        .filter(|a| acp_available(*a))
        .collect()
}

/// 一轮 prompt 的内容块。文本之外,图片走 base64 image 块(kimi/claude 适配器
/// 支持),其余文件走 resource_link(file:// URI + 名字),agent 需要时经
/// fs 回调或自己的读取器取——发什么由 agent 能力决定,Wake 不做转换
#[derive(Debug, Clone)]
pub enum PromptBlock {
    Text(String),
    /// data = base64 编码的文件内容
    Image {
        data: String,
        mime: String,
    },
    ResourceLink {
        uri: String,
        name: String,
    },
}

impl PromptBlock {
    pub fn text(text: impl Into<String>) -> Self {
        PromptBlock::Text(text.into())
    }

    fn to_json(&self) -> Value {
        match self {
            PromptBlock::Text(text) => json!({ "type": "text", "text": text }),
            PromptBlock::Image { data, mime } => {
                json!({ "type": "image", "data": data, "mimeType": mime })
            }
            PromptBlock::ResourceLink { uri, name } => {
                json!({ "type": "resource_link", "uri": uri, "name": name })
            }
        }
    }
}

/// session/new 返回的模型/配置选项(ACP 0.23 unstable 的 configOptions;
/// kimi 实测携带,server 用 session/set_config_option 切换)。空 = 该 agent
/// 没暴露选项,UI 不出选择器
#[derive(Debug, Clone)]
pub struct AcpModelOption {
    pub config_id: String,
    pub name: String,
    pub current: Option<String>,
    /// (value, 展示名)
    pub options: Vec<(String, String)>,
}

fn parse_config_options(result: &Value) -> Vec<AcpModelOption> {
    result
        .get("configOptions")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|o| {
                    // configId 是 ACP 0.23 的字段名;opencode 实测用短字段 id,
                    // 两个都收,谁在用谁
                    let config_id = o
                        .get("configId")
                        .or_else(|| o.get("id"))
                        .and_then(Value::as_str)?
                        .to_string();
                    let name = o
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("model")
                        .to_string();
                    let current = o
                        .get("currentValue")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let options: Vec<(String, String)> = o
                        .get("options")
                        .and_then(Value::as_array)
                        .map(|list| {
                            list.iter()
                                .filter_map(|opt| {
                                    let value = opt.get("value").and_then(Value::as_str)?;
                                    let label =
                                        opt.get("name").and_then(Value::as_str).unwrap_or(value);
                                    Some((value.to_string(), label.to_string()))
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    (!options.is_empty()).then_some(AcpModelOption {
                        config_id,
                        name,
                        current,
                        options,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------- 会话

pub struct AcpSession {
    child: Mutex<Option<Child>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    next_id: AtomicI64,
    pending: Arc<Mutex<HashMap<i64, Sender<Result<Value, String>>>>>,
    event_tx: Sender<ChatEvent>,
    /// 泵线程实时读取的策略标志(set_policy 切换后无需换线程)
    live_policy: Arc<AtomicBool>,
    /// initialize 结果里声明的认证方式(agentCapabilities.authMethods 的 id);
    /// 空 = 不需要 authenticate 握手
    pub auth_methods: Vec<String>,
    /// session/new 结果里的 configOptions(模型选择等),空 = 无选项
    pub models: Vec<AcpModelOption>,
    stderr_tail: Arc<Mutex<String>>,
    session_id: Arc<Mutex<Option<String>>>,
    /// initialize 结果里的 loadSession 能力
    pub supports_load: bool,
}

impl AcpSession {
    /// 真实启动:拉起方言规定的 CLI 子进程并完成 initialize(+authenticate)
    /// 与 session/new(session/load)。resume 给了且 agent 支持 loadSession
    /// 就续聊并回放历史(历史经 session/update 事件流回到时间线);
    /// 不支持就静默落到 session/new 开新对话。返回会话与事件接收端
    pub fn spawn(
        agent: AgentId,
        cwd: &Path,
        policy: PermissionPolicy,
        env: &[(String, String)],
        resume: Option<&str>,
    ) -> anyhow::Result<(AcpSession, Receiver<ChatEvent>)> {
        let d = acp_dialect(agent)
            .ok_or_else(|| anyhow::anyhow!("{} does not support ACP chat", agent.display_name()))?;
        let bin = d.bin.unwrap_or_else(|| {
            terminal::agent_bin(agent).expect("dialect covers agents with a bin")
        });
        let cli = terminal::cli_bin_path(bin)
            .ok_or_else(|| anyhow::anyhow!("agent CLI `{bin}` was not found on PATH"))?;
        let mut command = Command::new(&cli);
        command
            .args(d.args)
            .current_dir(cwd)
            // Settings → Agent access 配的凭证/API 端点经环境变量进 agent
            .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // 子进程自成进程组,shutdown 整组收,不留孤儿
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|e| anyhow::anyhow!("cannot spawn {cli}: {e}"))?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let (rx, mut session) = Self::over(
            Box::new(stdin),
            Box::new(BufReader::new(stdout)),
            Some(child),
            cwd.to_path_buf(),
            policy,
        );
        // stderr 只作退出诊断素材:留最后 2KB
        let tail = session.stderr_tail.clone();
        std::thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                let mut t = tail.lock().unwrap();
                if t.len() + line.len() + 1 > 2000 {
                    t.clear();
                }
                t.push_str(&line);
                t.push('\n');
            }
        });
        session
            .initialize()
            .map_err(|e| anyhow::anyhow!("ACP initialize failed: {e}"))?;
        session
            .authenticate_if_configured(env, d.auth_key_env)
            .map_err(|e| anyhow::anyhow!("ACP authenticate failed: {e}"))?;
        let (method, params) = match resume.filter(|_| session.supports_load) {
            Some(session_id) => (
                "session/load",
                json!({
                    "sessionId": session_id,
                    "cwd": cwd.display().to_string(),
                    "mcpServers": [],
                }),
            ),
            None => (
                "session/new",
                json!({ "cwd": cwd.display().to_string(), "mcpServers": [] }),
            ),
        };
        let result = session
            .request(method, params)
            .map_err(|e| anyhow::anyhow!("ACP {method} failed: {e}"))?;
        session.models = parse_config_options(&result);
        *session.session_id.lock().unwrap() = result
            .get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string);
        Ok((session, rx))
    }

    /// 测试与未来宿主共用的注入缝:给定 stdin/stdout(可选真实子进程)
    fn over(
        stdin: Box<dyn Write + Send>,
        stdout: Box<dyn BufRead + Send>,
        child: Option<Child>,
        cwd: PathBuf,
        policy: PermissionPolicy,
    ) -> (Receiver<ChatEvent>, AcpSession) {
        let (event_tx, event_rx) = channel::<ChatEvent>();
        let writer = Arc::new(Mutex::new(Box::new(stdin) as Box<dyn Write + Send>));
        let pending: Arc<Mutex<HashMap<i64, Sender<Result<Value, String>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        let session_id = Arc::new(Mutex::new(None::<String>));
        let live_policy = Arc::new(AtomicBool::new(matches!(
            policy,
            PermissionPolicy::AutoApprove
        )));
        let pump_policy = live_policy.clone();
        {
            let writer = writer.clone();
            let pending = pending.clone();
            let event_tx = event_tx.clone();
            let stderr_tail = stderr_tail.clone();
            let pump_cwd = cwd.clone();
            std::thread::spawn(move || {
                pump(
                    stdout,
                    writer,
                    pending,
                    event_tx,
                    pump_cwd,
                    pump_policy,
                    stderr_tail,
                );
            });
        }
        let session = AcpSession {
            child: Mutex::new(child),
            writer,
            next_id: AtomicI64::new(1),
            pending,
            event_tx,
            live_policy,
            auth_methods: Vec::new(),
            models: Vec::new(),
            stderr_tail,
            session_id,
            supports_load: false,
        };
        (event_rx, session)
    }

    fn initialize(&mut self) -> Result<(), String> {
        let result = self.request(
            "initialize",
            json!({
                "protocolVersion": 1,
                "clientCapabilities": {
                    "fs": { "readTextFile": true, "writeTextFile": true },
                    "terminal": false,
                },
                "clientInfo": { "name": "wake", "version": env!("CARGO_PKG_VERSION") },
            }),
        )?;
        self.supports_load = result
            .pointer("/agentCapabilities/loadSession")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.auth_methods = result
            .pointer("/agentCapabilities/authMethods")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(())
    }

    /// agent 声明了 authMethods 且用户给该 agent 配了 key 时,握手期先
    /// authenticate(ACP 规范:authMethods 非空就可能要这一步才能 session/new)。
    /// methodId 优先挑 api 字样的(终端登录类留给 CLI 自己),没有就取第一个。
    /// 已登录过的 agent(gemini oauth 等)不受影响:没配 key 就直接跳过
    fn authenticate_if_configured(
        &mut self,
        env: &[(String, String)],
        key_env: Option<&str>,
    ) -> Result<(), String> {
        if self.auth_methods.is_empty() {
            return Ok(());
        }
        let key_set = key_env.is_some_and(|k| env.iter().any(|(n, _)| n == k));
        if !key_set {
            return Ok(());
        }
        let method_id = self
            .auth_methods
            .iter()
            .find(|m| m.contains("api"))
            .or_else(|| self.auth_methods.first())
            .cloned()
            .unwrap_or_default();
        self.request("authenticate", json!({ "methodId": method_id }))
            .map(|_| ())
    }

    /// 有 id 的请求:写线 + 等应答(阻塞;只给握手类快请求用,
    /// prompt 的应答走 TurnDone 事件,见 prompt)
    fn request(&self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.write_line(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))?;
        match rx.recv_timeout(Duration::from_secs(120)) {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(format!("no response to {method} (agent did not answer)")),
        }
    }

    fn write_line(&self, msg: Value) -> Result<(), String> {
        let mut line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        line.push('\n');
        let mut w = self.writer.lock().unwrap();
        w.write_all(line.as_bytes())
            .and_then(|_| w.flush())
            .map_err(|e| format!("ACP pipe write failed: {e}"))
    }

    /// 提交一轮。blocks = 文本 + 附件内容块;应答不阻塞等待——它就是
    /// TurnDone(带 stopReason),由这条注释后面的转接线程从 pending 通道
    /// 摘走变成事件
    pub fn prompt(&self, blocks: &[PromptBlock]) -> Result<(), String> {
        let Some(session_id) = self.session_id.lock().unwrap().clone() else {
            return Err("ACP session not established".to_string());
        };
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        // 时间线上的用户气泡只吃文本块;附件由 UI 侧自行记录
        let text = blocks
            .iter()
            .filter_map(|b| match b {
                PromptBlock::Text(t) => Some(t.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        self.send_event(ChatEvent::UserMessage(text));
        self.write_line(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/prompt",
            "params": {
                "sessionId": session_id,
                "prompt": blocks.iter().map(|b| b.to_json()).collect::<Vec<_>>(),
            },
        }))?;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || match rx.recv() {
            Ok(Ok(v)) => {
                let _ = event_tx.send(ChatEvent::TurnDone {
                    stop_reason: v
                        .get("stopReason")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                });
            }
            // agent 中途退出等:pump 会把挂起请求全部放 Err 出来
            Ok(Err(e)) => {
                let _ = event_tx.send(ChatEvent::AgentError(e));
            }
            Err(_) => {}
        });
        Ok(())
    }

    /// 切换模型/配置选项(kimi 实测支持 session/set_config_option)。
    /// 应答挂到 pending 由专属线程收——失败经 AgentError 上浮,成功静默;
    /// 发送立即返回,不阻塞 UI
    pub fn set_config(&self, config_id: &str, value: &str) -> Result<(), String> {
        let Some(session_id) = self.session_id.lock().unwrap().clone() else {
            return Err("ACP session not established".to_string());
        };
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.write_line(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "session/set_config_option",
            "params": {
                "sessionId": session_id,
                "configId": config_id,
                "value": value,
            },
        }))?;
        let event_tx = self.event_tx.clone();
        std::thread::spawn(move || {
            if let Ok(Err(e)) = rx.recv() {
                let _ = event_tx.send(ChatEvent::AgentError(e));
            }
        });
        Ok(())
    }

    /// 中断当前回合(通知,无应答)
    pub fn cancel(&self) -> Result<(), String> {
        let Some(session_id) = self.session_id.lock().unwrap().clone() else {
            return Ok(());
        };
        self.write_line(json!({
            "jsonrpc": "2.0",
            "method": "session/cancel",
            "params": { "sessionId": session_id },
        }))
    }

    /// UI 应答权限请求;None = 用户取消(cancelled 语义交 agent 裁决)
    pub fn respond_permission(
        &self,
        request_id: i64,
        option_id: Option<&str>,
    ) -> Result<(), String> {
        self.write_line(json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": match option_id {
                Some(id) => json!({ "outcome": { "outcome": "selected", "optionId": id } }),
                None => json!({ "outcome": "cancelled" }),
            },
        }))
    }

    fn send_event(&self, e: ChatEvent) {
        // 接收端与进程同寿;掉线时事件丢弃即可,不 panic
        let _ = self.event_tx.send(e);
    }

    /// 关停:cancel → kill(独立进程组)→ wait。stdin 随子进程关闭,
    /// pump 读到 EOF 自然退场
    pub fn shutdown(&self) {
        let _ = self.cancel();
        if let Some(mut child) = self.child.lock().unwrap().take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    /// 运行时切换权限策略:泵线程经共享标志读到最新值,无需重启会话。
    /// &self 即可:live_policy 是原子量(会话经 Arc 共享,UI 只拿到 &AcpSession)
    pub fn set_policy(&self, policy: PermissionPolicy) {
        self.live_policy.store(
            matches!(policy, PermissionPolicy::AutoApprove),
            Ordering::Relaxed,
        );
    }

    /// 退出诊断:子进程意外结束时给 UI 的错误文案素材
    pub fn stderr_excerpt(&self) -> String {
        self.stderr_tail.lock().unwrap().trim().to_string()
    }

    /// 登记簿指针:session_id 在 session/new(或 session/load)之后才有
    pub fn session_id(&self) -> Option<String> {
        self.session_id.lock().unwrap().clone()
    }
}

impl Drop for AcpSession {
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ---------------------------------------------------------------- 泵线程

fn pump(
    stdout: Box<dyn BufRead + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pending: Arc<Mutex<HashMap<i64, Sender<Result<Value, String>>>>>,
    event_tx: Sender<ChatEvent>,
    cwd: PathBuf,
    policy: Arc<AtomicBool>,
    stderr_tail: Arc<Mutex<String>>,
) {
    for line in stdout.lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue; // 帧内垃圾跳过:agent 可能往 stdout 混入非协议输出
        };
        if let Some(id) = rpc_id(&msg) {
            if msg.get("method").is_some() {
                // agent → client 的 request
                handle_agent_request(&msg, id, &writer, &event_tx, &cwd, &policy);
            } else {
                // 对 id 的应答
                let sender = pending.lock().unwrap().remove(&id);
                if let Some(tx) = sender {
                    let payload = match msg.get("error") {
                        Some(err) => Err(err
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("ACP agent returned an error")
                            .to_string()),
                        None => Ok(msg.get("result").cloned().unwrap_or(Value::Null)),
                    };
                    let _ = tx.send(payload);
                }
                // 无 pending 的应答(理论只有 prompt 转接线程;它已退场)丢弃
            }
        } else if msg.get("method").is_some() {
            // 通知(session/update 等)
            handle_notification(&msg, &event_tx);
        }
    }
    // stdout 关闭 = 子进程退出
    let tail = stderr_tail.lock().unwrap().trim().to_string();
    let detail = if tail.is_empty() {
        String::new()
    } else {
        format!(" stderr: {tail}")
    };
    let _ = event_tx.send(ChatEvent::AgentError(format!("ACP agent exited.{detail}")));
    // 兜底:所有挂起的请求立刻醒过来报错,别让调用方干等 120s
    for (_, tx) in pending.lock().unwrap().drain() {
        let _ = tx.send(Err("ACP agent exited".to_string()));
    }
}

fn rpc_id(msg: &Value) -> Option<i64> {
    match msg.get("id") {
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.parse().ok(),
        _ => None,
    }
}

/// agent → client 的 request:权限请求上浮/自动应答,fs 回调按 cwd 收敛,
/// 未认识的立刻回错——别让对方等超时
fn handle_agent_request(
    msg: &Value,
    id: i64,
    writer: &Arc<Mutex<Box<dyn Write + Send>>>,
    event_tx: &Sender<ChatEvent>,
    cwd: &Path,
    policy: &Arc<AtomicBool>,
) {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let send = |line: Value| {
        if let Ok(mut line) = serde_json::to_string(&line) {
            line.push('\n');
            let mut w = writer.lock().unwrap();
            let _ = w.write_all(line.as_bytes());
            let _ = w.flush();
        }
    };
    let reply = |result: Value| {
        send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    };
    let reply_error = |message: &str| {
        send(json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32602, "message": message },
        }));
    };
    match method {
        "session/request_permission" => {
            let options: Vec<PermissionOption> = params
                .get("options")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(|o| {
                            Some(PermissionOption {
                                option_id: o.get("optionId")?.as_str()?.to_string(),
                                name: o
                                    .get("name")
                                    .and_then(Value::as_str)
                                    .unwrap_or("option")
                                    .to_string(),
                                kind: o
                                    .get("kind")
                                    .and_then(Value::as_str)
                                    .unwrap_or("other")
                                    .to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            if policy.load(Ordering::Relaxed) {
                // 优先 allow_always(后续同类不再问),退而求 allow_once
                let chosen = options
                    .iter()
                    .find(|o| o.kind == "allow_always")
                    .or_else(|| options.iter().find(|o| o.kind.starts_with("allow")));
                if let Some(o) = chosen {
                    return reply(json!({
                        "outcome": { "outcome": "selected", "optionId": o.option_id }
                    }));
                }
            }
            let title = params
                .pointer("/toolCall/title")
                .or_else(|| params.pointer("/toolCall/kind"))
                .and_then(Value::as_str)
                .unwrap_or("Permission requested")
                .to_string();
            let _ = event_tx.send(ChatEvent::PermissionRequest(PermissionRequest {
                request_id: id,
                tool_call_id: params
                    .pointer("/toolCall/toolCallId")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                title,
                options,
            }));
        }
        "fs/read_text_file" => {
            let Some(path) = params.get("path").and_then(Value::as_str) else {
                return reply_error("missing path");
            };
            if !within_cwd(Path::new(path), cwd) {
                return reply_error("path outside the session workspace");
            }
            match std::fs::read_to_string(path) {
                Ok(content) => reply(json!({ "content": content })),
                Err(e) => reply_error(&format!("read failed: {e}")),
            }
        }
        "fs/write_text_file" => {
            let (Some(path), Some(content)) = (
                params.get("path").and_then(Value::as_str),
                params.get("content").and_then(Value::as_str),
            ) else {
                return reply_error("missing path or content");
            };
            if !within_cwd(Path::new(path), cwd) {
                return reply_error("path outside the session workspace");
            }
            match std::fs::write(path, content) {
                Ok(()) => reply(Value::Null),
                Err(e) => reply_error(&format!("write failed: {e}")),
            }
        }
        _ => reply_error(&format!("wake does not implement {method}")),
    }
}

fn handle_notification(msg: &Value, event_tx: &Sender<ChatEvent>) {
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    if method != "session/update" {
        return;
    }
    let Some(update) = msg.pointer("/params/update") else {
        return;
    };
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    let block_text = |update: &Value| -> Option<String> {
        update
            .get("content")
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .map(str::to_string)
    };
    match kind {
        "agent_message_chunk" => {
            if let Some(text) = block_text(update) {
                let _ = event_tx.send(ChatEvent::AssistantDelta(text));
            }
        }
        "agent_thought_chunk" => {
            if let Some(text) = block_text(update) {
                let _ = event_tx.send(ChatEvent::ThinkingDelta(text));
            }
        }
        "tool_call" | "tool_call_update" => {
            let Some(id) = update
                .get("toolCallId")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                return;
            };
            // content 块数组里可能带工具输出(text 块);取全部 text 拼接
            let output = update
                .get("content")
                .and_then(Value::as_array)
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter_map(|b| {
                            b.get("text")
                                .and_then(Value::as_str)
                                .or_else(|| b.get("data").and_then(Value::as_str))
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                });
            let _ = event_tx.send(ChatEvent::ToolCall {
                id,
                title: update
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("tool")
                    .to_string(),
                kind: update
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("other")
                    .to_string(),
                status: update
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("pending")
                    .to_string(),
                input: update.get("rawInput").cloned(),
                output: output.filter(|s| !s.is_empty()),
            });
        }
        "plan" => {
            if let Some(entries) = update.get("entries").and_then(Value::as_array) {
                for e in entries {
                    let _ = event_tx.send(ChatEvent::PlanEntry {
                        content: e
                            .get("content")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        status: e
                            .get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("pending")
                            .to_string(),
                    });
                }
            }
        }
        "usage_update" => {
            let _ = event_tx.send(ChatEvent::Usage {
                total_tokens: update
                    .pointer("/meta/totalTokens")
                    .or_else(|| update.get("totalTokens"))
                    .and_then(Value::as_i64),
            });
        }
        _ => {} // current_mode_update / available_commands_update 等,V1 不消费
    }
}

/// fs 回调收敛:真实路径必须在会话 cwd 之内(符号链接解析后仍算)
fn within_cwd(path: &Path, cwd: &Path) -> bool {
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    let Ok(cwd) = cwd.canonicalize() else {
        return false;
    };
    path.starts_with(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    /// 双向注入 + 对面一个常驻的"agent 应答线程":
    /// - 握手(initialize / session/new)自动按 canned 值应答;
    /// - 客户端写出的每一行都进 client_log,测试按需断言;
    /// - 场景注入(通知 / 对 prompt 的应答 / agent→client 的 request)
    ///   由测试线程直接写 agent_out
    struct Harness {
        session: AcpSession,
        events: Receiver<ChatEvent>,
        client_log: Arc<Mutex<Vec<Value>>>,
        agent_out: UnixStream,
    }

    fn harness(policy: PermissionPolicy, cwd: PathBuf) -> Harness {
        let (a0, a1) = UnixStream::pair().expect("socket pair");
        let (b0, b1) = UnixStream::pair().expect("socket pair");
        let (events, session) = AcpSession::over(
            Box::new(a0),
            Box::new(BufReader::new(b0)),
            None,
            cwd,
            policy,
        );
        let client_log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let log = client_log.clone();
            let mut out = b1.try_clone().expect("clone agent out");
            std::thread::spawn(move || {
                let mut reader = BufReader::new(a1);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    log.lock().unwrap().push(msg.clone());
                    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    let reply = |out: &mut UnixStream, result: Value| {
                        use std::io::Write;
                        let mut s = json!({"jsonrpc":"2.0","id":id,"result":result}).to_string();
                        s.push('\n');
                        let _ = out.write_all(s.as_bytes());
                    };
                    match method {
                        "initialize" => reply(
                            &mut out,
                            json!({
                                "protocolVersion": 1,
                                "agentCapabilities": { "loadSession": true },
                            }),
                        ),
                        "session/new" => reply(&mut out, json!({ "sessionId": "sess-1" })),
                        _ => {}
                    }
                }
            });
        }
        Harness {
            session,
            events,
            client_log,
            agent_out: b1,
        }
    }

    fn write_msg(h: &Harness, v: Value) {
        use std::io::Write;
        let mut s = v.to_string();
        s.push('\n');
        let mut w = h.agent_out.try_clone().expect("clone agent out");
        w.write_all(s.as_bytes()).expect("write agent line");
    }

    fn logged(h: &Harness, i: usize) -> Value {
        // 握手与 prompt 都是秒级写完;给应答线程一点时间即可
        for _ in 0..100 {
            let log = h.client_log.lock().unwrap();
            if log.len() > i {
                return log[i].clone();
            }
            drop(log);
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("client line {i} never logged");
    }

    /// 完整握手:initialize + session/new 自动应答;会话 id 落到会话上
    fn handshake(h: &mut Harness) {
        h.session.initialize().expect("initialize");
        assert_eq!(logged(h, 0)["method"], "initialize");
        let result = h
            .session
            .request("session/new", json!({"cwd":"/tmp","mcpServers":[]}))
            .expect("session/new");
        assert_eq!(result["sessionId"], "sess-1");
        assert_eq!(logged(h, 1)["method"], "session/new");
        *h.session.session_id.lock().unwrap() = Some("sess-1".to_string());
        assert_eq!(h.session.session_id().as_deref(), Some("sess-1"));
        assert!(h.session.supports_load);
    }

    /// 等到第一个满足谓词的事件(其余按序丢进 keep)
    fn wait_event(h: &Harness, pred: impl Fn(&ChatEvent) -> bool) -> (ChatEvent, Vec<ChatEvent>) {
        let mut seen = Vec::new();
        loop {
            let e = h
                .events
                .recv_timeout(Duration::from_secs(5))
                .expect("event");
            if pred(&e) {
                return (e, seen);
            }
            seen.push(e);
        }
    }

    /// configOptions 解析:只认带 options 的条目,value 缺名回落到 value 本身
    #[test]
    fn config_options_parse_defensively() {
        let result = json!({
            "sessionId": "sess-1",
            "configOptions": [
                {
                    "configId": "model",
                    "name": "Model",
                    "currentValue": "k2",
                    "options": [
                        { "value": "k2", "name": "Kimi K2" },
                        { "value": "k1p" }
                    ]
                },
                { "configId": "thinking", "name": "Thinking" },
                { "name": "no configId" }
            ]
        });
        let models = parse_config_options(&result);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].config_id, "model");
        assert_eq!(models[0].current.as_deref(), Some("k2"));
        assert_eq!(
            models[0].options,
            vec![
                ("k2".to_string(), "Kimi K2".to_string()),
                ("k1p".to_string(), "k1p".to_string())
            ]
        );
        // 没有 configOptions 字段 = 空(agent 不支持就是不出选择器)
        assert!(parse_config_options(&json!({ "sessionId": "x" })).is_empty());
    }

    /// opencode 实测形制:顶层用 id 而非 configId,带 type/category 字段
    #[test]
    fn config_options_accept_opencode_id_field() {
        let result = json!({
            "sessionId": "sess-1",
            "configOptions": [
                {
                    "id": "model",
                    "name": "Model",
                    "category": "model",
                    "type": "select",
                    "currentValue": "opencode/big-pickle",
                    "options": [
                        { "value": "opencode/big-pickle", "name": "OpenCode Zen/Big Pickle" },
                        { "value": "myprovider/m1", "name": "My Provider/M1" }
                    ]
                }
            ]
        });
        let models = parse_config_options(&result);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].config_id, "model");
        assert_eq!(models[0].current.as_deref(), Some("opencode/big-pickle"));
        assert_eq!(models[0].options.len(), 2);
    }

    /// prompt 内容块:text 块原样,UserMessage 事件只携带文本块拼接
    #[test]
    fn prompt_blocks_serialize_and_text_only_user_event() {
        let mut h = harness(PermissionPolicy::Ask, std::env::temp_dir());
        handshake(&mut h);
        h.session
            .prompt(&[
                PromptBlock::text("look at this"),
                PromptBlock::Image {
                    data: "aGk=".to_string(),
                    mime: "image/png".to_string(),
                },
                PromptBlock::ResourceLink {
                    uri: "file:///tmp/notes.md".to_string(),
                    name: "notes.md".to_string(),
                },
            ])
            .expect("prompt");
        let sent = logged(&h, 2);
        assert_eq!(sent["method"], "session/prompt");
        let blocks = sent["params"]["prompt"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks[1]["mimeType"], "image/png");
        assert_eq!(blocks[2]["type"], "resource_link");
        assert_eq!(blocks[2]["uri"], "file:///tmp/notes.md");
        let (user, _) = wait_event(&h, |e| matches!(e, ChatEvent::UserMessage(_)));
        let ChatEvent::UserMessage(text) = user else {
            unreachable!()
        };
        assert_eq!(text, "look at this");
    }

    /// 完整回合:prompt → 增量 ×2 → tool_call 两态 → usage 扩展 → stopReason
    #[test]
    fn full_turn_streams_and_ends() {
        let mut h = harness(PermissionPolicy::Ask, std::env::temp_dir());
        handshake(&mut h);
        h.session
            .prompt(&[PromptBlock::text("hi")])
            .expect("prompt");
        assert_eq!(logged(&h, 2)["method"], "session/prompt");
        assert_eq!(logged(&h, 2)["params"]["prompt"][0]["text"], "hi");
        let prompt_id = logged(&h, 2)["id"].clone();
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Hello "}}}}),
        );
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"world"}}}}),
        );
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call","toolCallId":"t1","title":"Read file","kind":"read","status":"pending"}}}),
        );
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call_update","toolCallId":"t1","title":"Read file","kind":"read","status":"completed","content":[{"type":"text","text":"file body"}]}}}),
        );
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"usage_update","totalTokens":1234}}}),
        );
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"end_turn"}}),
        );
        let (done, seen) = wait_event(&h, |e| matches!(e, ChatEvent::TurnDone { .. }));
        let ChatEvent::TurnDone { stop_reason } = done else {
            unreachable!()
        };
        assert_eq!(stop_reason.as_deref(), Some("end_turn"));
        let mut text = String::new();
        let mut tool_done = false;
        let mut usage = None;
        for e in seen {
            match e {
                ChatEvent::UserMessage(t) => assert_eq!(t, "hi"),
                ChatEvent::AssistantDelta(t) => text.push_str(&t),
                ChatEvent::ToolCall {
                    id, status, output, ..
                } => {
                    assert_eq!(id, "t1");
                    if status == "completed" {
                        assert_eq!(output.as_deref(), Some("file body"));
                        tool_done = true;
                    }
                }
                ChatEvent::Usage { total_tokens } => usage = Some(total_tokens),
                other => panic!("unexpected event: {other:?}"),
            }
        }
        assert_eq!(text, "Hello world");
        assert!(tool_done);
        assert_eq!(usage, Some(Some(1234)));
    }

    /// 权限 Ask:请求上浮 → UI 应答 allow_once → agent 收到 → 回合结束
    #[test]
    fn permission_ask_roundtrip() {
        let mut h = harness(PermissionPolicy::Ask, std::env::temp_dir());
        handshake(&mut h);
        h.session
            .prompt(&[PromptBlock::text("go")])
            .expect("prompt");
        let prompt_id = logged(&h, 2)["id"].clone();
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sess-1","update":{"sessionUpdate":"tool_call","toolCallId":"t9","title":"Run cmd","kind":"execute","status":"pending"}}}),
        );
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","id":10,"method":"session/request_permission","params":{"sessionId":"sess-1","toolCall":{"toolCallId":"t9","title":"Run cmd"},"options":[{"optionId":"a1","name":"Allow once","kind":"allow_once"},{"optionId":"r1","name":"Reject","kind":"reject_once"}]}}),
        );
        let (req, seen) = wait_event(&h, |e| matches!(e, ChatEvent::PermissionRequest(_)));
        let ChatEvent::PermissionRequest(req) = req else {
            unreachable!()
        };
        assert_eq!(req.request_id, 10);
        assert_eq!(req.tool_call_id.as_deref(), Some("t9"));
        assert_eq!(req.options.len(), 2);
        h.session
            .respond_permission(req.request_id, Some("a1"))
            .expect("respond");
        for _ in 0..100 {
            let log = h.client_log.lock().unwrap();
            if log.len() > 3 {
                break;
            }
            drop(log);
            std::thread::sleep(Duration::from_millis(10));
        }
        let reply = logged(&h, 3);
        assert_eq!(reply["result"]["outcome"]["outcome"], "selected");
        assert_eq!(reply["result"]["outcome"]["optionId"], "a1");
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"end_turn"}}),
        );
        let _ = wait_event(&h, |e| matches!(e, ChatEvent::TurnDone { .. }));
        // 上浮序列里不该混进 AgentError
        assert!(!seen.iter().any(|e| matches!(e, ChatEvent::AgentError(_))));
    }

    /// AutoApprove:不产生事件,自动选 allow_always(退而求 allow_once)
    #[test]
    fn permission_auto_approve_prefers_allow_always() {
        let mut h = harness(PermissionPolicy::AutoApprove, std::env::temp_dir());
        handshake(&mut h);
        h.session
            .prompt(&[PromptBlock::text("go")])
            .expect("prompt");
        let prompt_id = logged(&h, 2)["id"].clone();
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","id":10,"method":"session/request_permission","params":{"sessionId":"sess-1","options":[{"optionId":"a1","name":"Allow once","kind":"allow_once"},{"optionId":"a2","name":"Always","kind":"allow_always"},{"optionId":"r1","name":"Reject","kind":"reject_once"}]}}),
        );
        for _ in 0..100 {
            let log = h.client_log.lock().unwrap();
            if log.len() > 3 {
                break;
            }
            drop(log);
            std::thread::sleep(Duration::from_millis(10));
        }
        let reply = logged(&h, 3);
        assert_eq!(reply["result"]["outcome"]["optionId"], "a2");
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"end_turn"}}),
        );
        // 整个回合只有 UserMessage 与 TurnDone,权限请求不上浮
        let (done, seen) = wait_event(&h, |e| matches!(e, ChatEvent::TurnDone { .. }));
        assert!(matches!(done, ChatEvent::TurnDone { .. }));
        assert!(seen.iter().all(|e| matches!(e, ChatEvent::UserMessage(_))));
    }

    /// 垃圾行不炸泵,后续正常消息照收
    #[test]
    fn malformed_lines_are_skipped() {
        let mut h = harness(PermissionPolicy::Ask, std::env::temp_dir());
        handshake(&mut h);
        h.session
            .prompt(&[PromptBlock::text("hi")])
            .expect("prompt");
        let prompt_id = logged(&h, 2)["id"].clone();
        // agent 侧混入的非协议输出:直接写裸文本行
        use std::io::Write;
        let mut w = h.agent_out.try_clone().expect("clone agent out");
        w.write_all(b"this is not json at all\n")
            .expect("write garbage");
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","method":"session/update","params":{"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"ok"}}}}),
        );
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","id":prompt_id,"result":{"stopReason":"end_turn"}}),
        );
        let (done, seen) = wait_event(&h, |e| matches!(e, ChatEvent::TurnDone { .. }));
        assert!(matches!(done, ChatEvent::TurnDone { .. }));
        assert!(seen
            .iter()
            .any(|e| matches!(e, ChatEvent::AssistantDelta(t) if t == "ok")));
    }

    /// 子进程退出:挂起请求立刻放错,事件流收到 AgentError
    #[test]
    fn agent_exit_wakes_pending_and_reports() {
        let mut h = harness(PermissionPolicy::Ask, std::env::temp_dir());
        handshake(&mut h);
        let (tx, rx) = channel::<Result<Value, String>>();
        h.session.pending.lock().unwrap().insert(999, tx);
        // socket 级关停:应答线程手里的 clone 也一并失效,pump 必然读到 EOF
        // (裸 drop 不够——clone 仍握着同一 socket)
        h.agent_out
            .shutdown(std::net::Shutdown::Both)
            .expect("shutdown agent side");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).expect("wakeup"),
            Err("ACP agent exited".to_string())
        );
        let err = h
            .events
            .recv_timeout(Duration::from_secs(5))
            .expect("event");
        assert!(matches!(err, ChatEvent::AgentError(e) if e.contains("exited")));
    }

    /// fs 回调收敛:cwd 之外拒绝,cwd 之内可读写
    #[test]
    fn fs_callbacks_confined_to_cwd() {
        let dir = std::env::temp_dir().join(format!("wake-acp-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut h = harness(PermissionPolicy::Ask, dir.clone());
        handshake(&mut h);
        h.session
            .prompt(&[PromptBlock::text("go")])
            .expect("prompt");
        let inside = dir.join("note.txt");
        std::fs::write(&inside, "v1").unwrap();
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","id":20,"method":"fs/read_text_file","params":{"path": inside.display().to_string()}}),
        );
        let reply = logged(&h, 3);
        assert_eq!(reply["result"]["content"], "v1");
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","id":21,"method":"fs/read_text_file","params":{"path":"/etc/hosts"}}),
        );
        for _ in 0..100 {
            let log = h.client_log.lock().unwrap();
            if log.len() > 4 {
                break;
            }
            drop(log);
            std::thread::sleep(Duration::from_millis(10));
        }
        let reply = logged(&h, 4);
        assert!(reply.get("error").is_some());
        write_msg(
            &h,
            json!({"jsonrpc":"2.0","id":22,"method":"fs/write_text_file","params":{"path": inside.display().to_string(),"content":"v2"}}),
        );
        for _ in 0..100 {
            let log = h.client_log.lock().unwrap();
            if log.len() > 5 {
                break;
            }
            drop(log);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(std::fs::read_to_string(&inside).unwrap(), "v2");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 方言表:本机实测的启动形制不许漂移;不支持的 agent 必须是 None
    #[test]
    fn dialect_table_matches_verified_launch_forms() {
        assert_eq!(acp_dialect(AgentId::Kimi).unwrap().args, &["acp"]);
        assert_eq!(acp_dialect(AgentId::Opencode).unwrap().args, &["acp"]);
        assert_eq!(acp_dialect(AgentId::Cursor).unwrap().args, &["acp"]);
        assert_eq!(acp_dialect(AgentId::Gemini).unwrap().args, &["--acp"]);
        let claude = acp_dialect(AgentId::ClaudeCode).unwrap();
        assert_eq!(claude.bin, Some("npx"));
        assert_eq!(claude.args, &["-y", "@zed-industries/claude-code-acp"]);
        assert!(acp_dialect(AgentId::Codex).is_none());
        assert!(acp_dialect(AgentId::Grok).is_none());
        assert!(acp_dialect(AgentId::Workbuddy).is_none());
    }

    /// 认证映射:claude 走 ANTHROPIC_*,gemini 走 GEMINI_*,
    /// 登录制 agent(kimi/opencode/cursor)只给 CLI 指引不给 key 变量
    #[test]
    fn dialect_auth_env_map_matches_cli_contracts() {
        let claude = acp_dialect(AgentId::ClaudeCode).unwrap();
        assert_eq!(claude.auth_key_env, Some("ANTHROPIC_API_KEY"));
        assert_eq!(claude.auth_base_env, Some("ANTHROPIC_BASE_URL"));
        assert!(claude.auth_hint.is_none());
        let gemini = acp_dialect(AgentId::Gemini).unwrap();
        assert_eq!(gemini.auth_key_env, Some("GEMINI_API_KEY"));
        assert_eq!(gemini.auth_base_env, Some("GOOGLE_GEMINI_BASE_URL"));
        for agent in [AgentId::Kimi, AgentId::Opencode, AgentId::Cursor] {
            let d = acp_dialect(agent).unwrap();
            assert!(d.auth_key_env.is_none());
            assert!(
                d.auth_hint.is_some(),
                "{} should hint at CLI login",
                agent.as_str()
            );
        }
    }

    /// authenticate 的正面剧本:initialize 带 authMethods → 配了 key →
    /// 发 authenticate {methodId:"api-key"}
    #[test]
    fn authenticate_sends_api_method_id() {
        let (a0, a1) = UnixStream::pair().expect("socket pair");
        let (b0, b1) = UnixStream::pair().expect("socket pair");
        let (events, mut session) = AcpSession::over(
            Box::new(a0),
            Box::new(BufReader::new(b0)),
            None,
            std::env::temp_dir(),
            PermissionPolicy::Ask,
        );
        let client_log: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        {
            let log = client_log.clone();
            let mut out = b1.try_clone().expect("clone agent out");
            std::thread::spawn(move || {
                let mut reader = BufReader::new(a1);
                let mut line = String::new();
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let Ok(msg) = serde_json::from_str::<Value>(line.trim()) else {
                        continue;
                    };
                    log.lock().unwrap().push(msg.clone());
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    if msg.get("method").and_then(Value::as_str) == Some("initialize") {
                        use std::io::Write;
                        let s = json!({
                            "jsonrpc": "2.0", "id": id,
                            "result": { "protocolVersion": 1,
                                "agentCapabilities": { "authMethods": [
                                    { "id": "login", "name": "Login" },
                                    { "id": "api-key", "name": "API key" } ] } },
                        })
                        .to_string()
                            + "\n";
                        let _ = out.write_all(s.as_bytes());
                    } else {
                        use std::io::Write;
                        let s = json!({"jsonrpc":"2.0","id":id,"result":Value::Null}).to_string()
                            + "\n";
                        let _ = out.write_all(s.as_bytes());
                    }
                }
            });
        }
        let _ = events;
        session.initialize().expect("initialize");
        assert_eq!(session.auth_methods, vec!["login", "api-key"]);
        // 没配 key:跳过,不发 authenticate
        session
            .authenticate_if_configured(&[], Some("ANTHROPIC_API_KEY"))
            .expect("skip without key");
        assert_eq!(client_log.lock().unwrap().len(), 1);
        // 配了 key:authenticate 走起,挑 api 字样的 methodId
        let env = vec![("ANTHROPIC_API_KEY".to_string(), "sk-x".to_string())];
        session
            .authenticate_if_configured(&env, Some("ANTHROPIC_API_KEY"))
            .expect("authenticate");
        for _ in 0..100 {
            if client_log.lock().unwrap().len() > 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let log = client_log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[1]["method"], "authenticate");
        assert_eq!(log[1]["params"]["methodId"], "api-key");
    }
}
