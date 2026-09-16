//! User-defined OpenAI-compatible providers; no OpenCode files or credentials are changed.
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use url::{Host, Url};

const CONFIG_ENV: &str = "OPENCODE_CONFIG_CONTENT";
const MAX_RESPONSE: u64 = 2 * 1024 * 1024;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderModel {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderHeader {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Provider {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub models: Vec<ProviderModel>,
    pub headers: Vec<ProviderHeader>,
}

fn storage_path() -> Result<PathBuf, String> {
    crate::prefs::dir()
        .map(|dir| dir.join("providers.json"))
        .ok_or_else(|| "The preferences directory is unavailable; providers were not saved.".into())
}

pub fn load() -> Result<Vec<Provider>, String> {
    load_path(&storage_path()?)
}

/// Persist drafts without models; only OpenCode injection requires a selected model.
pub fn save(providers: &[Provider]) -> Result<(), String> {
    save_path(&storage_path()?, providers)
}

fn load_path(path: &Path) -> Result<Vec<Provider>, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => return Err("Cannot inspect provider storage; check its permissions.".into()),
        Ok(metadata) if !metadata.is_file() => {
            return Err("Provider storage must be a regular file, not a link or directory.".into());
        }
        Ok(_) => {}
    }
    let bytes = fs::read(path)
        .map_err(|_| "Cannot read provider storage; check its permissions.".to_string())?;
    let providers: Vec<Provider> = serde_json::from_slice(&bytes)
        .map_err(|_| "Provider storage is malformed; repair it before saving changes.".to_string())?;
    validate_all(&providers, false)?;
    Ok(providers)
}

fn save_path(path: &Path, providers: &[Provider]) -> Result<(), String> {
    validate_all(providers, false)?;
    // Refuse to turn a failed load into a successful overwrite of damaged storage.
    load_path(path)?;
    let bytes = serde_json::to_vec_pretty(providers)
        .map_err(|_| "Cannot encode provider settings.".to_string())?;
    let parent = path.parent().ok_or("Provider storage has no parent directory.")?;
    fs::create_dir_all(parent)
        .map_err(|_| "Cannot create the provider storage directory.".to_string())?;
    let (temporary, mut file) = create_temporary(parent)
        .map_err(|_| "Cannot create private provider storage; check directory permissions.".to_string())?;
    let written = file.write_all(&bytes).and_then(|_| file.sync_all());
    drop(file);
    let result = written.and_then(|_| fs::rename(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err("Cannot save provider settings atomically; previous settings were retained.".into());
    }
    Ok(())
}

fn create_temporary(parent: &Path) -> io::Result<(PathBuf, File)> {
    for _ in 0..128 {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".providers-{}-{id}.tmp", std::process::id()));
        match private_file(&path) {
            Ok(file) => return Ok((path, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::from(io::ErrorKind::AlreadyExists))
}

#[cfg(unix)]
fn private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    // Set the exact mode before writing, even with an unusually restrictive umask.
    if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
        drop(file);
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(file)
}

#[cfg(windows)]
fn private_file(path: &Path) -> io::Result<File> {
    use std::ffi::c_void;
    use std::os::windows::{ffi::OsStrExt, io::FromRawHandle};
    #[repr(C)]
    struct SecurityAttributes {
        length: u32,
        descriptor: *mut c_void,
        inherit: i32,
    }
    #[link(name = "advapi32")]
    extern "system" {
        fn ConvertStringSecurityDescriptorToSecurityDescriptorW(
            text: *const u16, revision: u32, descriptor: *mut *mut c_void, size: *mut u32,
        ) -> i32;
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateFileW(name: *const u16, access: u32, share: u32,
            security: *const SecurityAttributes, disposition: u32, flags: u32,
            template: *mut c_void) -> *mut c_void;
        fn LocalFree(memory: *mut c_void) -> *mut c_void;
    }
    let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    if name[..name.len() - 1].contains(&0) {
        return Err(io::Error::from(io::ErrorKind::InvalidInput));
    }
    // Protected DACL: only the object's owner receives access, with no inherited ACEs.
    let sddl: Vec<u16> = "D:P(A;;FA;;;OW)\0".encode_utf16().collect();
    let mut descriptor = std::ptr::null_mut();
    unsafe {
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(), 1, &mut descriptor, std::ptr::null_mut(),
        ) == 0 {
            return Err(io::Error::last_os_error());
        }
        let security = SecurityAttributes {
            length: std::mem::size_of::<SecurityAttributes>() as u32, descriptor, inherit: 0,
        };
        // GENERIC_WRITE, no sharing, CREATE_NEW, FILE_ATTRIBUTE_NORMAL.
        let handle = CreateFileW(name.as_ptr(), 0x40000000, 0, &security, 1, 0x80, std::ptr::null_mut());
        let error = io::Error::last_os_error();
        LocalFree(descriptor);
        if handle as isize == -1 {
            Err(error)
        } else {
            Ok(File::from_raw_handle(handle))
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn private_file(_: &Path) -> io::Result<File> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "Private storage is unsupported."))
}

