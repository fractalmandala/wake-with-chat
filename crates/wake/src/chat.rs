// ============================================================================
// In-app chat(ACP):第四栏右侧 Chat 面板。
// 数据层在 wake-core::services::acp(通用 ACP 客户端 + 方言表);本文件只管
// UI:一个 ChatPanel entity 持有会话与时间线,子进程 IO 全部离开主线程——
// 启动会话走 std::thread,事件经 futures channel 回主线程批量上屏
//(与 Workbench 的 scan 事件泵同一形制)。
// 时间线只存内存;agent 自己写自己的会话库,由既有 adapter 索引进 Library。
// 渲染复用详情页管线:assistant 走 markdown_body,thinking 走折叠面板,
// 连续工具调用聚成 tool_cluster——同一条视觉语言,不是两套组件。
// ============================================================================
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::StreamExt;
use gpui::prelude::FluentBuilder as _;
use gpui::*;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_component::popover::Popover;
use gpui_component::spinner::Spinner;
use gpui_component::{
    h_flex, v_flex, ActiveTheme as _, Disableable as _, Icon, Sizable as _, StyledExt as _,
};

use wake_core::models::{AgentId, SessionMeta, ToolCallView};
use wake_core::services::acp::{self, AcpSession, ChatEvent, PermissionPolicy};

use crate::format::thousands;
use crate::i18n::t;
use crate::theme::agent_series_color;
use crate::ui::*;
use crate::workbench::{markdown_body, thinking_panel, tool_cluster};

// ---------------------------------------------------------------- 面板宽度

/// 面板宽度左缘拖拽可调(把手覆在 border 上),持久化在 prefs;过窄/过宽夹住
const CHAT_WIDTH_PREF: &str = "chat-panel-width";
const CHAT_WIDTH_MIN: f32 = 320.;
const CHAT_WIDTH_MAX: f32 = 720.;
const CHAT_WIDTH_DEFAULT: f32 = 440.;
/// 图片附件上限:再大就降级成 resource_link,不往 prompt 里塞巨块 base64
const ATTACH_IMAGE_MAX_BYTES: usize = 8 * 1024 * 1024;

fn load_width() -> Pixels {
    px(
        crate::prefs::read(CHAT_WIDTH_PREF)
            .and_then(|text| text.trim().parse::<f32>().ok())
            .map(|w| w.clamp(CHAT_WIDTH_MIN, CHAT_WIDTH_MAX))
            .unwrap_or(CHAT_WIDTH_DEFAULT),
    )
}

// ---------------------------------------------------------------- 时间线模型

/// 时间线条目。Tool 直接存 ToolCallView:tool_cluster 的入参模型,
/// tool_call 与 tool_call_update 按 id upsert 成一条
enum ChatItem {
    User {
        text: String,
        /// 随这条消息发出的附件名(仅展示;内容块已在 prompt 里)
        attachments: Vec<String>,
    },
    Assistant(String),
    Thinking(String),
    Tool(ToolCallView),
    Error(String),
}

/// 待发送附件:name 进气泡/标签,block 是真正进 prompt 的内容块
struct PendingAttachment {
    name: String,
    block: acp::PromptBlock,
}

/// 左缘拖拽把手的 ghost payload:只做拖拽类型匹配。真正的起点/起宽在
/// mouse-down 时记进 panel 字段——on_drag 构造器的 position 是相对把手
/// 原点的偏移而非全局光标位,active_drag.value 也永远是静态载荷,都不能
/// 用来算拖拽位移
#[derive(Clone)]
struct ChatResize;

impl Render for ChatResize {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// 泵线程 → UI 的事件:会话就绪(成败)+ ACP 事件流
enum ChatUiEvent {
    Ready(Result<Arc<AcpSession>, String>),
    Event(ChatEvent),
}

/// ChatPanel → Workbench 的交互事件(面板自己不持有关闭权)
pub(crate) enum ChatPanelEvent {
    Close,
    /// 错误卡里的 Open Settings:面板保持打开,由 Workbench 拉起设置窗
    OpenSettings,
    /// 登记簿刚写过盘,左栏 Chats 区重新加载
    RegistryDirty,
}

// ---------------------------------------------------------------- 面板

pub(crate) struct ChatPanel {
    agent: AgentId,
    cwd: PathBuf,
    policy: PermissionPolicy,
    session: Option<Arc<AcpSession>>,
    items: Vec<ChatItem>,
    composer: Entity<TextareaState>,
    /// markdown 链接解析用的会话壳(host 空,project_path = 会话 cwd);
    /// assistant 正文渲染与详情页共用同一条链接管线
    meta: SessionMeta,
    /// 折叠态:thinking 面板与工具簇按时间线索引记,默认收起
    expanded_thinking: HashSet<usize>,
    expanded_tools: HashSet<usize>,
    /// policy=Ask 时浮出的待应答请求;应答后清空
    pending_permission: Option<acp::PermissionRequest>,
    /// 一回合进行中:Send 变 Stop
    in_flight: bool,
    connecting: bool,
    /// 启动失败(agent 拉不起来/握手失败)的一栏式提示
    start_error: Option<String>,
    /// 最近一次 usage_update 的累计 token;没有扩展事件的 agent 不显示
    last_usage: Option<i64>,
    /// 面板宽(左缘把手可拖),saved_width 是上次落盘值(节流基准)
    width: Pixels,
    saved_width: Pixels,
    /// 拖拽中的按下态:全局按下 x 与当时面板宽(mouse-down 记,mouse-up 清)
    resize_press: Option<(Pixels, Pixels)>,
    /// 待发送附件;send 时并入 prompt 内容块并清空
    pending_attachments: Vec<PendingAttachment>,
    /// send 侧预置的附件名,UserMessage 事件到达时挂到气泡上
    pending_names: Vec<String>,
    /// 续聊目标:Some(session_id) 时 spawn 走 session/load 回放历史
    resume: Option<String>,
    /// 登记簿里是否已有本会话条目(标题只取首条用户消息)
    registered: bool,
    /// 模型选择(config_id → value):握手值初始化,set 后本地更新显示
    model_choices: Vec<(String, String)>,
    /// 模型选择浮层的搜索框(浮层常开常关,实体常驻免重建)
    model_search: Entity<InputState>,
    _subs: Vec<Subscription>,
}

impl EventEmitter<ChatPanelEvent> for ChatPanel {}

impl ChatPanel {
    /// 打开面板:立即出 UI,子进程在后台线程拉起(阻塞式握手不占主线程)。
    /// cwd 取打开时详情页会话的项目目录(没有就退回 HOME)——agent 在哪
    /// 个工作区干活由这里定,面板内不换
    pub(crate) fn open(
        agent: AgentId,
        cwd: PathBuf,
        resume: Option<String>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        cx.new(|cx| Self::new(agent, cwd, resume, window, cx))
    }

