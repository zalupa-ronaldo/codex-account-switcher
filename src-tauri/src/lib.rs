use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, COOKIE, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};
use uuid::Uuid;

const KEYCROP_ORIGIN: &str = "https://keycrop.net";
const KEYRING_SERVICE: &str = "dev.a123.codex-account-switcher";
const KEYRING_USER: &str = "keycrop-cookie";

type AppResult<T> = Result<T, String>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    id: Uuid,
    name: String,
    file_name: String,
    created_at: String,
    #[serde(default)]
    color_name: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AppSnapshot {
    profiles: Vec<Profile>,
    active_id: Option<Uuid>,
    codex_auth_path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyCropStatus {
    connected: bool,
    user: Option<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyCropSyncResult {
    purchases_seen: usize,
    accounts_found: usize,
    imported: usize,
    duplicates: usize,
}

struct Paths {
    codex_dir: PathBuf,
    auth: PathBuf,
    app_dir: PathBuf,
    profiles: PathBuf,
    backups: PathBuf,
    probes: PathBuf,
    index: PathBuf,
}

impl Paths {
    fn discover() -> AppResult<Self> {
        let home = dirs::home_dir().ok_or("Не удалось определить домашний каталог")?;
        let codex_dir = home.join(".codex");
        let app_dir = codex_dir.join("account-switcher");
        Ok(Self {
            auth: codex_dir.join("auth.json"),
            profiles: app_dir.join("profiles"),
            backups: app_dir.join("backups"),
            probes: app_dir.join("query-homes"),
            index: app_dir.join("profiles.json"),
            codex_dir,
            app_dir,
        })
    }

    fn ensure(&self) -> AppResult<()> {
        for dir in [
            &self.codex_dir,
            &self.app_dir,
            &self.profiles,
            &self.backups,
            &self.probes,
        ] {
            fs::create_dir_all(dir).map_err(error_string)?;
            secure_directory(dir)?;
        }
        Ok(())
    }
}

fn error_string(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(unix)]
fn secure_directory(path: &Path) -> AppResult<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(error_string)
}

#[cfg(not(unix))]
fn secure_directory(_: &Path) -> AppResult<()> {
    Ok(())
}

fn secure_write(path: &Path, bytes: &[u8]) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(error_string)?;
    }
    let temporary = path.with_extension(format!("tmp-{}", Uuid::new_v4()));
    fs::write(&temporary, bytes).map_err(error_string)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).map_err(error_string)?;
    }
    if cfg!(windows) && path.exists() {
        fs::remove_file(path).map_err(error_string)?;
    }
    fs::rename(&temporary, path).map_err(error_string)
}

fn read_profiles(paths: &Paths) -> AppResult<Vec<Profile>> {
    if !paths.index.exists() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(&paths.index).map_err(error_string)?;
    serde_json::from_slice(&bytes).map_err(|error| format!("profiles.json повреждён: {error}"))
}

fn write_profiles(paths: &Paths, profiles: &[Profile]) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(profiles).map_err(error_string)?;
    secure_write(&paths.index, &bytes)
}

fn canonical(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn identity(value: &Value) -> Option<String> {
    value
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| format!("account:{value}"))
        .or_else(|| {
            value
                .pointer("/_meta/email")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| format!("email:{}", value.to_lowercase()))
        })
}

fn validate_auth_json(text: &str) -> AppResult<Value> {
    let value: Value =
        serde_json::from_str(text).map_err(|error| format!("Некорректный JSON: {error}"))?;
    if !value.is_object() {
        return Err("auth.json должен содержать JSON-объект".into());
    }
    Ok(value)
}

fn profile_value(paths: &Paths, profile: &Profile) -> AppResult<Value> {
    let bytes = fs::read(paths.profiles.join(&profile.file_name)).map_err(error_string)?;
    serde_json::from_slice(&bytes).map_err(error_string)
}

