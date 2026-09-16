//! Antigravity 会话步骤载荷的通用 protobuf 解码。官方 .proto 不公开,这里按
//! wire format 逐字段走树,再按已知的字段号启发式取内容——与 agentsview 的
//! agProto* 做法同源(字段语义经其 sidecar 对照验证,见其
//! antigravity_proto.go / antigravity.go)。

pub(crate) const WIRE_VARINT: i32 = 0;
pub(crate) const WIRE_FIXED64: i32 = 1;
pub(crate) const WIRE_BYTES: i32 = 2;
const WIRE_FIXED32: i32 = 5;

/// 递归上限:坏载荷不得炸栈
const MAX_DEPTH: usize = 32;
/// 单次解析的字段总预算(含被丢弃的投机嵌套重解),防宽度放大
const MAX_FIELDS: usize = 1 << 20;

/// 一条 wire-format 字段。length-delimited 载荷若能整段重解成合法消息,
/// 存入 `nested`;否则视为字符串/不透明字节(`bytes`)。
pub(crate) struct Field<'a> {
    pub(crate) number: u32,
    pub(crate) wire: i32,
    pub(crate) varint: u64,
    pub(crate) bytes: &'a [u8],
    pub(crate) nested: Option<Vec<Field<'a>>>,
}

impl<'a> Field<'a> {
    pub(crate) fn as_str(&self) -> Option<&'a str> {
        if self.wire != WIRE_BYTES {
            return None;
        }
        std::str::from_utf8(self.bytes).ok()
    }
}

/// 顶层解码。预算耗尽返回已解出的前缀(大转录降级为部分内容,不整段丢弃)。
pub(crate) fn parse(data: &[u8]) -> Vec<Field<'_>> {
    let mut budget = MAX_FIELDS;
    parse_depth(data, 0, &mut budget).0
}

/// 返回 (字段, 是否整段干净解完)。`complete` 供投机重解用:只有整段
/// 合法才算嵌套消息,ASCII 字符串常能碰巧解出合法前缀,不能当消息。
fn parse_depth<'a>(data: &'a [u8], depth: usize, budget: &mut usize) -> (Vec<Field<'a>>, bool) {
    let mut out = Vec::new();
    if depth > MAX_DEPTH {
        return (out, false);
    }
    let mut pos = 0usize;
    while pos < len(data) {
        if *budget == 0 {
            return (out, false);
        }
        *budget -= 1;
        let Some((tag, n)) = uvarint(&data[pos..]) else {
            return (out, false);
        };
        pos += n;
        let number = (tag >> 3) as u32;
        let wire = (tag & 0x7) as i32;
        if number == 0 {
            return (out, false);
        }
        let mut field = Field {
            number,
            wire,
            varint: 0,
            bytes: &[],
            nested: None,
        };
        match wire {
            WIRE_VARINT => match uvarint(&data[pos..]) {
                Some((v, m)) => {
                    field.varint = v;
                    pos += m;
                }
                None => return (out, false),
            },
            WIRE_FIXED64 => {
                if pos + 8 > data.len() {
                    return (out, false);
                }
                field.bytes = &data[pos..pos + 8];
                pos += 8;
            }
            WIRE_FIXED32 => {
                if pos + 4 > data.len() {
                    return (out, false);
                }
                field.bytes = &data[pos..pos + 4];
                pos += 4;
            }
            WIRE_BYTES => {
                let Some((ln, m)) = uvarint(&data[pos..]) else {
                    return (out, false);
                };
                pos += m;
                if ln as usize > data.len() - pos {
                    return (out, false);
                }
                field.bytes = &data[pos..pos + ln as usize];
                pos += ln as usize;
                // 投机重解:整段干净解完且像消息才挂 nested,否则是字符串
                let mut nested_budget = *budget;
                let (nested, complete) = parse_depth(field.bytes, depth + 1, &mut nested_budget);
                *budget = nested_budget;
                if complete && looks_like_message(&nested) {
                    field.nested = Some(nested);
                }
            }
            _ => return (out, false),
        }
        out.push(field);
    }
    (out, true)
}

/// 空切片防止 `&data[pos..pos]` 的 pos 越界误用(调用方保证 pos <= len)
fn len(data: &[u8]) -> usize {
    data.len()
}

fn uvarint(data: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    for (i, byte) in data.iter().take(10).enumerate() {
        value |= ((byte & 0x7f) as u64) << (i * 7);
        if byte & 0x80 == 0 {
            return Some((value, i + 1));
        }
    }
    None
}

