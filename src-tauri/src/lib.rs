use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use chrono::Utc;
use reqwest::header::{
    HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE, COOKIE, ORIGIN, REFERER,
    USER_AGENT,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, OnceLock,
    },
    thread,
    time::Duration,
};
use tiny_http::{Header, Method, Response, Server, StatusCode};
use uuid::Uuid;

const KEYCROP_ORIGIN: &str = "https://keycrop.net";
const KEYRING_SERVICE: &str = "dev.a123.codex-account-switcher";
const KEYRING_USER: &str = "keycrop-cookie";
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OAUTH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CHATGPT_BACKEND: &str = "https://chatgpt.com/backend-api";
const BROKER_PORT: u16 = 1456;
const BROKER_URL: &str = "http://127.0.0.1:1456/oauth/token";

static REFRESH_LOCK: Mutex<()> = Mutex::new(());
static BROKER_STATE: OnceLock<Arc<BrokerRuntime>> = OnceLock::new();

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
    captured: usize,
    rejected: usize,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct TokenStatus {
    email: Option<String>,
    plan_type: Option<String>,
    subscription_until: Option<String>,
    access_expires_at: i64,
    id_expires_at: i64,
    access_seconds_left: i64,
    has_refresh: bool,
    last_refresh: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PaymentMethodInfo {
    label: String,
    last4: Option<String>,
    handle: Option<String>,
    expires: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountLiveInfo {
    token: TokenStatus,
    live_plan: Option<String>,
    country: Option<String>,
    currency: Option<String>,
    payment_methods: Vec<PaymentMethodInfo>,
    warnings: Vec<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BrokerStatus {
    running: bool,
    configured: bool,
    url: String,
    error: Option<String>,
}

#[derive(Default)]
struct BrokerRuntime {
    started: AtomicBool,
    running: AtomicBool,
    error: Mutex<Option<String>>,
}

struct Paths {
    codex_dir: PathBuf,
    auth: PathBuf,
    app_dir: PathBuf,
    profiles: PathBuf,
    backups: PathBuf,
    probes: PathBuf,
    index: PathBuf,
    broker_enabled: PathBuf,
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
            broker_enabled: app_dir.join("broker-enabled"),
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

fn jwt_claims(token: Option<&str>) -> Value {
    let Some(payload) = token.and_then(|token| token.split('.').nth(1)) else {
        return Value::Null;
    };
    URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or(Value::Null)
}

fn token_claims(value: &Value) -> Value {
    jwt_claims(value.pointer("/tokens/id_token").and_then(Value::as_str))
}

fn jwt_exp(token: Option<&str>) -> i64 {
    jwt_claims(token)
        .get("exp")
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

fn token_status(value: &Value) -> TokenStatus {
    let claims = token_claims(value);
    let auth = claims
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object);
    let access_expires_at = jwt_exp(
        value
            .pointer("/tokens/access_token")
            .and_then(Value::as_str),
    );
    TokenStatus {
        email: claims
            .get("email")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .or_else(|| {
                value
                    .pointer("/_meta/email")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            }),
        plan_type: auth
            .and_then(|auth| auth.get("chatgpt_plan_type"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        subscription_until: auth
            .and_then(|auth| auth.get("chatgpt_subscription_active_until"))
            .and_then(Value::as_str)
            .map(|value| value.chars().take(10).collect()),
        access_expires_at,
        id_expires_at: jwt_exp(value.pointer("/tokens/id_token").and_then(Value::as_str)),
        access_seconds_left: access_expires_at.saturating_sub(Utc::now().timestamp()),
        has_refresh: value
            .pointer("/tokens/refresh_token")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty()),
        last_refresh: value
            .get("last_refresh")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    }
}

fn account_id(value: &Value) -> Option<String> {
    value
        .pointer("/tokens/account_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            token_claims(value)
                .pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
        })
}

fn identity(value: &Value) -> Option<String> {
    account_id(value)
        .map(|value| format!("account:{value}"))
        .or_else(|| {
            value
                .pointer("/_meta/email")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(|value| format!("email:{}", value.to_lowercase()))
        })
        .or_else(|| {
            token_claims(value)
                .get("email")
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
    if let Some(name) = value
        .pointer("/_meta/email")
        .and_then(Value::as_str)
        .or_else(|| value.pointer("/tokens/account_id").and_then(Value::as_str))
        .filter(|value| !value.is_empty())
    {
        return name.to_owned();
    }
    token_claims(value)
        .get("email")
        .and_then(Value::as_str)
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
async fn add_profile_json(name: String, json: String) -> AppResult<()> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let mut value = validate_auth_json(&json)?;
    value = tauri::async_runtime::spawn_blocking(move || {
        refresh_auth_value_locked(&mut value).map(|_| value)
    })
    .await
    .map_err(error_string)??;
    let mut profiles = read_profiles(&paths)?;
    if !add_value(&paths, &mut profiles, &name, &value)? {
        return Err("Этот Codex-аккаунт уже добавлен".into());
    }
    Ok(())
}

fn oauth_error_code(value: &Value, status: reqwest::StatusCode) -> String {
    value
        .get("error")
        .and_then(|error| {
            error.as_str().map(ToOwned::to_owned).or_else(|| {
                error
                    .get("code")
                    .or_else(|| error.get("type"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
            })
        })
        .unwrap_or_else(|| status.as_u16().to_string())
}

fn invalid_refresh_error(code: &str) -> bool {
    let lowered = code.to_lowercase();
    lowered.contains("invalid") || lowered.contains("reuse")
}

fn remove_refresh_token(value: &mut Value) -> bool {
    value
        .pointer_mut("/tokens")
        .and_then(Value::as_object_mut)
        .and_then(|tokens| tokens.remove("refresh_token"))
        .is_some()
}

fn refresh_auth_value_sync(value: &mut Value) -> AppResult<TokenStatus> {
    let refresh_token = value
        .pointer("/tokens/refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("В профиле нет refresh_token")?
        .to_owned();
    let original_identity = identity(value);
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(error_string)?;
    let response = client
        .post(OAUTH_TOKEN_URL)
        .header(USER_AGENT, "codex_cli_rs/0.150.1")
        .json(&serde_json::json!({
            "client_id": OAUTH_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "scope": "openid profile email"
        }))
        .send()
        .map_err(|error| format!("Сеть не отвечает: {error}"))?;
    let status = response.status();
    let fresh: Value = response
        .json()
        .map_err(|_| format!("OAuth вернул некорректный ответ: HTTP {status}"))?;
    if !status.is_success() {
        let code = oauth_error_code(&fresh, status);
        if invalid_refresh_error(&code) {
            return Err(format!("Refresh token недействителен: {code}"));
        }
        return Err(format!("OAuth отказал: {code}"));
    }
    apply_fresh_tokens(value, &fresh)?;
    if original_identity.is_some() && identity(value) != original_identity {
        return Err("OAuth вернул токены другого аккаунта".into());
    }
    Ok(token_status(value))
}

fn refresh_auth_value_locked(value: &mut Value) -> AppResult<TokenStatus> {
    let _guard = REFRESH_LOCK
        .lock()
        .map_err(|_| "Блокировка обновления токена повреждена")?;
    refresh_auth_value_sync(value)
}

fn profile_and_value(paths: &Paths, profile_id: Uuid) -> AppResult<(Profile, Value)> {
    let profile = read_profiles(paths)?
        .into_iter()
        .find(|profile| profile.id == profile_id)
        .ok_or("Профиль не найден")?;
    let value = profile_value(paths, &profile)?;
    Ok((profile, value))
}

#[tauri::command]
fn profile_token_status(profile_id: Uuid) -> AppResult<TokenStatus> {
    let paths = Paths::discover()?;
    let (_, value) = profile_and_value(&paths, profile_id)?;
    Ok(token_status(&value))
}

fn refresh_profile_sync(profile_id: Uuid) -> AppResult<TokenStatus> {
    let paths = Paths::discover()?;
    paths.ensure()?;
    let profiles = read_profiles(&paths)?;
    let profile = profiles
        .iter()
        .find(|profile| profile.id == profile_id)
        .ok_or("Профиль не найден")?;
    let was_active = detect_active(&paths, &profiles) == Some(profile_id);
    let profile_path = paths.profiles.join(&profile.file_name);
    let original = fs::read(&profile_path).map_err(error_string)?;
    let mut value: Value = serde_json::from_slice(&original).map_err(error_string)?;
    let status = match refresh_auth_value_locked(&mut value) {
        Ok(status) => status,
        Err(error) if error.starts_with("Refresh token недействителен:") => {
            // Access token остаётся рабочим до своего exp. Убираем только уже
            // подтверждённо мёртвый refresh, чтобы не повторять обмен.
            remove_refresh_token(&mut value);
            let preserved = serde_json::to_vec_pretty(&value).map_err(error_string)?;
            secure_write(
                &paths.backups.join(format!(
                    "invalid_refresh_{}_{}.json",
                    profile.id,
                    Utc::now().format("%Y-%m-%d_%H-%M-%S-%3f")
                )),
                &original,
            )?;
            secure_write(&profile_path, &preserved)?;
            if was_active {
                secure_write(&paths.auth, &preserved)?;
            }
            return Err(error);
        }
        Err(error) => return Err(error),
    };
    let refreshed = serde_json::to_vec_pretty(&value).map_err(error_string)?;
    secure_write(
        &paths.backups.join(format!(
            "refresh_{}_{}.json",
            profile.id,
            Utc::now().format("%Y-%m-%d_%H-%M-%S-%3f")
        )),
        &original,
    )?;
    secure_write(&profile_path, &refreshed)?;
    if was_active {
        secure_write(&paths.auth, &refreshed)?;
    }
    Ok(status)
}

#[tauri::command]
async fn refresh_profile(profile_id: Uuid) -> AppResult<TokenStatus> {
    tauri::async_runtime::spawn_blocking(move || refresh_profile_sync(profile_id))
        .await
        .map_err(error_string)?
}

fn currency_country(currency: &str) -> Option<String> {
    let country = match currency.to_lowercase().as_str() {
        "pln" => "PL",
        "vnd" => "VN",
        "inr" => "IN",
        "usd" => "US",
        "gbp" => "GB",
        "uah" => "UA",
        "try" => "TR",
        "brl" => "BR",
        "idr" => "ID",
        "thb" => "TH",
        "sgd" => "SG",
        "jpy" => "JP",
        "krw" => "KR",
        "cad" => "CA",
        "aud" => "AU",
        "mxn" => "MX",
        "php" => "PH",
        "ngn" => "NG",
        "egp" => "EG",
        "zar" => "ZA",
        "mad" => "MA",
        "sek" => "SE",
        "nok" => "NO",
        "dkk" => "DK",
        "czk" => "CZ",
        "huf" => "HU",
        "ron" => "RO",
        "chf" => "CH",
        "ils" => "IL",
        "sar" => "SA",
        "aed" => "AE",
        "myr" => "MY",
        "twd" => "TW",
        "hkd" => "HK",
        "nzd" => "NZ",
        "clp" => "CL",
        "cop" => "CO",
        "ars" => "AR",
        "pen" => "PE",
        "kzt" => "KZ",
        _ => return None,
    };
    Some(country.to_owned())
}

fn masked_handle(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        return None;
    }
    if let Some((head, tail)) = value.split_once('@') {
        return Some(format!(
            "{}···@{tail}",
            head.chars().take(2).collect::<String>()
        ));
    }
    Some(format!("{}···", value.chars().take(2).collect::<String>()))
}

fn payment_label(kind: &str, card: &Value) -> String {
    if kind == "card" {
        if let Some(brand) = card.get("brand").and_then(Value::as_str) {
            let mut chars = brand.chars();
            return chars
                .next()
                .map(|first| first.to_uppercase().collect::<String>() + chars.as_str())
                .unwrap_or_else(|| "Карта".into());
        }
    }
    match kind {
        "card" => "Карта",
        "link" => "Link",
        "paypal" => "PayPal",
        "cashapp" => "Cash App",
        "klarna" => "Klarna",
        "pix" => "PIX",
        "upi" => "UPI",
        "sepa_debit" => "SEPA",
        "go_pay" => "GoPay",
        "blik" => "BLIK",
        "bizum" => "Bizum",
        "kakao_pay" => "KakaoPay",
        "naver_pay" => "NaverPay",
        "ideal" => "iDEAL",
        "twint" => "TWINT",
        value if !value.is_empty() => value,
        _ => "Способ оплаты",
    }
    .to_owned()
}

fn payment_methods(value: &Value) -> Vec<PaymentMethodInfo> {
    let mut seen = HashSet::new();
    value
        .get("payment_methods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|row| {
            let kind = row.get("type").and_then(Value::as_str).unwrap_or_default();
            let card = row.get("card").unwrap_or(&Value::Null);
            let detail = row.get(kind).unwrap_or(&Value::Null);
            let expires = match (
                card.get("exp_month").and_then(Value::as_u64),
                card.get("exp_year").and_then(Value::as_u64),
            ) {
                (Some(month), Some(year)) => Some(format!("{month:02}/{year}")),
                _ => None,
            };
            PaymentMethodInfo {
                label: payment_label(kind, card),
                last4: card
                    .get("last4")
                    .or_else(|| detail.get("last4"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                handle: masked_handle(
                    detail
                        .get("email")
                        .or_else(|| detail.get("vpa"))
                        .or_else(|| detail.get("payer_email"))
                        .and_then(Value::as_str),
                ),
                expires,
            }
        })
        .filter(|method| {
            seen.insert(format!(
                "{}|{}|{}",
                method.label,
                method.last4.as_deref().unwrap_or_default(),
                method.handle.as_deref().unwrap_or_default()
            ))
        })
        .collect()
}

fn payment_country(value: &Value) -> Option<String> {
    value
        .get("payment_methods")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find_map(|row| {
            row.pointer("/billing_details/address/country")
                .or_else(|| row.pointer("/billing_address/country"))
                .or_else(|| row.pointer("/card/country"))
                .and_then(Value::as_str)
                .filter(|value| value.len() == 2)
                .map(|value| value.to_uppercase())
        })
}

async fn backend_get(client: &reqwest::Client, path: &str) -> AppResult<Value> {
    let response = client
        .get(format!("{CHATGPT_BACKEND}{path}"))
        .send()
        .await
        .map_err(error_string)?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    response.json().await.map_err(error_string)
}

#[tauri::command]
async fn account_live_info(profile_id: Uuid) -> AppResult<AccountLiveInfo> {
    let paths = Paths::discover()?;
    let (_, value) = profile_and_value(&paths, profile_id)?;
    let token = token_status(&value);
    let access = value
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("В профиле нет access_token")?;
    let account_id = account_id(&value).ok_or("В профиле нет account_id")?;
    let mut headers = HeaderMap::new();
    let mut authorization =
        HeaderValue::from_str(&format!("Bearer {access}")).map_err(error_string)?;
    authorization.set_sensitive(true);
    headers.insert(AUTHORIZATION, authorization);
    headers.insert(
        "chatgpt-account-id",
        HeaderValue::from_str(&account_id).map_err(error_string)?,
    );
    headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
    headers.insert(ORIGIN, HeaderValue::from_static("https://chatgpt.com"));
    headers.insert(REFERER, HeaderValue::from_static("https://chatgpt.com/"));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static("codex-limit-panel/1.0"),
    );
    let client = reqwest::Client::builder()
        .default_headers(headers)
        .http1_only()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(error_string)?;
    let mut live_plan = None;
    let mut country = None;
    let mut currency = None;
    let mut methods = Vec::new();
    let mut warnings = Vec::new();
    let mut live_failures = 0_u8;

    match backend_get(&client, "/accounts/check/v4-2023-04-27").await {
        Ok(check) => {
            let row = check
                .pointer(&format!("/accounts/{account_id}"))
                .or_else(|| {
                    check
                        .get("accounts")
                        .and_then(Value::as_object)
                        .and_then(|accounts| accounts.values().find(|value| value.is_object()))
                });
            if let Some(row) = row {
                live_plan = row
                    .pointer("/account/plan_type")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned);
                currency = row
                    .pointer("/entitlement/billing_currency")
                    .and_then(Value::as_str)
                    .map(|value| value.to_uppercase());
            }
        }
        Err(_) => live_failures += 1,
    }

    let mut subscription_url =
        reqwest::Url::parse(&format!("{CHATGPT_BACKEND}/subscriptions")).map_err(error_string)?;
    subscription_url
        .query_pairs_mut()
        .append_pair("account_id", &account_id);
    match client.get(subscription_url).send().await {
        Ok(response) if response.status().is_success() => {
            if let Ok(subscription) = response.json::<Value>().await {
                country = subscription
                    .get("price_country")
                    .and_then(Value::as_str)
                    .filter(|value| value.len() == 2)
                    .map(|value| value.to_uppercase());
                if currency.is_none() {
                    currency = subscription
                        .get("billing_currency")
                        .and_then(Value::as_str)
                        .map(|value| value.to_uppercase());
                }
            }
        }
        _ => live_failures += 1,
    }
    if country.is_none() {
        country = currency.as_deref().and_then(currency_country);
    }

    let mut payments_url =
        reqwest::Url::parse(&format!("{CHATGPT_BACKEND}/payments/payment_methods"))
            .map_err(error_string)?;
    payments_url
        .query_pairs_mut()
        .append_pair("account_id", &account_id);
    match client.get(payments_url).send().await {
        Ok(response) if response.status().is_success() => {
            if let Ok(wallet) = response.json::<Value>().await {
                if country.is_none() {
                    country = payment_country(&wallet);
                }
                methods = payment_methods(&wallet);
            }
        }
        _ => live_failures += 1,
    }
    if live_failures == 3 {
        warnings.push("Расширенные данные OpenAI временно недоступны".into());
    }

    Ok(AccountLiveInfo {
        token,
        live_plan,
        country,
        currency,
        payment_methods: methods,
        warnings,
    })
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
    let mut duplicates = 0;
    let mut captured = 0;
    let mut rejected = 0;
    for (index, candidate) in candidates.into_iter().enumerate() {
        if is_duplicate(&paths, &profiles, &candidate)? {
            duplicates += 1;
            continue;
        }
        let rotated = tauri::async_runtime::spawn_blocking(move || {
            let mut candidate = candidate;
            refresh_auth_value_locked(&mut candidate).map(|_| candidate)
        })
        .await
        .map_err(error_string)?;
        let Ok(rotated) = rotated else {
            rejected += 1;
            continue;
        };
        let name = suggested_name(&rotated, &format!("KeyCrop #{}", index + 1));
        if add_value(&paths, &mut profiles, &name, &rotated)? {
            imported += 1;
            captured += 1;
        } else {
            duplicates += 1;
        }
    }
    Ok(KeyCropSyncResult {
        purchases_seen,
        accounts_found,
        imported,
        duplicates,
        captured,
        rejected,
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

fn broker_json(status: u16, value: &Value) -> Response<std::io::Cursor<Vec<u8>>> {
    let bytes = serde_json::to_vec(value).unwrap_or_else(|_| b"{\"error\":\"internal\"}".to_vec());
    let mut response = Response::from_data(bytes).with_status_code(StatusCode(status));
    if let Ok(header) = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]) {
        response.add_header(header);
    }
    response
}

fn apply_fresh_tokens(value: &mut Value, fresh: &Value) -> AppResult<()> {
    let access = fresh
        .get("access_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("OAuth не вернул access_token")?
        .to_owned();
    let refresh = fresh
        .get("refresh_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or("OAuth не вернул refresh_token")?
        .to_owned();
    let tokens = value
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or("В auth.json нет объекта tokens")?;
    tokens.insert("access_token".into(), Value::String(access));
    tokens.insert("refresh_token".into(), Value::String(refresh));
    if let Some(id_token) = fresh
        .get("id_token")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        tokens.insert("id_token".into(), Value::String(id_token.to_owned()));
    }
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "last_refresh".into(),
            Value::String(Utc::now().to_rfc3339()),
        );
    }
    Ok(())
}

fn handle_broker_request(mut request: tiny_http::Request) {
    if request.method() == &Method::Get && request.url() == "/health" {
        let _ = request.respond(broker_json(200, &serde_json::json!({"ok": true})));
        return;
    }
    if request.method() != &Method::Post || request.url().trim_end_matches('/') != "/oauth/token" {
        let _ = request.respond(broker_json(404, &serde_json::json!({"error": "not_found"})));
        return;
    }
    let mut body = String::new();
    if request.as_reader().read_to_string(&mut body).is_err() {
        let _ = request.respond(broker_json(
            400,
            &serde_json::json!({"error": "invalid_request"}),
        ));
        return;
    }
    let Ok(input) = serde_json::from_str::<Value>(&body) else {
        let _ = request.respond(broker_json(
            400,
            &serde_json::json!({"error": "invalid_json"}),
        ));
        return;
    };
    let Ok(_refresh_guard) = REFRESH_LOCK.lock() else {
        let _ = request.respond(broker_json(
            503,
            &serde_json::json!({"error": "refresh_busy"}),
        ));
        return;
    };
    let Ok(paths) = Paths::discover() else {
        let _ = request.respond(broker_json(
            503,
            &serde_json::json!({"error": "auth_unavailable"}),
        ));
        return;
    };
    let Ok(bytes) = fs::read(&paths.auth) else {
        let _ = request.respond(broker_json(
            503,
            &serde_json::json!({"error": "auth_unavailable"}),
        ));
        return;
    };
    let Ok(mut auth) = serde_json::from_slice::<Value>(&bytes) else {
        let _ = request.respond(broker_json(
            503,
            &serde_json::json!({"error": "auth_unavailable"}),
        ));
        return;
    };
    let stored_refresh = auth
        .pointer("/tokens/refresh_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let requested_refresh = input
        .get("refresh_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if stored_refresh.is_empty() || requested_refresh != stored_refresh {
        let _ = request.respond(broker_json(
            401,
            &serde_json::json!({"error": "stale_refresh_token"}),
        ));
        return;
    }
    let access = auth
        .pointer("/tokens/access_token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if jwt_exp(Some(access)) > Utc::now().timestamp() {
        let mut output = serde_json::json!({
            "access_token": access,
            "refresh_token": stored_refresh
        });
        if let Some(id_token) = auth.pointer("/tokens/id_token").and_then(Value::as_str) {
            output["id_token"] = Value::String(id_token.to_owned());
        }
        let _ = request.respond(broker_json(200, &output));
        return;
    }

    let client = match reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            let _ = request.respond(broker_json(
                502,
                &serde_json::json!({"error": "upstream_unavailable"}),
            ));
            return;
        }
    };
    let payload = serde_json::json!({
        "client_id": input.get("client_id").and_then(Value::as_str).unwrap_or(OAUTH_CLIENT_ID),
        "grant_type": "refresh_token",
        "refresh_token": requested_refresh,
        "scope": input.get("scope").and_then(Value::as_str).unwrap_or("openid profile email")
    });
    let upstream = match client
        .post(OAUTH_TOKEN_URL)
        .header(CONTENT_TYPE, "application/json")
        .header(USER_AGENT, "codex_cli_rs/0.150.1")
        .json(&payload)
        .send()
    {
        Ok(response) => response,
        Err(_) => {
            let _ = request.respond(broker_json(
                502,
                &serde_json::json!({"error": "upstream_unavailable"}),
            ));
            return;
        }
    };
    let status = upstream.status().as_u16();
    let output = upstream
        .json::<Value>()
        .unwrap_or_else(|_| serde_json::json!({"error": "invalid_upstream_response"}));
    if (200..300).contains(&status) {
        let original_identity = identity(&auth);
        if apply_fresh_tokens(&mut auth, &output).is_ok()
            && (original_identity.is_none() || identity(&auth) == original_identity)
        {
            if let Ok(refreshed) = serde_json::to_vec_pretty(&auth) {
                let _ = secure_write(&paths.auth, &refreshed);
            }
        }
    }
    let _ = request.respond(broker_json(status, &output));
}

fn start_refresh_broker() {
    let state = BROKER_STATE
        .get_or_init(|| Arc::new(BrokerRuntime::default()))
        .clone();
    if state
        .started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    thread::spawn(move || {
        let server = match Server::http(("127.0.0.1", BROKER_PORT)) {
            Ok(server) => server,
            Err(error) => {
                state.running.store(false, Ordering::SeqCst);
                state.started.store(false, Ordering::SeqCst);
                if let Ok(mut message) = state.error.lock() {
                    *message = Some(format!("Порт {BROKER_PORT} занят: {error}"));
                }
                return;
            }
        };
        state.running.store(true, Ordering::SeqCst);
        if let Ok(mut message) = state.error.lock() {
            *message = None;
        }
        for request in server.incoming_requests() {
            thread::spawn(move || handle_broker_request(request));
        }
        state.running.store(false, Ordering::SeqCst);
    });
}

fn configured_broker_override() -> String {
    if let Ok(value) = std::env::var("CODEX_REFRESH_TOKEN_URL_OVERRIDE") {
        if !value.is_empty() {
            return value;
        }
    }
    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = Command::new("launchctl")
            .args(["getenv", "CODEX_REFRESH_TOKEN_URL_OVERRIDE"])
            .output()
        {
            return String::from_utf8_lossy(&output.stdout).trim().to_owned();
        }
    }
    String::new()
}

#[tauri::command]
fn broker_status() -> BrokerStatus {
    let state = BROKER_STATE
        .get_or_init(|| Arc::new(BrokerRuntime::default()))
        .clone();
    let marker = Paths::discover().is_ok_and(|paths| paths.broker_enabled.exists());
    let configured =
        configured_broker_override() == BROKER_URL || (cfg!(target_os = "windows") && marker);
    BrokerStatus {
        running: state.running.load(Ordering::SeqCst),
        configured,
        url: BROKER_URL.into(),
        error: state.error.lock().ok().and_then(|error| error.clone()),
    }
}

fn apply_broker_override() -> AppResult<()> {
    #[cfg(target_os = "macos")]
    let success = Command::new("launchctl")
        .args(["setenv", "CODEX_REFRESH_TOKEN_URL_OVERRIDE", BROKER_URL])
        .status()
        .map_err(error_string)?
        .success();
    #[cfg(target_os = "windows")]
    let success = Command::new("cmd")
        .args(["/C", "setx", "CODEX_REFRESH_TOKEN_URL_OVERRIDE", BROKER_URL])
        .status()
        .map_err(error_string)?
        .success();
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let success = true;
    if !success {
        return Err("Не удалось настроить переменную окружения broker".into());
    }
    Ok(())
}

#[tauri::command]
fn configure_broker() -> AppResult<BrokerStatus> {
    start_refresh_broker();
    apply_broker_override()?;
    let paths = Paths::discover()?;
    paths.ensure()?;
    secure_write(&paths.broker_enabled, b"enabled")?;
    Ok(broker_status())
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
    if let Ok(paths) = Paths::discover() {
        if paths.broker_enabled.exists() {
            let _ = apply_broker_override();
        }
    }
    start_auth_refresh_interceptor();
    start_refresh_broker();
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
            profile_token_status,
            refresh_profile,
            account_live_info,
            broker_status,
            configure_broker,
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
            broker_enabled: root.join(".codex/account-switcher/broker-enabled"),
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

        assert_eq!(
            detect_active(&paths, std::slice::from_ref(&profile)),
            Some(profile.id)
        );
        assert_eq!(
            fs::read(paths.profiles.join(profile.file_name)).unwrap(),
            refreshed
        );
        fs::remove_dir_all(root).unwrap();
    }

    fn fake_jwt(payload: Value) -> String {
        format!(
            "e30.{}.signature",
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&payload).unwrap())
        )
    }

    #[test]
    fn jwt_status_is_read_without_exposing_tokens() {
        let expires = Utc::now().timestamp() + 7_200;
        let id_token = fake_jwt(json!({
            "exp": expires + 60,
            "email": "test@example.com",
            "https://api.openai.com/auth": {
                "chatgpt_plan_type": "plus",
                "chatgpt_subscription_active_until": "2030-01-02T00:00:00Z",
                "chatgpt_account_id": "acc_test"
            }
        }));
        let access_token = fake_jwt(json!({"exp": expires}));
        let auth = json!({"tokens": {
            "access_token": access_token,
            "id_token": id_token,
            "refresh_token": "refresh-secret"
        }});
        let status = token_status(&auth);
        assert_eq!(status.email.as_deref(), Some("test@example.com"));
        assert_eq!(status.plan_type.as_deref(), Some("plus"));
        assert_eq!(status.subscription_until.as_deref(), Some("2030-01-02"));
        assert!(status.has_refresh);
        assert!(status.access_seconds_left > 7_000);
        assert_eq!(identity(&auth).as_deref(), Some("account:acc_test"));
    }

    #[test]
    fn invalid_refresh_is_removed_without_touching_access() {
        let mut auth = json!({
            "tokens": {
                "access_token": "still-working",
                "refresh_token": "already-dead"
            }
        });

        assert!(remove_refresh_token(&mut auth));
        assert_eq!(
            auth.pointer("/tokens/access_token").and_then(Value::as_str),
            Some("still-working")
        );
        assert!(auth.pointer("/tokens/refresh_token").is_none());
    }

    #[test]
    fn fresh_token_pair_is_applied_atomically_to_value() {
        let mut auth = json!({"tokens": {
            "account_id": "acc_1",
            "access_token": "old-access",
            "id_token": "old-id",
            "refresh_token": "old-refresh"
        }});
        apply_fresh_tokens(
            &mut auth,
            &json!({
                "access_token": "new-access",
                "id_token": "new-id",
                "refresh_token": "new-refresh"
            }),
        )
        .unwrap();
        assert_eq!(
            auth.pointer("/tokens/access_token").and_then(Value::as_str),
            Some("new-access")
        );
        assert_eq!(
            auth.pointer("/tokens/refresh_token")
                .and_then(Value::as_str),
            Some("new-refresh")
        );
        assert!(auth.get("last_refresh").and_then(Value::as_str).is_some());
    }
}