fn base_url(provider: &Provider) -> Result<Url, String> {
    let text = provider.base_url.trim().trim_end_matches('/');
    let invalid = "Use an HTTP(S) API base URL without credentials, query, or fragment.";
    let url = Url::parse(text).map_err(|_| invalid.to_string())?;
    let authority = text.split_once("://").map(|(_, rest)| rest.split('/').next().unwrap_or(""));
    if !matches!(url.scheme(), "http" | "https") || url.host().is_none()
        || !url.username().is_empty() || url.password().is_some()
        || url.query().is_some() || url.fragment().is_some()
        || authority.is_none_or(|value| value.contains('@'))
        || text.contains('\\') || text.chars().any(char::is_control)
    {
        return Err(invalid.into());
    }
    let loopback = match url.host() {
        Some(Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(Host::Ipv4(ip)) => ip.is_loopback(),
        Some(Host::Ipv6(ip)) => ip.is_loopback(),
        _ => false,
    };
    if url.scheme() == "http" && !loopback {
        return Err("Use HTTPS for remote providers; HTTP is allowed only on loopback hosts.".into());
    }
    Ok(url)
}

fn header_value(value: &str) -> Result<HeaderValue, String> {
    let value = HeaderValue::from_str(value)
        .map_err(|_| "API keys and header values must be valid HTTP header text.".to_string())?;
    value.to_str()
        .map_err(|_| "API keys and header values must contain only HTTP header text.".to_string())?;
    Ok(value)
}

fn request_headers(provider: &Provider) -> Result<HeaderMap, String> {
    let mut headers = HeaderMap::new();
    header_value(&provider.api_key)?;
    if !provider.api_key.is_empty() {
        let mut value = header_value(&format!("Bearer {}", provider.api_key))?;
        value.set_sensitive(true);
        headers.insert(AUTHORIZATION, value);
    }
    let mut names = HashSet::new();
    for header in &provider.headers {
        let name = HeaderName::from_bytes(header.name.as_bytes())
            .map_err(|_| "Each custom header needs a valid HTTP header name.".to_string())?;
        if matches!(name.as_str(), "host" | "content-length" | "transfer-encoding" | "connection"
            | "upgrade" | "keep-alive" | "proxy-authorization" | "proxy-authenticate" | "proxy-connection" | "te"
            | "trailer" | "expect" | "accept-encoding" | "content-encoding")
        {
            return Err("Custom headers cannot override HTTP transport or proxy headers.".into());
        }
        if !names.insert(name.clone()) {
            return Err("Custom header names must be unique (case-insensitive).".into());
        }
        let mut value = header_value(&header.value)?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(headers)
}

/// The caller removes wholly blank UI rows. Empty model names fall back to their IDs.
pub fn validate(provider: &Provider, require_models: bool) -> Result<(), String> {
    if provider.id.is_empty() || provider.id.len() > 64
        || !provider.id.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"_-".contains(&c))
    {
        return Err("Provider ID must be 1–64 lowercase letters, digits, underscores, or hyphens.".into());
    }
    if provider.name.trim().is_empty() {
        return Err("A provider name is required.".into());
    }
    base_url(provider)?;
    request_headers(provider)?;
    let mut models = HashSet::new();
    for model in &provider.models {
        if model.id.trim().is_empty() {
            return Err("Each model needs a nonempty ID.".into());
        }
        if !models.insert(&model.id) {
            return Err("Model IDs must be unique within a provider.".into());
        }
    }
    if require_models && provider.models.is_empty() {
        return Err("Add or discover at least one model before using this provider in OpenCode.".into());
    }
    Ok(())
}

fn validate_all(providers: &[Provider], require_models: bool) -> Result<(), String> {
    let mut ids = HashSet::new();
    for provider in providers {
        validate(provider, require_models)?;
        if !ids.insert(&provider.id) {
            return Err("Provider IDs must be unique.".into());
        }
    }
    Ok(())
}

fn display_name<'a>(name: &'a str, id: &'a str) -> &'a str {
    if name.trim().is_empty() { id } else { name }
}

/// Blocking: call on a dedicated worker thread, never inside a Tokio/GPUI runtime.
pub fn discover_models(provider: &Provider) -> Result<Vec<ProviderModel>, String> {
    validate(provider, false)?;
    let mut url = base_url(provider)?;
    let path = format!("{}/models", url.path().trim_end_matches('/'));
    url.set_path(&path);
    let builder = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none());
    // Plain HTTP is loopback-only; never forward its credentials to a system proxy.
    let builder = if url.scheme() == "http" { builder.no_proxy() } else { builder };
    let client = builder.build()
        .map_err(|_| "Cannot initialize the provider HTTP client.".to_string())?;
    let response = client.get(url).headers(request_headers(provider)?)
        .send().map_err(|error| if error.is_timeout() {
            "Model discovery timed out after 15 seconds; check the endpoint and retry.".to_string()
        } else {
            "Cannot reach the provider; check its base URL, TLS certificate, and network.".to_string()
        })?;
    match response.status().as_u16() {
        200..=299 => {}
        401 | 403 => return Err("Provider denied access; check the API key and Authorization header.".into()),
        404 => return Err("Models endpoint not found; check the API base URL (include /v1 if required).".into()),
        300..=399 => return Err("Provider redirected model discovery; use the final API base URL. Redirects are blocked to protect credentials.".into()),
        429 => return Err("Provider rate limit reached; retry model discovery later.".into()),
        status => return Err(format!("Model discovery failed (HTTP {status}); check provider availability.")),
    }
    if response.content_length().is_some_and(|length| length > MAX_RESPONSE) {
        return Err("Provider model response is too large (maximum 2 MiB).".into());
    }
    let mut bytes = Vec::new();
    response.take(MAX_RESPONSE + 1).read_to_end(&mut bytes)
        .map_err(|_| "Cannot read the model response; retry and check the provider connection.".to_string())?;
    parse_models(&bytes)
}