/// 投机重解的判定:至少一个字段,且字段号全部在合理范围
fn looks_like_message(fields: &[Field<'_>]) -> bool {
    !fields.is_empty() && fields.iter().all(|f| (1..=100000).contains(&f.number))
}

/// 深度优先找第一个指定字段号的子字段(含嵌套)
pub(crate) fn find<'a>(fields: &'a [Field<'a>], number: u32) -> Option<&'a Field<'a>> {
    fields.iter().find(|f| f.number == number)
}

/// 递归收集树中所有 UTF-8 字符串(min_len 个字符以上,按遭遇序,不去重)
pub(crate) fn collect_strings(fields: &[Field<'_>], min_len: usize) -> Vec<String> {
    let mut out = Vec::new();
    walk_strings(fields, min_len, &mut out);
    out
}

fn walk_strings(fields: &[Field<'_>], min_len: usize, out: &mut Vec<String>) {
    for f in fields {
        if f.wire == WIRE_BYTES && f.nested.is_none() {
            if let Some(s) = f.as_str() {
                if s.chars().count() >= min_len {
                    // NUL 分隔输出是真实内容,但落库文本不能带 NUL
                    out.push(s.replace('\0', "\u{FFFD}"));
                }
            }
        }
        if let Some(nested) = &f.nested {
            walk_strings(nested, min_len, out);
        }
    }
}

///google.protobuf.Timestamp 形状:field1 varint 秒 + 可选 field2 纳秒,无其他字段
fn timestamp_of(fields: &[Field<'_>]) -> Option<i64> {
    let mut sec = 0i64;
    let mut nanos = 0u64;
    let mut saw_sec = false;
    for f in fields {
        if f.wire != WIRE_VARINT {
            return None;
        }
        match f.number {
            1 => {
                sec = f.varint as i64;
                saw_sec = true;
            }
            2 => {
                if f.varint >= 1_000_000_000 {
                    return None;
                }
                nanos = f.varint;
            }
            _ => return None,
        }
    }
    saw_sec.then_some(sec * 1000 + (nanos / 1_000_000) as i64)
}

/// 树中最早的合理 Timestamp(2000–2100 年),epoch ms
pub(crate) fn earliest_timestamp_ms(fields: &[Field<'_>]) -> i64 {
    let mut best = 0i64;
    for f in fields {
        if let Some(nested) = &f.nested {
            if let Some(ms) = timestamp_of(nested) {
                let sec = ms / 1000;
                if (946_684_800..4_102_444_800).contains(&sec) && (best == 0 || ms < best) {
                    best = ms;
                }
            }
            let inner = earliest_timestamp_ms(nested);
            if inner > 0 && (best == 0 || inner < best) {
                best = inner;
            }
        }
    }
    best
}

// ---- 字段号(出自 Antigravity CLI 内嵌 FileDescriptorProto,agentsview 考证)----

const AG_STEP_GENERATOR_METADATA_CHAT_MODEL: u32 = 1;

const AG_CHAT_MODEL_METADATA_USAGE: u32 = 4;
const AG_CHAT_MODEL_METADATA_RESPONSE_MODEL: u32 = 19;
const AG_CHAT_MODEL_METADATA_DISPLAY_NAME: u32 = 21;

const AG_MODEL_USAGE_STATS_MODEL: u32 = 1;
const AG_MODEL_USAGE_STATS_INPUT: u32 = 2;
const AG_MODEL_USAGE_STATS_OUTPUT: u32 = 3;
const AG_MODEL_USAGE_STATS_CACHE_WRITE: u32 = 4;
const AG_MODEL_USAGE_STATS_CACHE_READ: u32 = 5;

/// 单条 gen_metadata 解出的用量(model 未解出时为空串)
pub(crate) struct TokenBlock {
    pub(crate) uncached_input: i64,
    pub(crate) total_output: i64,
    pub(crate) cache_read: i64,
    pub(crate) model: String,
}

/// 其他嵌套消息可能巧合满足 token 块形状;真实生成不会到几百万 token
const MAX_PLAUSIBLE_TOKENS: u64 = 2_000_000;

fn token_block_from(fields: &[Field<'_>]) -> Option<TokenBlock> {
    let model = find(fields, AG_MODEL_USAGE_STATS_MODEL)?;
    let input = find(fields, AG_MODEL_USAGE_STATS_INPUT)?;
    let output = find(fields, AG_MODEL_USAGE_STATS_OUTPUT)?;
    if model.wire != WIRE_VARINT || input.wire != WIRE_VARINT || output.wire != WIRE_VARINT {
        return None;
    }
    // model 是枚举,varint 落在 [1000, 5000)
    if !(1000..5000).contains(&model.varint) {
        return None;
    }
    if input.varint > MAX_PLAUSIBLE_TOKENS || output.varint > MAX_PLAUSIBLE_TOKENS {
        return None;
    }
    if input.varint + output.varint > MAX_PLAUSIBLE_TOKENS {
        return None;
    }
    if let Some(cache_write) = find(fields, AG_MODEL_USAGE_STATS_CACHE_WRITE) {
        if cache_write.wire != WIRE_VARINT || cache_write.varint > MAX_PLAUSIBLE_TOKENS {
            return None;
        }
    }
    let cache_read = match find(fields, AG_MODEL_USAGE_STATS_CACHE_READ) {
        Some(f) if f.wire == WIRE_VARINT && f.varint <= MAX_PLAUSIBLE_TOKENS => f.varint as i64,
        Some(_) => return None,
        None => 0,
    };
    Some(TokenBlock {
        uncached_input: input.varint as i64,
        total_output: output.varint as i64,
        cache_read,
        model: String::new(),
    })
}

fn plausible_model_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars().count() <= 64
        && s.chars().all(|c| !c.is_control())
        && s.chars().any(|c| c.is_alphabetic())
}

fn model_name_field(fields: &[Field<'_>], number: u32) -> String {
    match find(fields, number) {
        Some(f) => f.as_str().filter(|s| plausible_model_name(s)).unwrap_or(""),
        None => "",
    }
    .to_string()
}

/// ChatModelMetadata 子消息:usage/display_name/response_model 任一在才算
fn chat_model_fields<'a>(fields: &'a [Field<'a>]) -> Option<&'a [Field<'a>]> {
    let chat = find(fields, AG_STEP_GENERATOR_METADATA_CHAT_MODEL)?;
    let nested = chat.nested.as_ref()?;
    let has_usage = find(nested, AG_CHAT_MODEL_METADATA_USAGE)
        .is_some_and(|f| f.nested.is_some());
    let has_display = !model_name_field(nested, AG_CHAT_MODEL_METADATA_DISPLAY_NAME).is_empty();
    let has_response = !model_name_field(nested, AG_CHAT_MODEL_METADATA_RESPONSE_MODEL).is_empty();
    (has_usage || has_display || has_response).then_some(nested.as_slice())
}

fn chat_model_name(fields: &[Field<'_>]) -> String {
    let display = model_name_field(fields, AG_CHAT_MODEL_METADATA_DISPLAY_NAME);
    if !display.is_empty() {
        return display;
    }
    model_name_field(fields, AG_CHAT_MODEL_METADATA_RESPONSE_MODEL)
}

fn legacy_model_name(fields: &[Field<'_>], number: u32) -> String {
    for f in fields {
        if f.number == number {
            if let Some(s) = f.as_str() {
                if plausible_model_name(s) {
                    return s.to_string();
                }
            }
        }
        if let Some(nested) = &f.nested {
            let found = legacy_model_name(nested, number);
            if !found.is_empty() {
                return found;
            }
        }
    }
    String::new()
}

fn legacy_token_block(fields: &[Field<'_>]) -> Option<TokenBlock> {
    if let Some(b) = token_block_from(fields) {
        return Some(b);
    }
    for f in fields {
        if let Some(nested) = &f.nested {
            if let Some(b) = legacy_token_block(nested) {
                return Some(b);
            }
        }
    }
    None
}

/// gen_metadata 载荷 → (token 块, 模型名)。新版走 chat_model 字段,旧记录
/// 退回递归找 ModelUsageStats;模型名 display_name 优先于 response_model。
pub(crate) fn extract_generation_usage(data: &[u8]) -> Option<TokenBlock> {
    let fields = parse(data);
    if let Some(chat) = chat_model_fields(&fields) {
        let mut block = find(chat, AG_CHAT_MODEL_METADATA_USAGE)
            .and_then(|f| f.nested.as_ref())
            .and_then(|usage| token_block_from(usage))?;
        let mut model = chat_model_name(chat);
        if model.is_empty() {
            model = legacy_model_name(&fields, AG_CHAT_MODEL_METADATA_DISPLAY_NAME);
        }
        if model.is_empty() {
            model = legacy_model_name(&fields, AG_CHAT_MODEL_METADATA_RESPONSE_MODEL);
        }
        block.model = model;
        return Some(block);
    }
    let mut block = legacy_token_block(&fields)?;
    if block.model.is_empty() {
        block.model = legacy_model_name(&fields, AG_CHAT_MODEL_METADATA_DISPLAY_NAME);
    }
    if block.model.is_empty() {
        block.model = legacy_model_name(&fields, AG_CHAT_MODEL_METADATA_RESPONSE_MODEL);
    }
    Some(block)
}

// ---- 工具调用启发式 ----

/// Antigravity 实际使用的工具名全集;只认精确命中,避免泛词误报
const KNOWN_TOOLS: &[&str] = &[
    "view_file",
    "read_url_content",
    "replace_file_content",
    "multi_replace_file_content",
    "write_to_file",
    "define_subagent",
    "invoke_subagent",
    "manage_subagents",
    "send_message",
    "manage_task",
    "ask_permission",
    "ask_question",
    "schedule",
    "search_web",
    "generate_image",
    "run_command",
    "execute_command",
    "run_shell_command",
    "grep_search",
    "search_files",
    "list_directory",
    "edit_file",
    "read_file",
    "write_file",
];

fn is_known_tool(s: &str) -> bool {
    KNOWN_TOOLS.contains(&s)
}

fn uuid_like(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, &c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == b'-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

/// 从字符串流里抽工具调用:已知工具名 + 邻近 UUID 作 id、邻近 JSON 对象作输入。
/// 邻居窗口内出现别的工具名则不认(不偷别的调用的 id/输入)
pub(crate) fn extract_tool_calls(
    step_idx: i64,
    fields: &[Field<'_>],
) -> Vec<(String, String, Option<String>)> {
    let all = collect_strings(fields, 1);
    let mut calls = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, s) in all.iter().enumerate() {
        if !is_known_tool(s) {
            continue;
        }
        let neighbour = |offsets: &[isize], pred: &dyn Fn(&str) -> bool| -> Option<String> {
            for &off in offsets {
                let j = i as isize + off;
                if j < 0 || j as usize >= all.len() {
                    continue;
                }
                // 中途出现工具名说明这邻居属于另一次调用
                let (lo, hi) = if off > 0 {
                    (i + 1, j as usize)
                } else {
                    (j as usize + 1, i)
                };
                if all[lo..hi].iter().any(|x| is_known_tool(x)) {
                    continue;
                }
                if pred(&all[j as usize]) {
                    return Some(all[j as usize].clone());
                }
            }
            None
        };
        let id = neighbour(&[1, 2, -1, -2], &|x| uuid_like(x))
            .unwrap_or_else(|| format!("ag-step-{step_idx}-{i}"));
        let input = neighbour(&[1, 2, -1], &|x| x.trim_start().starts_with('{'));
        let key = format!("{s}\u{0}{id}\u{0}{}", input.as_deref().unwrap_or(""));
        if !seen.insert(key) {
            continue;
        }
        calls.push((s.clone(), id, input));
    }
    calls
}

// ---- 内容噪声过滤(agentsview 同源判据)----

fn is_noisy(s: &str) -> bool {
    if s.is_empty() || uuid_like(s) {
        return true;
    }
    if s.starts_with("MODEL_PLACEHOLDER_") {
        return true;
    }
    if s.starts_with('{')
        && (s.contains("\"toolAction\"") || s.contains("\"toolSummary\"") || s.contains("\"DirectoryPath\""))
    {
        return true;
    }
    if looks_like_opaque_id(s) {
        return true;
    }
    if s.starts_with("file:///home/") {
        return true;
    }
    if (s.starts_with("/home/") || s.starts_with("/Users/") || s.starts_with("C:\\Users\\"))
        && (s.contains("/.gemini/") || s.contains("\\.gemini\\"))
    {
        return true;
    }
    ["command(", "execute_url(", "read_url(", "mcp("]
        .iter()
        .any(|p| s.starts_with(p))
}

/// 非用户步里,纯 URL(无空白)是工具回显的元数据噪声
fn is_noisy_non_user(s: &str) -> bool {
    (s.starts_with("http://") || s.starts_with("https://"))
        && !s.contains([' ', '\t', '\n'])
}

fn looks_like_opaque_id(s: &str) -> bool {
    if s.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let n = s.len();
    if !(16..=128).contains(&n) {
        return false;
    }
    let mut alpha = 0;
    let mut digit = 0;
    let mut symbol = 0;
    for c in s.chars() {
        match c {
            'a'..='z' | 'A'..='Z' => alpha += 1,
            '0'..='9' => digit += 1,
            '_' | '-' | '.' => symbol += 1,
            _ => return false,
        }
    }
    if digit == n || digit + symbol == n {
        return true;
    }
    alpha > 0 && digit > 0
}

/// 一步的展示字符串:去重 + 噪声过滤;非用户步再滤纯 URL。
/// 全滤空时回退纯 URL(否则 URL-only 助手步会整条消失)。
pub(crate) fn step_strings(fields: &[Field<'_>], is_user: bool) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut cleaned: Vec<String> = collect_strings(fields, 20)
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !is_noisy(s))
        .filter(|s| is_user || !is_noisy_non_user(s))
        .filter(|s| seen.insert(s.clone()))
        .collect();
    if cleaned.is_empty() {
        seen.clear();
        cleaned = collect_strings(fields, 1)
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| is_noisy_non_user(s))
            .filter(|s| seen.insert(s.clone()))
            .collect();
    }
    cleaned
}

/// 用户步的最佳 prompt 候选(优先含空格的长散文,惩罚 JSON/路径/无字母)
pub(crate) fn best_user_prompt(candidates: &[String]) -> Option<String> {
    let mut best: Option<(i64, &String)> = None;
    for s in candidates {
        let t = s.trim();
        if t.is_empty() || is_noisy(t) {
            continue;
        }
        let mut score = t.len() as i64;
        if t.contains([' ', '\n', '\t']) {
            score += 50;
        }
        if t.starts_with('{') || t.starts_with('[') {
            score -= 100;
        }
        if t.starts_with('/') || t.starts_with("file://") {
            score -= 100;
        }
        if !t.chars().any(|c| c.is_alphabetic()) {
            score -= 100;
        }
        if score > best.map(|(s, _)| s).unwrap_or(-1) {
            best = Some((score, s));
        }
    }
    best.filter(|(score, _)| *score > 0).map(|(_, s)| s.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手工编码一个 {1: 14 (varint), 2: "hello"} 形状的消息
    fn enc_varint(number: u32, value: u64) -> Vec<u8> {
        let mut out = encode_tag(number, WIRE_VARINT);
        out.extend(varint_bytes(value));
        out
    }

    fn enc_bytes(number: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = encode_tag(number, WIRE_BYTES);
        out.extend(varint_bytes(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn encode_tag(number: u32, wire: i32) -> Vec<u8> {
        varint_bytes(((number as u64) << 3) | wire as u64)
    }

    fn varint_bytes(mut v: u64) -> Vec<u8> {
        let mut out = Vec::new();
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        out
    }

    #[test]
    fn walks_varints_bytes_and_nested_messages() {
        let inner = enc_varint(1, 42);
        let data = [enc_varint(2, 14), enc_bytes(3, &inner)].concat();
        let fields = parse(&data);
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].varint, 14);
        let nested = fields[1].nested.as_ref().expect("nested message");
        assert_eq!(nested[0].varint, 42);
        assert_eq!(collect_strings(&fields, 1).len(), 0, "42 不是合法 UTF-8 长串");
    }

    #[test]
    fn strings_and_timestamps_surface_from_the_tree() {
        let ts = [enc_varint(1, 1_700_000_000), enc_varint(2, 500_000_000)].concat();
        let data = [enc_bytes(1, b"user prompt text here"), enc_bytes(2, &ts)].concat();
        let fields = parse(&data);
        assert_eq!(collect_strings(&fields, 4), ["user prompt text here"]);
        assert_eq!(earliest_timestamp_ms(&fields), 1_700_000_000_500); // sec*1000 + nanos/1e6
    }

    #[test]
    fn garbage_does_not_recurse_forever() {
        let data = vec![0xffu8; 4096];
        // 全 0xff 是非法 tag 流,顶层即刻降级返回空
        assert!(parse(&data).is_empty());
    }

    #[test]
    fn uuid_shape_detection_is_strict() {
        assert!(uuid_like("28736201-71ef-414d-ae6f-ae7ce464517d"));
        assert!(!uuid_like("2873620171ef414dae6fae7ce464517d"));
        assert!(!uuid_like("zzzzzzzz-zzzz-zzzz-zzzz-zzzzzzzzzzzz"));
    }
}