fn is_duplicate(paths: &Paths, profiles: &[Profile], candidate: &Value) -> AppResult<bool> {
    let candidate_id = identity(candidate);
    let candidate_json = canonical(candidate);
    for profile in profiles {
        let saved = profile_value(paths, profile)?;
        if candidate_id.is_some() && identity(&saved) == candidate_id {
            return Ok(true);
        }
        if canonical(&saved) == candidate_json {
            return Ok(true);
        }
    }
    Ok(false)
}

fn suggested_name(value: &Value, fallback: &str) -> String {
    value
        .pointer("/_meta/email")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/tokens/account_id").and_then(Value::as_str))
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

fn add_value(
    paths: &Paths,
    profiles: &mut Vec<Profile>,
    name: &str,
    value: &Value,
) -> AppResult<bool> {
    if is_duplicate(paths, profiles, value)? {
        return Ok(false);
    }
    let file_name = format!("{}.json", Uuid::new_v4());
    let profile = Profile {
        id: Uuid::new_v4(),
        name: if name.trim().is_empty() {
            suggested_name(value, "Codex аккаунт")
        } else {
            name.trim().to_owned()
        },
        file_name: file_name.clone(),
        created_at: Utc::now().to_rfc3339(),
        color_name: None,
    };
    secure_write(
        &paths.profiles.join(file_name),
        &serde_json::to_vec_pretty(value).map_err(error_string)?,
    )?;
    profiles.push(profile);
    write_profiles(paths, profiles)?;
    Ok(true)
}

fn detect_active(paths: &Paths, profiles: &[Profile]) -> Option<Uuid> {
    let current_bytes = fs::read(&paths.auth).ok()?;
    let current: Value = serde_json::from_slice(&current_bytes).ok()?;
    let current_id = identity(&current);
    let matched = profiles.iter().find_map(|profile| {
        let saved = profile_value(paths, profile).ok()?;
        if canonical(&saved) == canonical(&current)
            || (current_id.is_some() && identity(&saved) == current_id)
        {
            Some(profile.id)
        } else {
            None
        }
    });
    if let Some(profile_id) = matched {
        if let Some(profile) = profiles.iter().find(|profile| profile.id == profile_id) {
            let destination = paths.profiles.join(&profile.file_name);
            if fs::read(&destination).ok().as_deref() != Some(current_bytes.as_slice()) {
                let _ = secure_write(&destination, &current_bytes);
            }
        }
    }
    matched
}

fn start_auth_refresh_interceptor() {
    thread::spawn(|| loop {
        if let Ok(paths) = Paths::discover() {
            if let Ok(profiles) = read_profiles(&paths) {
                let _ = detect_active(&paths, &profiles);
            }
        }
        thread::sleep(Duration::from_secs(2));
    });
}

fn ensure_initial_profile(paths: &Paths, profiles: &mut Vec<Profile>) -> AppResult<()> {
    if profiles.is_empty() && paths.auth.exists() {
        let text = fs::read_to_string(&paths.auth).map_err(error_string)?;
        let value = validate_auth_json(&text)?;
        add_value(paths, profiles, "Текущий аккаунт", &value)?;
    }
    Ok(())
}

#[tauri::command]
fn load_snapshot() -> AppResult<AppSnapshot> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let mut profiles = read_profiles(&paths)?;
    ensure_initial_profile(&paths, &mut profiles)?;
    Ok(AppSnapshot {
        active_id: detect_active(&paths, &profiles),
        codex_auth_path: paths.auth.display().to_string(),
        profiles,
    })
}

#[tauri::command]
fn add_profile_json(name: String, json: String) -> AppResult<()> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let value = validate_auth_json(&json)?;
    let mut profiles = read_profiles(&paths)?;
    if !add_value(&paths, &mut profiles, &name, &value)? {
        return Err("Этот Codex-аккаунт уже добавлен".into());
    }
    Ok(())
}