fn parse_models(bytes: &[u8]) -> Result<Vec<ProviderModel>, String> {
    if bytes.len() as u64 > MAX_RESPONSE {
        return Err("Provider model response is too large (maximum 2 MiB).".into());
    }
    let value: Value = serde_json::from_slice(bytes)
        .map_err(|_| "Provider returned invalid JSON; check the API base URL or add models manually.".to_string())?;
    let rows = value.get("data").or_else(|| value.get("models")).unwrap_or(&value)
        .as_array().ok_or("Expected an OpenAI model list; check the API base URL or add models manually.")?;
    let mut models = BTreeMap::new();
    for row in rows {
        let Some(id) = row.get("id").or_else(|| row.get("model")).or_else(|| row.get("name"))
            .and_then(Value::as_str).map(str::trim).filter(|id| !id.is_empty()) else { continue };
        let name = row.get("name").and_then(Value::as_str).unwrap_or("");
        models.entry(id.to_string()).or_insert_with(|| ProviderModel {
            id: id.to_string(), name: display_name(name, id).to_string(),
        });
    }
    if models.is_empty() {
        return Err("Provider returned no usable models; check access or add model IDs manually.".into());
    }
    Ok(models.into_values().collect())
}

pub fn opencode_id(id: &str) -> String {
    format!("wake-{id}")
}