    fn new(
        agent: AgentId,
        cwd: PathBuf,
        resume: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let composer = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(1, 8)
                .submit_on_enter(true)
                .placeholder(t("Ask the agent — Enter to send, Shift+Enter for newline"))
        });
        // ⏎ 发送 / ⇧⏎ 换行:submit_on_enter 已把语义放进输入状态,这里只消费
        // 提交事件(与 gpui-component 的 chat input 示例同一形制)
        let sub = cx.subscribe_in(&composer, window, |this, input, event, window, cx| {
            if let InputEvent::PressEnter { shift: false, .. } = event {
                this.send(input, window, cx);
            }
        });
        let mut panel = Self {
            // markdown 链接解析用的会话壳:只填 agent 与 project_path,
            // 其余字段对渲染无意义(host 空 = 本地链接解析)
            meta: SessionMeta {
                key: format!("chat:{}", agent.as_str()),
                id: String::new(),
                host: String::new(),
                agent,
                title: String::new(),
                project_path: cwd.display().to_string(),
                project_name: String::new(),
                file_path: String::new(),
                created_at: 0,
                updated_at: 0,
                message_count: 0,
                size_bytes: 0,
                git_branch: None,
                model: None,
                tokens_used: None,
                archived: false,
                source: None,
                favorite: false,
                pinned: false,
            },
            agent,
            cwd,
            policy: PermissionPolicy::Ask,
            session: None,
            items: Vec::new(),
            composer,
            expanded_thinking: HashSet::new(),
            expanded_tools: HashSet::new(),
            pending_permission: None,
            in_flight: false,
            connecting: true,
            start_error: None,
            last_usage: None,
            width: load_width(),
            saved_width: load_width(),
            resize_press: None,
            pending_attachments: Vec::new(),
            pending_names: Vec::new(),
            resume,
            registered: false,
            model_choices: Vec::new(),
            model_search: cx.new(|cx| {
                InputState::new(window, cx).placeholder(t("Search models"))
            }),
            _subs: vec![sub],
        };
        panel.start_session(cx);
        panel
    }

    /// 后台线程拉起子进程并完成握手,事件经 unbounded channel 回主线程。
    /// 泵与 panel 同生命周期:panel 被丢弃后 update 失败,循环退出
    fn start_session(&mut self, cx: &mut Context<Self>) {
        self.connecting = true;
        self.start_error = None;
        self.items.clear();
        self.expanded_thinking.clear();
        self.expanded_tools.clear();
        self.pending_permission = None;
        self.in_flight = false;
        self.last_usage = None;
        let (agent, cwd, policy) = (self.agent, self.cwd.clone(), self.policy);
        let resume = self.resume.clone();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<ChatUiEvent>();
        std::thread::spawn(move || {
            // Settings → Agent access 的凭证每次启动现读盘:改完对下一次
            // 会话即生效,面板/应用都不用重启(文件是几十字节的小 JSON)
            let mut env = crate::agent_auth::env_for(agent);
            // Settings → Providers 的自定义 OpenAI 兼容 provider 仅注入 opencode:
            // 经 OPENCODE_CONFIG_CONTENT 内联配置传给子进程,不碰用户的
            // ~/.config/opencode 文件;注入失败直接报告,不起半配置的会话
            if agent == AgentId::Opencode {
                let injected = crate::providers::load().and_then(|mut list| {
                    // 设置页允许保存未配模型的草稿 provider;未完成的不注入,
                    // 不阻塞普通 opencode 聊天
                    list.retain(|provider| !provider.models.is_empty());
                    crate::providers::merge_opencode_env(&list, &mut env)
                });
                if let Err(error) = injected {
                    let _ = tx.unbounded_send(ChatUiEvent::Ready(Err(error)));
                    return;
                }
            }
            match AcpSession::spawn(agent, &cwd, policy, &env, resume.as_deref()) {
                Ok((session, event_rx)) => {
                    // 会话的所有权经 channel 交给 panel;子进程被杀后事件流自然
                    // 收尾(AcpSession::Drop → shutdown → stdout EOF)
                    if tx
                        .unbounded_send(ChatUiEvent::Ready(Ok(Arc::new(session))))
                        .is_err()
                    {
                        return;
                    }
                    // std Receiver 的阻塞迭代在专属线程上,不进 async 执行器
                    for ev in event_rx {
                        if tx.unbounded_send(ChatUiEvent::Event(ev)).is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.unbounded_send(ChatUiEvent::Ready(Err(format!("{e:#}"))));
                }
            }
        });
        cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                if this.update(cx, |this, cx| this.on_ui_event(ev, cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn on_ui_event(&mut self, ev: ChatUiEvent, cx: &mut Context<Self>) {
        match ev {
            ChatUiEvent::Ready(Ok(session)) => {
                self.connecting = false;
                // 模型选择器:configOptions 只在握手时带来一次,set_config
                // 的应答不回传新值,切换显示由本地副本维护
                self.model_choices = session
                    .models
                    .iter()
                    .map(|m| {
                        let value = m
                            .current
                            .clone()
                            .or_else(|| m.options.first().map(|(v, _)| v.clone()))
                            .unwrap_or_default();
                        (m.config_id.clone(), value)
                    })
                    .collect();
                self.session = Some(session);
            }
            ChatUiEvent::Ready(Err(e)) => {
                self.connecting = false;
                self.start_error = Some(e);
            }
            ChatUiEvent::Event(ev) => self.on_chat_event(ev, cx),
        }
        cx.notify();
    }

    fn on_chat_event(&mut self, ev: ChatEvent, cx: &mut Context<Self>) {
        match ev {
            ChatEvent::UserMessage(text) => {
                // send() 已把附件名预置在 pending_names;续聊回放的历史
                // 没有附件可挂,拿到空列表就是空气泡附件
                let attachments = std::mem::take(&mut self.pending_names);
                self.items.push(ChatItem::User { text, attachments });
                self.register_chat(cx);
            }
            ChatEvent::AssistantDelta(text) => {
                append_to_last(
                    &mut self.items,
                    |item| matches!(item, ChatItem::Assistant(_)),
                    |item| match item {
                        ChatItem::Assistant(t) => t.push_str(&text),
                        _ => {}
                    },
                    || ChatItem::Assistant(text.clone()),
                );
            }
            ChatEvent::ThinkingDelta(text) => {
                append_to_last(
                    &mut self.items,
                    |item| matches!(item, ChatItem::Thinking(_)),
                    |item| match item {
                        ChatItem::Thinking(t) => t.push_str(&text),
                        _ => {}
                    },
                    || ChatItem::Thinking(text.clone()),
                );
            }
            ChatEvent::ToolCall {
                id,
                title,
                kind,
                status,
                input,
                output,
            } => {
                // 按 id upsert:update 到位刷新状态/输出,新 id 追加
                if let Some(view) = self.items.iter_mut().rev().find_map(|item| match item {
                    ChatItem::Tool(v) if v.id == id => Some(v),
                    _ => None,
                }) {
                    view.is_error = status == "failed";
                    view.output = output.or(view.output.clone());
                    if !title.is_empty() {
                        view.name = title;
                    }
                    if let Some(raw) = input {
                        view.input = Some(pretty_json(&raw));
                        view.input_preview = compact_json(&raw);
                    }
                } else {
                    let name = if title.is_empty() { kind } else { title };
                    self.items.push(ChatItem::Tool(ToolCallView {
                        id,
                        name,
                        input_preview: input
                            .as_ref()
                            .map(|v| compact_json(v))
                            .unwrap_or_default(),
                        input: input.map(|v| pretty_json(&v)),
                        output,
                        is_error: status == "failed",
                        sidechain_ref: None,
                    }));
                }
            }
            // plan 块的渲染收进 M4 polish;M3 不展示也不丢事件
            ChatEvent::PlanEntry { .. } => {}
            ChatEvent::PermissionRequest(req) => {
                self.pending_permission = Some(req);
            }
            ChatEvent::TurnDone { .. } => {
                self.in_flight = false;
                // 一回合落定,刷新登记簿时间(标题已登记,空串不覆盖)
                self.register_chat(cx);
            }
            ChatEvent::AgentError(msg) => {
                self.items.push(ChatItem::Error(msg));
                self.in_flight = false;
            }
            ChatEvent::Usage { total_tokens } => {
                self.last_usage = total_tokens;
            }
        }
        cx.notify();
    }

    /// 发送一轮。附件并入 prompt 内容块(图片 base64、其余 resource_link),
    /// composer 的清理走传入的 entity(订阅回调与 Send 按钮共用)
    fn send(
        &mut self,
        input: &Entity<TextareaState>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let text = input.read(cx).value().trim().to_string();
        if (text.is_empty() && self.pending_attachments.is_empty())
            || self.in_flight
            || !self.can_prompt()
        {
            return;
        }
        let Some(session) = self.session.clone() else {
            return;
        };
        let attachments = std::mem::take(&mut self.pending_attachments);
        // 用户气泡由事件流画(事件流自足,回放/测试两边一致);附件名
        // 先预置,UserMessage 事件到达时挂上
        self.pending_names = attachments.iter().map(|a| a.name.clone()).collect();
        input.update(cx, |state, cx| state.set_value("", window, cx));
        let mut blocks: Vec<acp::PromptBlock> = Vec::new();
        if !text.is_empty() {
            blocks.push(acp::PromptBlock::text(text));
        }
        blocks.extend(attachments.into_iter().map(|a| a.block));
        if let Err(e) = session.prompt(&blocks) {
            self.pending_names.clear();
            self.items.push(ChatItem::Error(e));
            cx.notify();
            return;
        }
        self.in_flight = true;
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            let _ = session.cancel();
        }
        cx.notify();
    }

    /// 换 agent = 换会话:旧子进程显式收掉(它的事件线程随之收尾),
    /// 时间线清空重来。同 agent 的调用是无操作(重开由 Workbench 负责)
    fn switch_agent(&mut self, agent: AgentId, cx: &mut Context<Self>) {
        if agent == self.agent {
            return;
        }
        if let Some(session) = self.session.take() {
            session.shutdown();
        }
        self.agent = agent;
        self.meta.agent = agent;
        self.resume = None;
        self.registered = false;
        self.start_session(cx);
    }

    /// 新对话:收掉旧子进程、清续聊指针重新握手。旧会话的登记条目保留,
    /// 仍可从侧栏 Chats 续
    fn start_new_chat(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = self.session.take() {
            session.shutdown();
        }
        self.resume = None;
        self.registered = false;
        self.model_choices.clear();
        self.pending_attachments.clear();
        self.pending_names.clear();
        self.start_session(cx);
    }

    /// 切模型/配置选项(kimi 实测支持 session/set_config_option)。发送
    /// 立即返回,失败经 AgentError 上浮成错误条;显示先切本地副本
    fn set_model(&mut self, config_id: String, value: String, cx: &mut Context<Self>) {
        if let Some(session) = &self.session {
            if let Err(e) = session.set_config(&config_id, &value) {
                self.items.push(ChatItem::Error(e));
            }
        }
        if let Some(slot) = self
            .model_choices
            .iter_mut()
            .find(|(id, _)| *id == config_id)
        {
            slot.1 = value;
        }
        cx.notify();
    }

    /// 会话指针写进登记簿(左栏 Chats 数据源)。标题只取首条用户消息,
    /// 之后只刷时间;写盘失败静默——登记簿是便利层,不该打断对话
    fn register_chat(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let Some(session_id) = session.session_id() else {
            return;
        };
        let now = now_ms();
        let title = if self.registered {
            String::new()
        } else {
            self.items
                .iter()
                .find_map(|item| match item {
                    ChatItem::User { text, .. } if !text.is_empty() => {
                        Some(text.chars().take(80).collect::<String>())
                    }
                    _ => None,
                })
                .unwrap_or_default()
        };
        let record = crate::chat_registry::ChatRecord {
            agent: self.agent.as_str().to_string(),
            session_id,
            cwd: self.cwd.display().to_string(),
            title,
            created_at: now,
            updated_at: now,
            can_resume: session.supports_load,
            // 首句标题常是问候语,模型值给左栏 Chats 当区分徽章
            model: self.model_choices.first().map(|(_, v)| v.clone()),
        };
        let _ = crate::chat_registry::upsert(record);
        self.registered = true;
        cx.emit(ChatPanelEvent::RegistryDirty);
    }

    /// 系统文件选择器 → 后台读盘编码 → 待发送附件。多选一次进全
    fn attach_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some(t("Attach files").into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = rx.await else {
                return;
            };
            // 读盘 + base64 在后台线程,主线程只收成品
            let prepared = cx
                .background_spawn(async move {
                    paths
                        .into_iter()
                        .filter_map(prepare_attachment)
                        .collect::<Vec<_>>()
                })
                .await;
            this.update_in(cx, |this, _, cx| {
                this.pending_attachments.extend(prepared);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// 拖拽中实时改宽;写盘按位移阈值节流,松手必然补一次
    fn resize_to(&mut self, width: Pixels, cx: &mut Context<Self>) {
        self.width = width;
        if f32::from(self.width) - f32::from(self.saved_width) > 24.
            || f32::from(self.saved_width) - f32::from(self.width) > 24.
        {
            self.persist_width();
        }
        cx.notify();
    }

    fn persist_width(&mut self) {
        self.saved_width = self.width;
        let _ = crate::prefs::write(CHAT_WIDTH_PREF, format!("{:.0}", f32::from(self.width)).as_bytes());
    }

    fn set_policy(&mut self, policy: PermissionPolicy, cx: &mut Context<Self>) {
        self.policy = policy;
        if let Some(session) = &self.session {
            session.set_policy(policy);
        }
        cx.notify();
    }

    /// 权限应答;None = 取消(ACP cancelled outcome,agent 侧当 reject 处理)
    fn respond_permission(&mut self, option_id: Option<String>, cx: &mut Context<Self>) {
        if let Some(req) = self.pending_permission.take() {
            if let Some(session) = &self.session {
                let _ = session.respond_permission(req.request_id, option_id.as_deref());
            }
        }
        cx.notify();
    }

    /// Continue in Wake:预填 composer(handoff 提示),顺手聚焦
    pub(crate) fn prefill(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.composer.update(cx, |state, cx| {
            state.set_value(text, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    pub(crate) fn agent(&self) -> AgentId {
        self.agent
    }

    fn can_prompt(&self) -> bool {
        self.session.is_some() && !self.connecting && self.start_error.is_none()
    }

    // ---------------------------------------------------------------- 渲染

    /// mockup 04:模型选择浮层(session/new 的 configOptions;空 = agent
    /// 没暴露选项,不出按钮)。搜索过滤 + 按 provider 分组,顶锚右对齐从
    /// composer 上方展开;无 context/effort 装饰(用户明确不要)
    fn render_model_buttons(&self, cx: &Context<Self>) -> Vec<AnyElement> {
        self.session
            .as_ref()
            .map(|s| s.models.clone())
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(ix, m)| {
                let current = self
                    .model_choices
                    .iter()
                    .find(|(id, _)| *id == m.config_id)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_default();
                let label = m
                    .options
                    .iter()
                    .find(|(v, _)| *v == current)
                    .map(|(_, l)| l.clone())
                    .or_else(|| m.current.clone())
                    .unwrap_or_else(|| m.name.clone());
                let entity = cx.entity();
                let config_id = m.config_id.clone();
                let options = m.options.clone();
                let search = self.model_search.clone();
                Popover::new(SharedString::from(format!("chat-model-pop-{ix}")))
                    .appearance(false)
                    // composer 在面板底部:向上展开、右缘对齐触发钮
                    .anchor(Anchor::TopRight)
                    .on_open_change({
                        let search = search.clone();
                        move |open, window, cx| {
                            if *open {
                                // 每次打开都从空搜索开始,避免上次的过滤残留
                                search.update(cx, |s, cx| s.set_value("", window, cx));
                            }
                        }
                    })
                    .trigger(
                        Button::new(SharedString::from(format!("chat-model-{ix}")))
                            .ghost()
                            .rounded(RADIUS_BUTTON)
                            .label(label)
                            .icon(
                                Icon::empty()
                                    .path("icons/chevron-down.svg")
                                    .with_size(px(12.)),
                            )
                            .tooltip(t("Model")),
                    )
                    .content(move |_state, _window, cx| {
                        let theme = cx.theme();
                        let muted_fg = theme.muted_foreground;
                        let radius = theme.radius;
                        let query = search.read(cx).value().trim().to_lowercase();
                        // 分组:展示名 "Provider/Model" 的前缀优先,退回值前缀;
                        // 搜索同时匹配值与展示名,过滤后组照常保留
                        let mut groups: std::collections::BTreeMap<
                            String,
                            Vec<(String, String)>,
                        > = std::collections::BTreeMap::new();
                        let mut total = 0usize;
                        for (value, name) in &options {
                            let hits = query.is_empty()
                                || value.to_lowercase().contains(&query)
                                || name.to_lowercase().contains(&query);
                            if !hits {
                                continue;
                            }
                            total += 1;
                            let provider = if name.contains('/') {
                                name.split('/').next().unwrap_or(name).to_string()
                            } else if value.contains('/') {
                                value.split('/').next().unwrap_or(value).to_string()
                            } else {
                                t("Models").to_string()
                            };
                            groups.entry(provider).or_default().push((value.clone(), name.clone()));
                        }
                        let pop = cx.entity();
                        let mut rows: Vec<AnyElement> = Vec::new();
                        for (provider, models) in groups {
                            rows.push(
                                div()
                                    .text_size(FONT_CAPTION)
                                    .font_medium()
                                    .text_color(muted_fg)
                                    .child(provider)
                                    .into_any_element(),
                            );
                            for (value, name) in models {
                                let entity = entity.clone();
                                let config_id = config_id.clone();
                                let pop = pop.clone();
                                let selected = value == current;
                                rows.push(
                                    div()
                                        .id(SharedString::from(format!(
                                            "chat-model-row-{config_id}-{value}"
                                        )))
                                        .flex()
                                        .items_center()
                                        .gap(SPACE_SM)
                                        .px(SPACE_SM)
                                        .py(px(4.))
                                        .rounded(radius)
                                        .cursor_pointer()
                                        .when(selected, |row| row.bg(theme.muted))
                                        .hover(|row| row.bg(theme.muted))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .truncate()
                                                .child(name),
                                        )
                                        .when(selected, |row| {
                                            row.child(
                                                Icon::empty()
                                                    .path("icons/check.svg")
                                                    .with_size(px(14.)),
                                            )
                                        })
                                        .on_click(move |_, _, cx| {
                                            // 选中即收浮层,再切本地副本 + set_config
                                            pop.update(cx, |s, cx| s.set_open(false, cx));
                                            entity.update(cx, |this, cx| {
                                                this.set_model(
                                                    config_id.clone(),
                                                    value.clone(),
                                                    cx,
                                                )
                                            });
                                        })
                                        .into_any_element(),
                                );
                            }
                        }
                        if total == 0 {
                            rows.push(
                                div()
                                    .text_size(FONT_CAPTION)
                                    .text_color(muted_fg)
                                    .child(t("No matching models"))
                                    .into_any_element(),
                            );
                        }
                        v_flex()
                            .w(px(300.))
                            .rounded(theme.radius_lg)
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.popover)
                            .shadow_lg()
                            .p(SPACE_SM)
                            .gap(SPACE_SM)
                            .child(
                                div()
                                    .rounded(radius)
                                    .bg(theme.muted)
                                    .px(SPACE_SM)
                                    .py(px(2.))
                                    .child(
                                        Input::new(&search)
                                            .bordered(false)
                                            .appearance(false)
                                            .small(),
                                    ),
                            )
                            .child(
                                v_flex()
                                    .id("chat-model-list")
                                    .max_h(px(360.))
                                    .overflow_y_scroll()
                                    .gap(SPACE_XS)
                                    .children(rows),
                            )
                    })
            })
            .map(|popover| popover.into_any_element())
            .collect::<Vec<_>>()
    }

    /// mockup 03:权限模式下拉,菜单项带一行说明(Ask 逐项批准 / Auto 全自动)
    fn render_policy_button(&self, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let muted_fg = theme.muted_foreground;
        let entity = cx.entity();
        let current = self.policy;
        let label: SharedString = match self.policy {
            PermissionPolicy::Ask => t("Ask every time"),
            PermissionPolicy::AutoApprove => t("Auto-approve"),
        }
        .into();
        Button::new("chat-policy")
            .ghost()
            .rounded(RADIUS_BUTTON)
            .label(label)
            .icon(Icon::empty().path("icons/chevron-down.svg").with_size(px(12.)))
            .tooltip(t("Permission mode"))
            .dropdown_menu(move |mut menu, _, _| {
                for (policy, title, sub) in [
                    (PermissionPolicy::Ask, "Ask every time", "Approve each action"),
                    (PermissionPolicy::AutoApprove, "Auto-approve", "Run without asking"),
                ] {
                    let entity = entity.clone();
                    let selected = policy == current;
                    menu = menu.item(
                        PopupMenuItem::element(move |_, _| {
                            h_flex()
                                .items_center()
                                .justify_between()
                                .gap(SPACE_LG)
                                .child(
                                    v_flex().gap(px(2.))
                                        .child(div().child(t(title)))
                                        .child(
                                            div()
                                                .text_size(FONT_CAPTION)
                                                .text_color(muted_fg)
                                                .child(t(sub)),
                                        ),
                                )
                                .when(selected, |this| {
                                    this.child(
                                        Icon::empty().path("icons/check.svg").with_size(px(14.)),
                                    )
                                })
                        })
                        .checked(selected)
                        .on_click(move |_, _, cx| {
                            entity.update(cx, |this, cx| this.set_policy(policy, cx));
                        }),
                    );
                }
                menu
            })
            .into_any_element()
    }

    fn render_header(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let entity = cx.entity();
        let agent = self.agent;
        h_flex()
            .h(px(44.))
            .flex_shrink_0()
            .px(SPACE_MD)
            .gap(SPACE_XS)
            .items_center()
            .border_b_1()
            .border_color(theme.border)
            // agent picker:方言表可用目标 + 品牌色点,与 Continue with 菜单
            // 同一 idiom。菜单展开时现查可用列表(有 PATH 缓存,不付代价)
            .child(
                Button::new("chat-agent-picker")
                    .ghost()
                    .rounded(RADIUS_BUTTON)
                    .label(agent.display_name())
                    .icon(Icon::empty().path("icons/chevron-down.svg").with_size(px(12.)))
                    .tooltip(t("Switch agent"))
                    .dropdown_menu(move |mut menu, _, _| {
                        for a in acp::acp_targets() {
                            let entity = entity.clone();
                            let color = rgb(agent_series_color(a));
                            menu = menu.item(
                                PopupMenuItem::element(move |_, _| {
                                    h_flex()
                                        .gap(SPACE_SM)
                                        .items_center()
                                        .child(
                                            div()
                                                .size(px(8.))
                                                .rounded_full()
                                                .flex_shrink_0()
                                                .bg(color),
                                        )
                                        .child(a.display_name())
                                })
                                .checked(a == agent)
                                .on_click(move |_, _, cx| {
                                    entity.update(cx, |this, cx| this.switch_agent(a, cx));
                                }),
                            );
                        }
                        menu
                    }),
            )
            .child(div().flex_1())
            .when_some(self.last_usage, |this, tokens| {
                this.child(
                    div()
                        .text_size(FONT_LABEL)
                        .text_color(theme.muted_foreground)
                        .child(crate::tf!("{} tokens", thousands(tokens))),
                )
            })
            .child(
                Button::new("chat-new")
                    .ghost()
                    .rounded(RADIUS_BUTTON)
                    .icon(Icon::empty().path("icons/plus.svg").with_size(px(14.)))
                    .tooltip(t("New chat"))
                    .on_click(cx.listener(|this, _, _, cx| this.start_new_chat(cx))),
            )
            .child(
                Button::new("chat-close")
                    .ghost()
                    .rounded(RADIUS_BUTTON)
                    .icon(Icon::empty().path("icons/close.svg").with_size(px(14.)))
                    .tooltip(t("Close chat"))
                    .on_click(cx.listener(|_this, _, _, cx| {
                        cx.emit(ChatPanelEvent::Close);
                        cx.notify();
                    })),
            )
    }

    /// 时间线渲染:连续工具调用聚成一簇,其余条目逐条出。
    /// 索引即折叠态 key(时间线只追加,跨帧稳定)
    fn render_timeline(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut children: Vec<AnyElement> = Vec::new();
        let mut ix = 0usize;
        while ix < self.items.len() {
            if matches!(self.items[ix], ChatItem::Tool(_)) {
                let start = ix;
                while ix < self.items.len() && matches!(self.items[ix], ChatItem::Tool(_)) {
                    ix += 1;
                }
                let calls: Vec<ToolCallView> = self.items[start..ix]
                    .iter()
                    .map(|item| match item {
                        ChatItem::Tool(v) => v.clone(),
                        _ => unreachable!("run only holds tools"),
                    })
                    .collect();
                let expanded = self.expanded_tools.contains(&start);
                children.push(
                    tool_cluster(
                        start,
                        &calls,
                        TOOL_ARG_CELLS,
                        expanded,
                        cx.listener(move |this, _, _, cx| {
                            if !this.expanded_tools.remove(&start) {
                                this.expanded_tools.insert(start);
                            }
                            cx.notify();
                        }),
                        cx,
                    )
                    .into_any_element(),
                );
                continue;
            }
            children.push(self.render_item(ix, cx));
            ix += 1;
        }
        let theme = cx.theme();
        v_flex()
            .id("chat-timeline")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px(SPACE_LG)
            .py(SPACE_LG)
            .gap(SPACE_MD)
            .when(self.items.is_empty() && !self.connecting, |this| {
                // mockup 01:空态 hero(bot 图标 + 一句招呼),居中占满时间线
                this.child(
                    v_flex()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .gap(SPACE_LG)
                        .child(
                            Icon::empty()
                                .path("icons/bot.svg")
                                .with_size(px(56.))
                                .text_color(theme.muted_foreground),
                        )
                        .child(
                            div()
                                .text_size(px(20.))
                                .text_color(theme.foreground)
                                .child(t("What can I help you build?")),
                        ),
                )
            })
            .when(self.connecting, |this| {
                this.child(
                    h_flex()
                        .gap(SPACE_SM)
                        .text_size(FONT_CAPTION)
                        .text_color(theme.muted_foreground)
                        .child(Spinner::new().small())
                        .child(t("Starting agent…")),
                )
            })
            .children(children)
    }

    fn render_item(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        // 需要的颜色先拷出(cx.theme() 借着 cx,而 markdown_body 要 &mut App;
        // 每条临时借用随分号结束,不留长命绑定)
        let danger = cx.theme().danger;
        let radius_lg = cx.theme().radius_lg;
        let muted = cx.theme().muted;
        let muted_fg = cx.theme().muted_foreground;
        let dark = cx.theme().mode.is_dark();
        match &self.items[ix] {
            ChatItem::User { text, attachments } => h_flex()
                .w_full()
                .justify_end()
                .child(
                    v_flex()
                        .max_w(px(380.))
                        .min_w_0()
                        .rounded(radius_lg)
                        .bg(muted)
                        .px(px(14.))
                        .py(SPACE_SM)
                        .gap(SPACE_XS)
                        .text_size(FONT_MSG_USER)
                        .line_height(relative(1.6))
                        .when(!text.is_empty(), |this| this.child(text.clone()))
                        // 附件名单列在气泡尾部(内容块已在 prompt 里,这里只记账)
                        .when(!attachments.is_empty(), |this| {
                            this.children(attachments.iter().map(|name| {
                                h_flex()
                                    .gap(SPACE_XS)
                                    .items_center()
                                    .text_size(FONT_CAPTION)
                                    .text_color(muted_fg)
                                    .child(
                                        Icon::empty()
                                            .path("icons/file-text.svg")
                                            .with_size(px(12.)),
                                    )
                                    .child(name.clone())
                            }))
                        }),
                )
                .into_any_element(),
            ChatItem::Assistant(text) => markdown_body(
                SharedString::from(format!("chat-msg-{ix}")),
                text,
                &self.meta,
                FONT_MSG_BODY,
                gpui::rems(0.5),
                dark,
                cx,
            )
            .into_any_element(),
            ChatItem::Thinking(text) => thinking_panel(
                ix,
                text,
                self.expanded_thinking.contains(&ix),
                cx.listener(move |this, _, _, cx| {
                    if !this.expanded_thinking.remove(&ix) {
                        this.expanded_thinking.insert(ix);
                    }
                    cx.notify();
                }),
                cx,
            )
            .into_any_element(),
            ChatItem::Tool(_) => unreachable!("tool runs are clustered in render_timeline"),
            ChatItem::Error(msg) => div()
                .w_full()
                .rounded(RADIUS_IMAGE)
                .border_1()
                .border_color(danger)
                .p(SPACE_SM)
                .text_size(FONT_CAPTION)
                .text_color(danger)
                .child(msg.clone())
                .when(looks_like_auth(msg), |this| {
                    this.child(
                        div()
                            .mt(SPACE_SM)
                            .child(t(
                                "Needs credentials — add an API key in Settings → Agent access, or sign in via the agent's CLI.",
                            )),
                    )
                })
                .into_any_element(),
        }
    }

    fn render_permission_card(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let Some(req) = &self.pending_permission else {
            return div().into_any_element();
        };
        let entity = cx.entity();
        let mut col = v_flex()
            .flex_shrink_0()
            .mx(SPACE_LG)
            .mb(SPACE_SM)
            .gap(SPACE_SM)
            .rounded(RADIUS_IMAGE)
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .p(SPACE_MD)
            .child(
                div()
                    .text_size(FONT_CAPTION)
                    .text_color(theme.muted_foreground)
                    .child(t("The agent requests permission")),
            )
            .child(div().text_size(FONT_BODY).child(req.title.clone()))
            .child(h_flex().flex_wrap().gap(SPACE_SM));
        for opt in &req.options {
            let entity = entity.clone();
            let option_id = opt.option_id.clone();
            let is_reject = opt.kind.starts_with("reject");
            col = col.child(if is_reject {
                Button::new(SharedString::from(format!("perm-{}", opt.option_id)))
                    .ghost()
                    .rounded(RADIUS_BUTTON)
                    .small()
                    .label(opt.name.clone())
                    .on_click(move |_, _, cx| {
                        let id = option_id.clone();
                        entity.update(cx, |this, cx| this.respond_permission(Some(id), cx));
                    })
            } else {
                Button::new(SharedString::from(format!("perm-{}", opt.option_id)))
                    .rounded(RADIUS_BUTTON)
                    .small()
                    .label(opt.name.clone())
                    .on_click(move |_, _, cx| {
                        let id = option_id.clone();
                        entity.update(cx, |this, cx| this.respond_permission(Some(id), cx));
                    })
            });
        }
        col.into_any_element()
    }

    fn render_composer(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        if self.in_flight {
            h_flex()
                .flex_shrink_0()
                .items_center()
                .gap(SPACE_SM)
                .px(SPACE_LG)
                .pb(SPACE_LG)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .rounded(RADIUS_IMAGE)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.background)
                        .px(SPACE_MD)
                        .py(SPACE_SM)
                        .text_size(FONT_CAPTION)
                        .text_color(theme.muted_foreground)
                        .child(t("Working — press Stop to interrupt")),
                )
                .child(
                    Button::new("chat-stop")
                        .rounded(RADIUS_BUTTON)
                        .small()
                        .label(t("Stop"))
                        .on_click(cx.listener(|this, _, _, cx| this.stop(cx))),
                )
                .into_any_element()
        } else {
            // mockup 01/03/04:composer 是浮在面板上的卡片——输入区在上,
            // 工具条在下(＋ 附件 · 权限模式 ··· 模型 · 圆形发送);模型与
            // 权限从头部移进来,头部只留 agent/用量/新对话/关闭
            let model_buttons = self.render_model_buttons(cx);
            let policy_button = self.render_policy_button(cx);
            v_flex()
                .flex_shrink_0()
                .px(SPACE_LG)
                .pb(SPACE_LG)
                .child(
                    v_flex()
                        .rounded(theme.radius_lg)
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .shadow_sm()
                        .px(SPACE_MD)
                        .py(SPACE_SM)
                        .gap(SPACE_SM)
                        // 待发送附件条:chip 上的 × 摘掉对应附件
                        .when(!self.pending_attachments.is_empty(), |this| {
                    this.child(h_flex().flex_wrap().gap(SPACE_SM).children(
                        self.pending_attachments.iter().enumerate().map(|(ix, a)| {
                            let entity = cx.entity();
                            let name = a.name.clone();
                            h_flex()
                                .gap(SPACE_XS)
                                .items_center()
                                .max_w(px(200.))
                                .rounded(RADIUS_IMAGE)
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.muted)
                                .px(SPACE_SM)
                                .py(px(2.))
                                .text_size(FONT_CAPTION)
                                .child(div().min_w_0().truncate().child(name))
                                .child(
                                    Button::new(SharedString::from(format!(
                                        "chat-attach-rm-{ix}"
                                    )))
                                    .ghost()
                                    .xsmall()
                                    .rounded(RADIUS_IMAGE)
                                    .icon(
                                        Icon::empty()
                                            .path("icons/circle-x.svg")
                                            .with_size(px(12.)),
                                    )
                                    .tooltip(t("Remove attachment"))
                                    .on_click(move |_, _, cx| {
                                        entity.update(cx, |this, cx| {
                                            this.pending_attachments.remove(ix);
                                            cx.notify();
                                        });
                                    }),
                                )
                                .into_any_element()
                        }),
                    ))
                })
                        .child(Textarea::new(&self.composer).bordered(false).appearance(false))
                        .child(
                            h_flex()
                                .items_center()
                                .gap(SPACE_XS)
                                // mockup:＋ 拉起系统文件选择器选附件
                                .child(
                                    Button::new("chat-attach")
                                        .ghost()
                                        .rounded(RADIUS_BUTTON)
                                        .icon(
                                            Icon::empty()
                                                .path("icons/plus.svg")
                                                .with_size(px(16.)),
                                        )
                                        .tooltip(t("Attach files"))
                                        .disabled(!self.can_prompt())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.attach_files(window, cx)
                                        })),
                                )
                                .child(policy_button)
                                .child(div().flex_1())
                                .children(model_buttons)
                                // mockup:圆形发送按钮,箭头向上
                                .child(
                                    Button::new("chat-send")
                                        .primary()
                                        .small()
                                        .rounded_full()
                                        .icon(
                                            Icon::empty()
                                                .path("icons/arrow-up.svg")
                                                .with_size(px(16.)),
                                        )
                                        .tooltip(t("Send"))
                                        .disabled(!self.can_prompt())
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            let composer = this.composer.clone();
                                            this.send(&composer, window, cx);
                                        })),
                                ),
                        ),
                )
                .into_any_element()
        }
    }
}