#[tauri::command]
fn activate_profile(profile_id: Uuid) -> AppResult<()> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let profiles = read_profiles(&paths)?;
    let profile = profiles
        .iter()
        .find(|profile| profile.id == profile_id)
        .ok_or("Профиль не найден")?;
    let bytes = fs::read(paths.profiles.join(&profile.file_name)).map_err(error_string)?;
    validate_auth_json(std::str::from_utf8(&bytes).map_err(error_string)?)?;
    if paths.auth.exists() {
        let stamp = Utc::now().format("%Y-%m-%d_%H-%M-%S-%3f");
        secure_write(
            &paths.backups.join(format!("auth_{stamp}.json")),
            &fs::read(&paths.auth).map_err(error_string)?,
        )?;
    }
    secure_write(&paths.auth, &bytes)
}

#[tauri::command]
fn delete_profile(profile_id: Uuid) -> AppResult<()> {
    let paths = Paths::discover()?;
    let mut profiles = read_profiles(&paths)?;
    let index = profiles
        .iter()
        .position(|profile| profile.id == profile_id)
        .ok_or("Профиль не найден")?;
    let profile = profiles.remove(index);
    let source = paths.profiles.join(profile.file_name);
    if source.exists() {
        let destination = paths.backups.join(format!(
            "deleted_{}_{}.json",
            profile.id,
            Utc::now().timestamp()
        ));
        fs::rename(source, destination).map_err(error_string)?;
    }
    write_profiles(&paths, &profiles)
}

fn keyring_entry() -> AppResult<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(error_string)
}

fn normalized_cookie(raw: &str) -> AppResult<String> {
    // Поддерживаем как обычный Cookie header, так и JSON-экспорт
    // EditThisCookie/Cookie-Editor. HTML entities и markdown-экранирование
    // встречаются при копировании через мессенджеры.
    let cleaned = raw
        .replace("&#x20;", " ")
        .replace("&#32;", " ")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
        .replace("\\_", "_");
    let trimmed = cleaned.trim();
    if trimmed.starts_with('[') {
        let cookies: Vec<Value> = serde_json::from_str(trimmed)
            .map_err(|error| format!("Не удалось прочитать экспорт EditThisCookie: {error}"))?;
        let now = Utc::now().timestamp() as f64;
        let mut pairs = Vec::new();
        for cookie in cookies {
            let domain = cookie
                .get("domain")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let domain = domain.trim_start_matches('.').to_lowercase();
            if domain != "keycrop.net" {
                continue;
            }
            if cookie
                .get("expirationDate")
                .and_then(Value::as_f64)
                .is_some_and(|expires| expires <= now)
            {
                continue;
            }
            let Some(name) = cookie
                .get("name")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            let Some(value) = cookie.get("value").and_then(Value::as_str) else {
                continue;
            };
            if name.contains([';', '\n', '\r', '=']) || value.contains([';', '\n', '\r']) {
                continue;
            }
            pairs.push(format!("{name}={value}"));
        }
        if pairs.is_empty() {
            return Err("В экспорте нет действующих cookies для keycrop.net".into());
        }
        return Ok(pairs.join("; "));
    }
    let value = trimmed
        .strip_prefix("Cookie:")
        .or_else(|| trimmed.strip_prefix("cookie:"))
        .unwrap_or(trimmed)
        .trim();
    if value.is_empty() || !value.contains('=') || value.contains('\n') || value.contains('\r') {
        return Err("Вставь значение заголовка Cookie целиком, без переносов строк".into());
    }
    Ok(value.to_owned())
}

fn saved_cookie() -> AppResult<String> {
    keyring_entry()?
        .get_password()
        .map_err(|_| "Сессия KeyCrop ещё не сохранена".to_owned())
}

fn keycrop_client(cookie: &str) -> AppResult<reqwest::Client> {
    let mut headers = HeaderMap::new();
    let mut cookie_value = HeaderValue::from_str(cookie).map_err(error_string)?;
    cookie_value.set_sensitive(true);
    headers.insert(COOKIE, cookie_value);
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("CodexAccountSwitcher/2.0"),
    );
    reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(error_string)
}