// Keep existing raw JSON strings intact: reserialization could activate previously
// escaped {env:...}/{file:...} literals or disable intentional user placeholders.
type RawObject = BTreeMap<String, (String, String)>;
fn raw_object(text: &str) -> Result<RawObject, String> {
    let invalid = "OpenCode inline configuration must be a valid JSON object; fix it before adding providers.";
    serde_json::from_str::<Map<String, Value>>(text).map_err(|_| invalid.to_string())?;
    let mut rest = text.trim().strip_prefix('{').ok_or(invalid)?.trim_start();
    let mut fields = BTreeMap::new();
    while !rest.starts_with('}') {
        let mut keys = serde_json::Deserializer::from_str(rest).into_iter::<String>();
        let key = keys.next().ok_or(invalid)?.map_err(|_| invalid.to_string())?;
        let key_end = keys.byte_offset();
        let raw_key = rest[..key_end].to_string();
        rest = rest[key_end..].trim_start().strip_prefix(':').ok_or(invalid)?.trim_start();
        let mut values = serde_json::Deserializer::from_str(rest).into_iter::<Value>();
        values.next().ok_or(invalid)?.map_err(|_| invalid.to_string())?;
        let end = values.byte_offset();
        fields.insert(key, (raw_key, rest[..end].to_string()));
        rest = rest[end..].trim_start();
        rest = rest.strip_prefix(',').unwrap_or(rest).trim_start();
    }
    Ok(fields)
}

fn encode_object(fields: RawObject) -> String {
    let entries: Vec<String> = fields.into_values().map(|(key, value)| format!("{key}:{value}")).collect();
    format!("{{{}}}", entries.join(","))
}

fn is_config_env(name: &str) -> bool {
    if cfg!(windows) { name.eq_ignore_ascii_case(CONFIG_ENV) } else { name == CONFIG_ENV }
}

/// Only mutates the supplied child-process environment, and only after all checks pass.
pub fn merge_opencode_env(providers: &[Provider], env: &mut Vec<(String, String)>) -> Result<(), String> {
    if providers.is_empty() { return Ok(()); }
    let inherited = match env.iter().rev().find(|(name, _)| is_config_env(name)) {
        Some((_, value)) => Some(value.clone()),
        None => match std::env::var(CONFIG_ENV) {
            Ok(value) => Some(value),
            Err(std::env::VarError::NotPresent) => None,
            Err(_) => return Err("Inherited OpenCode inline configuration is not valid Unicode.".into()),
        },
    };
    merge_with_config(providers, env, inherited.as_deref())
}