impl Render for ChatPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // 颜色先拷出:render_timeline 需要 &mut cx
        let bg = cx.theme().background;
        let border = cx.theme().border;
        let muted_fg = cx.theme().muted_foreground;
        let entity = cx.entity();
        let width = self.width;
        v_flex()
            .relative()
            .w(width)
            .flex_shrink_0()
            .h_full()
            .bg(bg)
            .border_l_1()
            .border_color(border)
            // 松手清按下态并落盘(pointer 在面板内即触发;拖拽中按 24px 阈值节流兜底)
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _cx| {
                    this.resize_press = None;
                    this.persist_width();
                }),
            )
            .child(self.render_header(cx))
            .child(self.render_timeline(cx))
            .when_some(self.start_error.clone(), |this, err| {
                this.child(
                    div()
                        .mx(SPACE_LG)
                        .mb(SPACE_SM)
                        .rounded(RADIUS_IMAGE)
                        .border_1()
                        .border_color(border)
                        .p(SPACE_MD)
                        .text_size(FONT_CAPTION)
                        .text_color(muted_fg)
                        .child(err.clone())
                        .when(looks_like_auth(&err), |this| {
                            this.child(
                                div()
                                    .mt(SPACE_SM)
                                    .text_color(muted_fg)
                                    .child(t(
                                        "Needs credentials — add an API key in Settings → Agent access, or sign in via the agent's CLI.",
                                    )),
                            )
                            .child(
                                Button::new("chat-open-settings")
                                    .outline()
                                    .small()
                                    .rounded(RADIUS_BUTTON)
                                    .mt(SPACE_SM)
                                    .label(t("Open Settings"))
                                    .on_click(cx.listener(|_this, _, _, cx| {
                                        cx.emit(ChatPanelEvent::OpenSettings);
                                        cx.notify();
                                    })),
                            )
                        }),
                )
            })
            .child(self.render_permission_card(cx))
            .child(self.render_composer(cx))
            // 左缘拖拽把手:absolute 覆在 border 上,拖动实时改宽。
            // ghost 记起点与起宽,move 里用差值换算(向左拖 = 变宽)
            .child(
                div()
                    .id("chat-resize-handle")
                    .absolute()
                    .left_0()
                    .top_0()
                    .bottom_0()
                    .w(px(6.))
                    .cursor_col_resize()
                    // 按下记全局起点与当时面板宽;拖拽中用全局位移换算新宽
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, _, _| {
                            this.resize_press = Some((event.position.x, this.width));
                        }),
                    )
                    .on_drag(ChatResize, move |_, _, _, cx| cx.new(|_| ChatResize))
                    .on_drag_move(
                        move |event: &DragMoveEvent<ChatResize>, _, cx: &mut App| {
                            entity.update(cx, |this, cx| {
                                let Some((press_x, press_w)) = this.resize_press else {
                                    return;
                                };
                                let w = f32::from(press_w)
                                    + f32::from(press_x - event.event.position.x);
                                this.resize_to(px(w.clamp(CHAT_WIDTH_MIN, CHAT_WIDTH_MAX)), cx);
                            });
                        },
                    ),
            )
    }
}