async fn request_keycrop(cookie: &str, path: &str) -> AppResult<Value> {
    if !path.starts_with('/') {
        return Err("Некорректный путь KeyCrop API".into());
    }
    let response = keycrop_client(cookie)?
        .get(format!("{KEYCROP_ORIGIN}{path}"))
        .send()
        .await
        .map_err(error_string)?;
    let status = response.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        return Err("Cookie KeyCrop истекла или недействительна".into());
    }
    if !status.is_success() {
        return Err(format!("KeyCrop ответил HTTP {status}"));
    }
    response
        .json::<Value>()
        .await
        .map_err(|error| format!("KeyCrop вернул не JSON: {error}"))
}

fn status_from_me(value: Value) -> AppResult<KeyCropStatus> {
    let user =
        value
            .get("user")
            .cloned()
            .or_else(|| if value.is_object() { Some(value) } else { None });
    if user.as_ref().is_none_or(Value::is_null) {
        return Err("KeyCrop не подтвердил авторизацию".into());
    }
    Ok(KeyCropStatus {
        connected: true,
        user,
    })
}

#[tauri::command]
async fn keycrop_connect(cookie: String) -> AppResult<KeyCropStatus> {
    let cookie = normalized_cookie(&cookie)?;
    let status = status_from_me(request_keycrop(&cookie, "/api/auth/me").await?)?;
    keyring_entry()?
        .set_password(&cookie)
        .map_err(error_string)?;
    Ok(status)
}

#[tauri::command]
async fn keycrop_status() -> AppResult<KeyCropStatus> {
    let cookie = match saved_cookie() {
        Ok(cookie) => cookie,
        Err(_) => {
            return Ok(KeyCropStatus {
                connected: false,
                user: None,
            })
        }
    };
    match request_keycrop(&cookie, "/api/auth/me")
        .await
        .and_then(status_from_me)
    {
        Ok(status) => Ok(status),
        Err(_) => Ok(KeyCropStatus {
            connected: false,
            user: None,
        }),
    }
}

#[tauri::command]
async fn keycrop_sync_accounts() -> AppResult<KeyCropSyncResult> {
    let cookie = saved_cookie()?;
    let paths = Paths::discover()?;
    paths.ensure()?;
    let mut profiles = read_profiles(&paths)?;
    let mut page = 1_u32;
    let mut total_pages = 1_u32;
    let mut purchases_seen = 0_usize;
    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

    while page <= total_pages {
        let payload = request_keycrop(
            &cookie,
            &format!("/api/account/purchases?page={page}&limit=8"),
        )
        .await?;
        purchases_seen += payload
            .get("purchases")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        total_pages = payload
            .pointer("/pagination/pages")
            .and_then(Value::as_u64)
            .unwrap_or(1)
            .clamp(1, 10_000) as u32;
        collect_auth_values(&payload, &mut candidates, &mut seen);
        page += 1;
    }

    let accounts_found = candidates.len();
    let mut imported = 0;
    for (index, candidate) in candidates.iter().enumerate() {
        let name = suggested_name(candidate, &format!("KeyCrop #{}", index + 1));
        if add_value(&paths, &mut profiles, &name, candidate)? {
            imported += 1;
        }
    }
    Ok(KeyCropSyncResult {
        purchases_seen,
        accounts_found,
        imported,
        duplicates: accounts_found.saturating_sub(imported),
    })
}

fn looks_like_codex_auth(value: &Value) -> bool {
    let Some(tokens) = value.get("tokens").and_then(Value::as_object) else {
        return false;
    };
    tokens.contains_key("account_id")
        || tokens.contains_key("access_token")
        || tokens.contains_key("refresh_token")
}

fn collect_auth_values(value: &Value, found: &mut Vec<Value>, seen: &mut HashSet<String>) {
    if looks_like_codex_auth(value) {
        let key = identity(value).unwrap_or_else(|| canonical(value));
        if seen.insert(key) {
            found.push(value.clone());
        }
        return;
    }
    match value {
        Value::Array(items) => {
            for item in items {
                collect_auth_values(item, found, seen);
            }
        }
        Value::Object(object) => {
            for child in object.values() {
                collect_auth_values(child, found, seen);
            }
        }
        Value::String(text) if text.trim_start().starts_with('{') => {
            if let Ok(parsed) = serde_json::from_str::<Value>(text) {
                collect_auth_values(&parsed, found, seen);
            }
        }
        _ => {}
    }
}