fn merge_with_config(providers: &[Provider], env: &mut Vec<(String, String)>, content: Option<&str>) -> Result<(), String> {
    if providers.is_empty() { return Ok(()); }
    validate_all(providers, true)?;
    let mut config = raw_object(content.unwrap_or("{}"))?;
    let mut entries = match config.get("provider") {
        Some((_, value)) => raw_object(value)
            .map_err(|_| "The OpenCode provider setting must be a JSON object.".to_string())?,
        None => BTreeMap::new(),
    };
    for provider in providers {
        let id = opencode_id(&provider.id);
        let headers: BTreeMap<_, _> = provider.headers.iter().map(|header| {
            // The SDK spreads an object containing capitalized Authorization first.
            // Match its spelling so custom auth replaces rather than duplicates it.
            let name = if header.name.eq_ignore_ascii_case("authorization") {
                "Authorization".into()
            } else {
                header.name.to_ascii_lowercase()
            };
            (name, &header.value)
        }).collect();
        let models: BTreeMap<_, _> = provider.models.iter()
            .map(|model| (&model.id, json!({"name": display_name(&model.name, &model.id)}))).collect();
        let entry = json!({
            "npm": "@ai-sdk/openai-compatible", "name": provider.name,
            "options": {"baseURL": base_url(provider)?.as_str().trim_end_matches('/'),
                "apiKey": provider.api_key, "headers": headers},
            "models": models,
        });
        // OpenCode substitutes raw text BEFORE JSON parsing. Unicode escapes make
        // arbitrary provider values literal without exposing extra process env vars.
        let safe = entry.to_string().replace("{env:", "\\u007benv:").replace("{file:", "\\u007bfile:");
        entries.insert(id.clone(), (json!(id).to_string(), safe));
    }
    config.insert("provider".into(), ("\"provider\"".into(), encode_object(entries)));
    let merged = encode_object(config);
    env.retain(|(name, _)| !is_config_env(name));
    env.push((CONFIG_ENV.into(), merged));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;
    use tempfile::tempdir;

    fn provider() -> Provider {
        Provider {
            id: "local".into(), name: "Local".into(), base_url: "http://127.0.0.1:11434/v1".into(),
            api_key: String::new(), headers: Vec::new(),
            models: vec![ProviderModel { id: "model-a".into(), name: String::new() }],
        }
    }

    #[test]
    fn persistence_roundtrip_and_damaged_storage_protection() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested/providers.json");
        assert!(load_path(&path).unwrap().is_empty());
        let mut p = provider();
        p.api_key = "private-key".into();
        let expected = vec![p];
        save_path(&path, &expected).unwrap();
        assert!(load_path(&path).unwrap() == expected);
        save_path(&path, &[]).unwrap();
        assert!(load_path(&path).unwrap().is_empty());
        fs::write(&path, "private-key not json").unwrap();
        assert!(!load_path(&path).err().unwrap().contains("private-key"));
        assert!(save_path(&path, &expected).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "private-key not json");
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        assert!(load_path(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn private_permissions_before_writing_and_after_replacement() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempdir().unwrap();
        let (tmp, file) = create_temporary(dir.path()).unwrap();
        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
        drop(file);
        fs::remove_file(tmp).unwrap();
        let path = dir.path().join("providers.json");
        fs::write(&path, "[]").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        save_path(&path, &[provider()]).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        let link = dir.path().join("link.json");
        symlink(&path, &link).unwrap();
        assert!(load_path(&link).is_err());
        assert!(save_path(&link, &[]).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
        // Root can still read mode-000 files, so only assert unreadability when enforced.
        if File::open(&path).is_err() {
            assert!(load_path(&path).is_err());
            assert!(save_path(&path, &[]).is_err());
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(load_path(&path).unwrap() == vec![provider()]);
    }

    #[test]
    fn validation_and_unique_ids() {
        let mut p = provider();
        assert!(validate(&p, true).is_ok());
        for id in ["", "Upper", "a/b", "é", &"a".repeat(65)] {
            p.id = id.into();
            assert!(validate(&p, false).is_err());
        }
        p = provider();
        for url in ["https://user:secret@example.com", "https://@example.com", "https://example.com?q=secret",
            "https://example.com/#secret", "http://example.com", "file:///secret", "not a url"] {
            p.base_url = url.into();
            let error = validate(&p, false).unwrap_err();
            assert!(!error.contains("secret"));
        }
        for url in ["https://example.com/api/v1/", "http://localhost:9000", "http://[::1]:8080", "http://127.2.3.4"] {
            p.base_url = url.into();
            assert!(validate(&p, false).is_ok());
        }
        p.name.clear();
        assert!(validate(&p, false).is_err());
        p.name = "Local".into();
        p.models[0].id = " ".into();
        assert!(validate(&p, false).is_err());
        p.models[0].id = "model-a".into();
        p.models.push(p.models[0].clone());
        assert!(validate(&p, true).is_err());
        p.models.clear();
        assert!(validate(&p, false).is_ok());
        assert!(validate(&p, true).is_err());
        let dir = tempdir().unwrap();
        assert!(save_path(&dir.path().join("providers.json"), &[p.clone(), p]).is_err());
    }

    #[test]
    fn header_validation_and_authorization_precedence() {
        let mut p = provider();
        assert!(request_headers(&p).unwrap().get(AUTHORIZATION).is_none());
        p.api_key = "default-key".into();
        assert_eq!(request_headers(&p).unwrap()[AUTHORIZATION], "Bearer default-key");
        p.headers.push(ProviderHeader { name: "aUtHoRiZaTiOn".into(), value: "Basic override".into() });
        assert_eq!(request_headers(&p).unwrap()[AUTHORIZATION], "Basic override");
        p.headers.push(ProviderHeader { name: "authorization".into(), value: String::new() });
        assert!(validate(&p, false).is_err());
        for name in ["Host", "Content-Length", "Transfer-Encoding", "Connection", "Proxy-Authorization", "bad name", ""] {
            p.headers = vec![ProviderHeader { name: name.into(), value: "secret".into() }];
            assert!(!validate(&p, false).unwrap_err().contains("secret"));
        }
        p.headers = vec![ProviderHeader { name: "x-key".into(), value: "secret\ninvalid".into() }];
        assert!(!validate(&p, false).unwrap_err().contains("secret"));
        p.headers.clear();
        p.api_key = "secret\r\ninjected: yes".into();
        assert!(!validate(&p, false).unwrap_err().contains("secret"));
    }

    #[test]
    fn parsing_deduplication_sorting_and_sanitization() {
        let models = parse_models(br#"{"data":[{"id":"z"},{"id":"a","name":"Alpha"},{"id":"a"},{"id":""},{}]}"#).unwrap();
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a", "z"]);
        assert_eq!(models[0].name, "Alpha");
        assert_eq!(models[1].name, "z");
        assert_eq!(parse_models(br#"{"models":[{"model":"local","name":"Local"}]}"#).unwrap()[0].id, "local");
        assert_eq!(parse_models(br#"[{"id":"direct"}]"#).unwrap()[0].id, "direct");
        for bytes in [b"secret".as_slice(), br#"{"error":"secret"}"#, br#"{"data":[]}"#] {
            assert!(!parse_models(bytes).err().unwrap().contains("secret"));
        }
    }

    // One loopback request, bounded accept/read deadlines, no live provider calls.
    fn fixture(response: Vec<u8>) -> (Provider, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut p = provider();
        p.base_url = format!("http://{}/v1/", listener.local_addr().unwrap());
        let (sender, receiver) = mpsc::channel();
        let task = thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock && std::time::Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
                    Err(error) => panic!("Loopback fixture accept failed: {error}"),
                }
            };
            // macOS can inherit the listener's nonblocking flag on accepted streams.
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            stream.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") && request.len() < 32768 {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            sender.send(String::from_utf8(request).unwrap()).unwrap();
            let _ = stream.write_all(&response);
        });
        (p, receiver, task)
    }

    fn http(status: &str, headers: &str, body: &str) -> Vec<u8> {
        format!("HTTP/1.1 {status}\r\nConnection: close\r\nContent-Length: {}\r\n{headers}\r\n{body}", body.len()).into_bytes()
    }

    #[test]
    fn discovery_optional_auth_and_custom_header_on_wire() {
        for auth in [None, Some("Bearer default-key"), Some("Basic override")] {
            let (mut p, request, task) = fixture(http("200 OK", "", r#"{"data":[{"id":"a"}]}"#));
            if auth.is_some() { p.api_key = "default-key".into(); }
            if auth == Some("Basic override") {
                p.headers.push(ProviderHeader { name: "Authorization".into(), value: "Basic override".into() });
            }
            assert_eq!(discover_models(&p).unwrap()[0].id, "a");
            let request = request.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(request.starts_with("GET /v1/models HTTP/1.1\r\n"));
            match auth {
                Some(auth) => assert!(request.contains(&format!("authorization: {auth}\r\n"))),
                None => assert!(!request.contains("authorization:")),
            }
            task.join().unwrap();
        }
    }

    #[test]
    fn discovery_http_errors_redirects_and_chunked_limit() {
        for status in ["401 Unauthorized", "403 Forbidden", "404 Not Found", "429 Too Many Requests", "500 Failure", "302 Found"] {
            let (p, _request, task) = fixture(http(status, "Location: http://127.0.0.1:1/secret\r\n", "secret"));
            let error = discover_models(&p).err().unwrap();
            assert!(!error.contains("secret") && !error.contains("127.0.0.1"));
            if status.starts_with("302") { assert!(error.contains("Redirects are blocked")); }
            task.join().unwrap();
        }
        let mut response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        response.extend_from_slice(format!("{:x}\r\n", MAX_RESPONSE + 1).as_bytes());
        response.extend(vec![b'x'; MAX_RESPONSE as usize + 1]);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (p, _request, task) = fixture(response);
        let error = discover_models(&p).err().unwrap();
        assert!(error.contains("too large"), "{error}");
        task.join().unwrap();
        for body in ["secret invalid JSON", r#"{"data":[]}"#] {
            let (p, _request, task) = fixture(http("200 OK", "", body));
            assert!(!discover_models(&p).err().unwrap().contains("secret"));
            task.join().unwrap();
        }
        let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", MAX_RESPONSE + 1);
        let (p, _request, task) = fixture(response.into_bytes());
        let error = discover_models(&p).err().unwrap();
        assert!(error.contains("too large"), "{error}");
        task.join().unwrap();
    }

    #[test]
    fn merge_multiple_providers_preserves_config_and_last_explicit_env() {
        let p = provider();
        let mut other = provider();
        other.id = "remote".into();
        other.api_key = "key".into();
        other.base_url = "https://example.com/v1/".into();
        other.headers.push(ProviderHeader { name: "aUtHoRiZaTiOn".into(), value: "Basic override".into() });
        let mut env = vec![("OTHER".into(), "keep".into()), (CONFIG_ENV.into(), "invalid earlier value".into()),
            (CONFIG_ENV.into(), r#"{"model":"existing/model","disabled_providers":["other"],"provider":{"existing":{"options":{"apiKey":"keep"}},"wake-local":{"old":true},"wake-removed":{"keep":true}}}"#.into())];
        merge_opencode_env(&[p, other], &mut env).unwrap();
        assert_eq!(env.iter().filter(|(key, _)| key == CONFIG_ENV).count(), 1);
        assert_eq!(env[0], ("OTHER".into(), "keep".into()));
        let config: Value = serde_json::from_str(&env[1].1).unwrap();
        assert_eq!(config["model"], "existing/model");
        assert_eq!(config["disabled_providers"], json!(["other"]));
        assert_eq!(config["provider"]["existing"]["options"]["apiKey"], "keep");
        assert_eq!(config["provider"]["wake-removed"]["keep"], true);
        assert!(config["provider"]["wake-local"].get("old").is_none());
        assert_eq!(config["provider"]["wake-local"]["options"]["apiKey"], "");
        assert_eq!(config["provider"]["wake-local"]["models"]["model-a"]["name"], "model-a");
        assert_eq!(config["provider"]["wake-remote"]["npm"], "@ai-sdk/openai-compatible");
        assert_eq!(config["provider"]["wake-remote"]["options"]["headers"]["Authorization"], "Basic override");
    }

    #[test]
    fn merge_literal_placeholders_injection_and_failures_are_transactional() {
        let mut p = provider();
        p.name = "{file:/secret}".into();
        p.api_key = "{env:SECRET}".into();
        p.headers.push(ProviderHeader { name: "x-literal".into(), value: "{file:/secret}".into() });
        p.models[0].id = "{env:MODEL}".into();
        let original = r#"{"instructions":["{file:intentional}","\u007bfile:literal}"],"provider":{"old":{"name":"\u007benv:literal}"}},"nested":{"a":[1,{"b":"x,}:\""}]},"literal":false,"number":123,"empty":null}"#;
        let mut env = Vec::new();
        merge_with_config(&[p.clone()], &mut env, None).unwrap();
        let defaults: Value = serde_json::from_str(&env[0].1).unwrap();
        assert!(defaults.get("model").is_none());
        assert_eq!(defaults.as_object().unwrap().len(), 1);
        // Deterministic inherited-config seam; never changes the process environment.
        merge_with_config(&[p.clone()], &mut env, Some(original)).unwrap();
        assert!(env[0].1.contains("{file:intentional}"));
        assert!(env[0].1.contains("\\u007bfile:literal}"));
        assert!(!env[0].1.contains("{env:SECRET}"));
        assert!(!env[0].1.contains("{file:/secret}"));
        let parsed: Value = serde_json::from_str(&env[0].1).unwrap();
        assert_eq!(parsed["provider"]["wake-local"]["options"]["apiKey"], p.api_key);
        let mut second = provider();
        second.id = "second".into();
        merge_opencode_env(&[second], &mut env).unwrap();
        assert!(!env[0].1.contains("{env:SECRET}"));
        for invalid in ["secret invalid json", "[]", "null", r#"{"provider":[]}"#] {
            let mut env = vec![(CONFIG_ENV.into(), invalid.into())];
            let before = env.clone();
            let error = merge_opencode_env(&[p.clone()], &mut env).unwrap_err();
            assert!(!error.contains("secret"));
            assert_eq!(env, before);
            merge_opencode_env(&[], &mut env).unwrap();
            assert_eq!(env, before);
        }
    }
}