// ---------------------------------------------------------------- 小工具

/// 工具簇头部的参数摘要列宽(详情页同款近似等宽格)
const TOOL_ARG_CELLS: usize = 44;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

/// 附件准备:图片(≤8MB)转 base64 image 块;其余文件与超大图片走
/// resource_link(file:// URI),agent 经自己的读取器取——发什么由
/// agent 能力决定,Wake 不做格式转换
fn prepare_attachment(path: PathBuf) -> Option<PendingAttachment> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.display().to_string());
    let small_image = image_mime(&path).filter(|_| {
        std::fs::metadata(&path)
            .map(|m| m.len() as usize <= ATTACH_IMAGE_MAX_BYTES)
            .unwrap_or(false)
    });
    if let Some(mime) = small_image {
        let Ok(bytes) = std::fs::read(&path) else {
            return None;
        };
        use base64::Engine as _;
        return Some(PendingAttachment {
            name,
            block: acp::PromptBlock::Image {
                data: base64::engine::general_purpose::STANDARD.encode(bytes),
                mime: mime.to_string(),
            },
        });
    }
    Some(PendingAttachment {
        name: name.clone(),
        block: acp::PromptBlock::ResourceLink {
            uri: file_uri(&path),
            name,
        },
    })
}

fn image_mime(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

/// file:// URI(空格等非法字符交给 Url 编码,选择器给的路径不做保证)
fn file_uri(path: &Path) -> String {
    match url::Url::from_file_path(path) {
        Ok(u) => u.to_string(),
        Err(_) => format!("file://{}", path.display()),
    }
}

/// 错误文案看起来是认证问题时,附上 Settings → Agent access 的指引。
/// 各家 CLI 的报错措辞不一,按关键词宽匹配(claude "Authentication
/// required"、opencode "auth required"、适配器 401 等)
fn looks_like_auth(message: &str) -> bool {
    let m = message.to_lowercase();
    ["authenticat", "unauthorized", "not logged in", "login required", "api key", "credentials"]
        .iter()
        .any(|needle| m.contains(needle))
}

fn pretty_json(v: &serde_json::Value) -> String {
    serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string())
}

fn compact_json(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// 把增量文本并进时间线尾部最近的同类条目(没有就新开一条)
fn append_to_last(
    items: &mut Vec<ChatItem>,
    same_kind: impl Fn(&ChatItem) -> bool,
    push_text: impl Fn(&mut ChatItem),
    make: impl FnOnce() -> ChatItem,
) {
    if let Some(item) = items.last_mut().filter(|item| same_kind(item)) {
        push_text(item);
    } else {
        items.push(make());
    }
}