fn find_codex_binary() -> AppResult<PathBuf> {
    let executable = if cfg!(windows) { "codex.cmd" } else { "codex" };
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(executable);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    let mut candidates = vec![
        PathBuf::from("/opt/homebrew/bin/codex"),
        PathBuf::from("/usr/local/bin/codex"),
    ];
    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".local/bin/codex"));
        candidates.push(home.join("AppData/Roaming/npm/codex.cmd"));
    }
    candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| "Codex CLI не найден. Установи его и перезапусти приложение.".into())
}

fn probe_rate_limits_sync(profile_id: Uuid) -> AppResult<Value> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let profiles = read_profiles(&paths)?;
    let profile = profiles
        .iter()
        .find(|profile| profile.id == profile_id)
        .ok_or("Профиль не найден")?;
    let probe_home = paths.probes.join(profile.id.to_string());
    fs::create_dir_all(&probe_home).map_err(error_string)?;
    secure_directory(&probe_home)?;
    let profile_path = paths.profiles.join(&profile.file_name);
    let profile_auth = fs::read(&profile_path).map_err(error_string)?;
    let profile_identity = serde_json::from_slice::<Value>(&profile_auth)
        .ok()
        .and_then(|value| identity(&value));
    secure_write(&probe_home.join("auth.json"), &profile_auth)?;
    let config = paths.codex_dir.join("config.toml");
    if config.exists() {
        secure_write(
            &probe_home.join("config.toml"),
            &fs::read(config).map_err(error_string)?,
        )?;
    }

    let binary = find_codex_binary()?;
    let mut command = if cfg!(windows) {
        let mut cmd = Command::new("cmd");
        cmd.args([
            "/C",
            binary.to_string_lossy().as_ref(),
            "app-server",
            "--stdio",
        ]);
        cmd
    } else {
        let mut cmd = Command::new(binary);
        cmd.args(["app-server", "--stdio"]);
        cmd
    };
    let mut child = command
        .env("CODEX_HOME", &probe_home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(error_string)?;
    let mut stdin = child.stdin.take().ok_or("Codex CLI не открыл stdin")?;
    let stdout = child.stdout.take().ok_or("Codex CLI не открыл stdout")?;
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(Value::as_i64) == Some(2) {
                let _ = sender.send(value);
                break;
            }
        }
    });
    stdin.write_all(b"{\"id\":1,\"method\":\"initialize\",\"params\":{\"clientInfo\":{\"name\":\"codex-account-switcher\",\"version\":\"2.0\"}}}\n").map_err(error_string)?;
    stdin.flush().map_err(error_string)?;
    thread::sleep(Duration::from_millis(300));
    stdin.write_all(b"{\"method\":\"initialized\"}\n{\"id\":2,\"method\":\"account/rateLimits/read\",\"params\":null}\n").map_err(error_string)?;
    stdin.flush().map_err(error_string)?;
    let response = receiver.recv_timeout(Duration::from_secs(8));
    let _ = child.kill();
    let _ = child.wait();
    let _ = reader.join();
    let value = response.map_err(|_| "Codex не вернул лимиты за 8 секунд")?;
    if let Some(error) = value.get("error") {
        return Err(format!("Codex отклонил профиль: {error}"));
    }
    let rate_limits = value
        .pointer("/result/rateLimits")
        .cloned()
        .ok_or_else(|| "В ответе Codex нет rateLimits".to_owned())?;

    // app-server может ротировать access/refresh token во время запроса.
    // Возвращаем свежий auth в профиль только после успешного ответа и
    // только если это всё ещё тот же аккаунт.
    if let Ok(refreshed) = fs::read(probe_home.join("auth.json")) {
        if refreshed != profile_auth {
            if let Ok(value) = serde_json::from_slice::<Value>(&refreshed) {
                if profile_identity.is_some() && identity(&value) == profile_identity {
                    secure_write(&profile_path, &refreshed)?;
                }
            }
        }
    }
    Ok(rate_limits)
}

#[tauri::command]
async fn probe_rate_limits(profile_id: Uuid) -> AppResult<Value> {
    tauri::async_runtime::spawn_blocking(move || probe_rate_limits_sync(profile_id))
        .await
        .map_err(error_string)?
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    start_auth_refresh_interceptor();
    tauri::Builder::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .invoke_handler(tauri::generate_handler![
            load_snapshot,
            add_profile_json,
            activate_profile,
            delete_profile,
            keycrop_connect,
            keycrop_status,
            keycrop_sync_accounts,
            probe_rate_limits,
        ])
        .run(tauri::generate_context!())
        .expect("failed to run Codex Account Switcher");
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn finds_nested_stringified_auth() {
        let payload =
            json!({"purchases": [{"delivery": "{\"tokens\":{\"account_id\":\"acc_1\"}}"}]});
        let mut found = Vec::new();
        collect_auth_values(&payload, &mut found, &mut HashSet::new());
        assert_eq!(found.len(), 1);
        assert_eq!(identity(&found[0]).as_deref(), Some("account:acc_1"));
    }

    #[test]
    fn cookie_header_is_normalized() {
        assert_eq!(
            normalized_cookie("Cookie: session=abc; theme=dark").unwrap(),
            "session=abc; theme=dark"
        );
        assert!(normalized_cookie("no-cookie-here").is_err());
    }

    #[test]
    fn edit_this_cookie_export_is_normalized_and_filtered() {
        let export = r#"[
          {"domain":".keycrop.net","name":"__stripe_mid","value":"stripe-test","expirationDate":4102444800},
          {"domain":"keycrop.net","name":"session","value":"session-test","expirationDate":4102444800},
          {"domain":"example.com","name":"ignored","value":"nope","expirationDate":4102444800}
        ]"#;
        assert_eq!(
            normalized_cookie(export).unwrap(),
            "__stripe_mid=stripe-test; session=session-test"
        );
    }

    #[test]
    fn messenger_html_entities_are_supported() {
        let export = r#"[{
          &#x20; "domain":"keycrop.net",
          &#x20; "name":"session",
          &#x20; "value":"redacted_test",
          &#x20; "expirationDate":4102444800
        }]"#;
        assert_eq!(normalized_cookie(export).unwrap(), "session=redacted_test");
    }

    #[test]
    fn active_profile_captures_rotated_tokens() {
        let root = std::env::temp_dir().join(format!("codex-switcher-test-{}", Uuid::new_v4()));
        let paths = Paths {
            codex_dir: root.join(".codex"),
            auth: root.join(".codex/auth.json"),
            app_dir: root.join(".codex/account-switcher"),
            profiles: root.join(".codex/account-switcher/profiles"),
            backups: root.join(".codex/account-switcher/backups"),
            probes: root.join(".codex/account-switcher/query-homes"),
            index: root.join(".codex/account-switcher/profiles.json"),
        };
        paths.ensure().unwrap();
        let profile = Profile {
            id: Uuid::new_v4(),
            name: "test".into(),
            file_name: "test.json".into(),
            created_at: Utc::now().to_rfc3339(),
            color_name: None,
        };
        let old = br#"{"tokens":{"account_id":"acc_1","access_token":"old"}}"#;
        let refreshed = br#"{"tokens":{"account_id":"acc_1","access_token":"fresh"}}"#;
        secure_write(&paths.profiles.join(&profile.file_name), old).unwrap();
        secure_write(&paths.auth, refreshed).unwrap();
        write_profiles(&paths, std::slice::from_ref(&profile)).unwrap();

        assert_eq!(detect_active(&paths, &[profile.clone()]), Some(profile.id));
        assert_eq!(
            fs::read(paths.profiles.join(profile.file_name)).unwrap(),
            refreshed
        );
        fs::remove_dir_all(root).unwrap();
    }
}
