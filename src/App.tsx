import { invoke } from "@tauri-apps/api/core";
import { readText } from "@tauri-apps/plugin-clipboard-manager";
import {
  CalendarClock,
  Check,
  ChevronRight,
  Cookie,
  CreditCard,
  Globe2,
  Info,
  KeyRound,
  LoaderCircle,
  Plus,
  RefreshCw,
  Settings,
  ShieldCheck,
  Trash2,
  X,
} from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import keycropMark from "./assets/keycrop-mark.webp";
import type {
  AccountLiveInfo,
  AppSnapshot,
  BrokerStatus,
  KeyCropStatus,
  Profile,
  RateLimit,
  TokenStatus,
} from "./types";

type Notice = { kind: "ok" | "error"; text: string } | null;
type SyncResult = {
  purchasesSeen: number;
  accountsFound: number;
  imported: number;
  duplicates: number;
  captured: number;
  rejected: number;
};
type SyncPhase = "idle" | "keycrop" | "limits";
type ProbeResult = "ok" | "expired" | "error";

function isExpiredAuth(error: unknown) {
  const message = String(error).toLowerCase();
  return [
    "401",
    "unauthorized",
    "authentication expired",
    "authorization expired",
    "invalid_grant",
    "invalid token",
    "token expired",
    "access token could not be refreshed",
    "refresh token was already used",
    "refresh_token_reused",
    "login required",
    "not logged in",
  ].some((marker) => message.includes(marker));
}

function isInvalidRefresh(error: unknown) {
  const message = String(error).toLowerCase();
  return (
    message.includes("refresh token недействителен") ||
    message.includes("invalid_refresh_token") ||
    message.includes("refresh_token_reused") ||
    message.includes("refresh token was already used")
  );
}

export default function App() {
  const [snapshot, setSnapshot] = useState<AppSnapshot | null>(null);
  const [keycrop, setKeycrop] = useState<KeyCropStatus | null>(null);
  const [rates, setRates] = useState<Record<string, RateLimit>>({});
  const [rateErrors, setRateErrors] = useState<Record<string, string>>({});
  const [checkingId, setCheckingId] = useState<string | null>(null);
  const [activatingId, setActivatingId] = useState<string | null>(null);
  const [syncPhase, setSyncPhase] = useState<SyncPhase>("idle");
  const [notice, setNotice] = useState<Notice>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);
  const [selectedProfile, setSelectedProfile] = useState<Profile | null>(null);
  const [tokenStatuses, setTokenStatuses] = useState<
    Record<string, TokenStatus>
  >({});
  const [liveInfo, setLiveInfo] = useState<Record<string, AccountLiveInfo>>({});
  const [detailsLoading, setDetailsLoading] = useState(false);
  const [refreshingTokenId, setRefreshingTokenId] = useState<string | null>(
    null,
  );
  const [broker, setBroker] = useState<BrokerStatus | null>(null);
  const [configuringBroker, setConfiguringBroker] = useState(false);
  const [cookie, setCookie] = useState("");
  const [profileName, setProfileName] = useState("");
  const [profileJson, setProfileJson] = useState("");
  const [saving, setSaving] = useState(false);

  const reload = useCallback(async () => {
    const next = await invoke<AppSnapshot>("load_snapshot");
    setSnapshot(next);
    return next;
  }, []);

  const probeProfile = useCallback(
    async (profile: Profile): Promise<ProbeResult> => {
      setCheckingId(profile.id);
      try {
        const value = await invoke<RateLimit>("probe_rate_limits", {
          profileId: profile.id,
        });
        setRates((current) => ({ ...current, [profile.id]: value }));
        setRateErrors((current) => ({ ...current, [profile.id]: "" }));
        return "ok";
      } catch (error) {
        const expired = isExpiredAuth(error);
        setRateErrors((current) => ({
          ...current,
          [profile.id]: expired ? "Авторизация истекла" : "Лимиты недоступны",
        }));
        return expired ? "expired" : "error";
      } finally {
        setCheckingId(null);
      }
    },
    [],
  );

  const refreshLimits = useCallback(
    async (profiles: Profile[]) => {
      setSyncPhase("limits");
      const expired: Profile[] = [];
      for (const profile of profiles) {
        if ((await probeProfile(profile)) === "expired") expired.push(profile);
      }
      if (expired.length) {
        for (const profile of expired) {
          await invoke("delete_profile", { profileId: profile.id });
        }
        const expiredIds = new Set(expired.map((profile) => profile.id));
        setRates((current) =>
          Object.fromEntries(
            Object.entries(current).filter(([id]) => !expiredIds.has(id)),
          ),
        );
        setRateErrors((current) =>
          Object.fromEntries(
            Object.entries(current).filter(([id]) => !expiredIds.has(id)),
          ),
        );
        setSelectedProfile((current) =>
          current && expiredIds.has(current.id) ? null : current,
        );
        await reload();
        setNotice({
          kind: "ok",
          text: `Удалено аккаунтов с истекшей авторизацией: ${expired.length}`,
        });
      }
      setSyncPhase("idle");
    },
    [probeProfile, reload],
  );

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        let next = await reload();
        const status = await invoke<KeyCropStatus>("keycrop_status");
        if (cancelled) return;
        setKeycrop(status);
        if (status.connected) {
          setSyncPhase("keycrop");
          await invoke<SyncResult>("keycrop_sync_accounts");
          next = await reload();
        }
        if (!cancelled) await refreshLimits(next.profiles);
      } catch (error) {
        if (!cancelled) {
          setSyncPhase("idle");
          setNotice({ kind: "error", text: String(error) });
        }
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [reload, refreshLimits]);

  useEffect(() => {
    if (!notice) return;
    const timeout = window.setTimeout(() => setNotice(null), 4500);
    return () => window.clearTimeout(timeout);
  }, [notice]);

  useEffect(() => {
    void invoke<BrokerStatus>("broker_status")
      .then(setBroker)
      .catch(() => {});
  }, []);

  useEffect(() => {
    if (!selectedProfile) return;
    let cancelled = false;
    setDetailsLoading(true);
    void Promise.allSettled([
      invoke<TokenStatus>("profile_token_status", {
        profileId: selectedProfile.id,
      }),
      invoke<AccountLiveInfo>("account_live_info", {
        profileId: selectedProfile.id,
      }),
    ]).then(([tokenResult, liveResult]) => {
      if (cancelled) return;
      if (tokenResult.status === "fulfilled") {
        setTokenStatuses((current) => ({
          ...current,
          [selectedProfile.id]: tokenResult.value,
        }));
      }
      if (liveResult.status === "fulfilled") {
        setLiveInfo((current) => ({
          ...current,
          [selectedProfile.id]: liveResult.value,
        }));
        setTokenStatuses((current) => ({
          ...current,
          [selectedProfile.id]: liveResult.value.token,
        }));
      }
      setDetailsLoading(false);
    });
    return () => {
      cancelled = true;
    };
  }, [selectedProfile]);

  async function syncAll() {
    if (syncPhase !== "idle") return;
    try {
      let next = snapshot;
      if (keycrop?.connected) {
        setSyncPhase("keycrop");
        const result = await invoke<SyncResult>("keycrop_sync_accounts");
        next = await reload();
        setNotice({
          kind: "ok",
          text: result.imported
            ? `Добавлено и перехвачено: ${result.captured}`
            : result.rejected
              ? `Новых рабочих аккаунтов нет · отклонено: ${result.rejected}`
              : "Аккаунты синхронизированы",
        });
      }
      if (next) await refreshLimits(next.profiles);
    } catch (error) {
      setSyncPhase("idle");
      setNotice({ kind: "error", text: String(error) });
    }
  }

  async function activate(profile: Profile) {
    if (profile.id === snapshot?.activeId || activatingId) return;
    setActivatingId(profile.id);
    try {
      await invoke("activate_profile", { profileId: profile.id });
      await reload();
      setNotice({ kind: "ok", text: `Активирован «${profile.name}»` });
    } catch (error) {
      setNotice({ kind: "error", text: String(error) });
    } finally {
      setActivatingId(null);
    }
  }

  async function refreshToken(profile: Profile) {
    if (refreshingTokenId) return;
    setRefreshingTokenId(profile.id);
    try {
      const status = await invoke<TokenStatus>("refresh_profile", {
        profileId: profile.id,
      });
      setTokenStatuses((current) => ({ ...current, [profile.id]: status }));
      const info = await invoke<AccountLiveInfo>("account_live_info", {
        profileId: profile.id,
      }).catch(() => null);
      if (info) setLiveInfo((current) => ({ ...current, [profile.id]: info }));
      await probeProfile(profile);
      setNotice({ kind: "ok", text: "Токены обновлены и сохранены" });
    } catch (error) {
      if (isInvalidRefresh(error)) {
        const status = await invoke<TokenStatus>("profile_token_status", {
          profileId: profile.id,
        });
        setTokenStatuses((current) => ({ ...current, [profile.id]: status }));
        if (status.accessSecondsLeft <= 0) {
          await invoke("delete_profile", { profileId: profile.id });
          await reload();
          setSelectedProfile(null);
          setNotice({
            kind: "error",
            text: "Access и refresh истекли · профиль удалён",
          });
        } else {
          setNotice({
            kind: "error",
            text: `Refresh недействителен · access продолжит работать ещё ${formatLifetime(status.accessSecondsLeft)}`,
          });
        }
      } else if (isExpiredAuth(error)) {
        await invoke("delete_profile", { profileId: profile.id });
        await reload();
        setSelectedProfile(null);
        setNotice({
          kind: "error",
          text: "Авторизация истекла · профиль удалён",
        });
      } else {
        setNotice({ kind: "error", text: String(error) });
      }
    } finally {
      setRefreshingTokenId(null);
    }
  }

  async function enableBroker() {
    setConfiguringBroker(true);
    try {
      const status = await invoke<BrokerStatus>("configure_broker");
      setBroker(status);
      setNotice({
        kind: "ok",
        text: "Broker подключён · перезапусти Codex",
      });
    } catch (error) {
      setNotice({ kind: "error", text: String(error) });
    } finally {
      setConfiguringBroker(false);
    }
  }

  async function connectKeyCrop() {
    if (!cookie.trim()) return;
    setSaving(true);
    try {
      const status = await invoke<KeyCropStatus>("keycrop_connect", { cookie });
      setKeycrop(status);
      setCookie("");
      setSettingsOpen(false);
      setSyncPhase("keycrop");
      const result = await invoke<SyncResult>("keycrop_sync_accounts");
      const next = await reload();
      setNotice({
        kind: "ok",
        text: result.imported
          ? `KeyCrop подключён · перехвачено ${result.captured}`
          : result.rejected
            ? `KeyCrop подключён · отклонено ${result.rejected}`
            : "KeyCrop подключён",
      });
      await refreshLimits(next.profiles);
    } catch (error) {
      setNotice({ kind: "error", text: String(error) });
    } finally {
      setSaving(false);
    }
  }

  async function addProfile() {
    if (!profileJson.trim()) return;
    setSaving(true);
    try {
      await invoke("add_profile_json", {
        name: profileName,
        json: profileJson,
      });
      setProfileName("");
      setProfileJson("");
      const next = await reload();
      setNotice({ kind: "ok", text: "Профиль добавлен · refresh перехвачен" });
      const added = next.profiles.at(-1);
      if (added) await refreshLimits([added]);
    } catch (error) {
      setNotice({ kind: "error", text: String(error) });
    } finally {
      setSaving(false);
    }
  }

  async function removeProfile(profile: Profile) {
    if (
      !window.confirm(`Удалить «${profile.name}»? Копия останется в backups.`)
    )
      return;
    try {
      await invoke("delete_profile", { profileId: profile.id });
      await reload();
      setSelectedProfile(null);
      setNotice({ kind: "ok", text: "Профиль удалён" });
    } catch (error) {
      setNotice({ kind: "error", text: String(error) });
    }
  }

  const profiles = snapshot?.profiles ?? [];
  const active = profiles.find((profile) => profile.id === snapshot?.activeId);
  const syncText =
    syncPhase === "keycrop"
      ? "Получаю аккаунты…"
      : syncPhase === "limits"
        ? "Проверяю лимиты…"
        : "Обновить";

  return (
    <div className="app-shell">
      <header className="topbar">
        <div className="brand">
          <img src={keycropMark} alt="" />
          <strong>
            Codex<span>Crop</span>
          </strong>
        </div>
        <div className="topbar-actions">
          <span className={`connection ${keycrop?.connected ? "online" : ""}`}>
            <i /> {keycrop?.connected ? "KeyCrop" : "Без KeyCrop"}
          </span>
          <button
            className="toolbar-button"
            disabled={syncPhase !== "idle"}
            onClick={syncAll}
          >
            {syncPhase === "idle" ? (
              <RefreshCw size={15} />
            ) : (
              <LoaderCircle className="spin" size={15} />
            )}
            {syncText}
          </button>
          <button
            className="icon-button"
            aria-label="Настройки"
            onClick={() => setSettingsOpen(true)}
          >
            <Settings size={17} />
          </button>
        </div>
      </header>

      <main>
        <section className="page-heading">
          <div>
            <p className="eyebrow">CODEX ACCOUNTS</p>
            <h1>
              Аккаунты <span>{profiles.length}</span>
            </h1>
            <p>
              {active
                ? `Сейчас активен: ${active.name}`
                : "Активный аккаунт не определён"}
            </p>
          </div>
        </section>

        {notice && <div className={`notice ${notice.kind}`}>{notice.text}</div>}

        <section className="accounts-panel">
          <div className="table-head">
            <span>Аккаунт</span>
            <span>Тариф</span>
            <span>5 часов</span>
            <span>Неделя</span>
            <span />
          </div>
          <div className="account-list">
            {profiles.map((profile) => {
              const isActive = profile.id === snapshot?.activeId;
              const isActivating = profile.id === activatingId;
              return (
                <div
                  key={profile.id}
                  className={`account-row ${isActive ? "active" : ""}`}
                  role="button"
                  tabIndex={0}
                  onClick={() => setSelectedProfile(profile)}
                  onKeyDown={(event) => {
                    if (event.key === "Enter" || event.key === " ")
                      setSelectedProfile(profile);
                  }}
                >
                  <div className="identity">
                    <span className="avatar">
                      {profile.name.slice(0, 1).toUpperCase()}
                    </span>
                    <span>
                      <b>{profile.name}</b>
                      <small>
                        {isActive
                          ? "Активный"
                          : rateErrors[profile.id] || "Нажми для подробностей"}
                      </small>
                    </span>
                  </div>
                  <PlanCell
                    rate={rates[profile.id]}
                    loading={checkingId === profile.id}
                  />
                  <UsageCell window={rates[profile.id]?.primary} />
                  <UsageCell window={rates[profile.id]?.secondary} />
                  <span className="row-action">
                    {isActivating ? (
                      <LoaderCircle className="spin" size={18} />
                    ) : isActive ? (
                      <Check size={18} />
                    ) : (
                      <ChevronRight size={18} />
                    )}
                  </span>
                </div>
              );
            })}
            {!profiles.length && (
              <div className="empty">
                Подключи KeyCrop или добавь auth.json в настройках.
              </div>
            )}
          </div>
        </section>
        <footer>
          Клик по аккаунту открывает информацию · переключение подтверждается
        </footer>
      </main>

      {selectedProfile && (
        <div
          className="modal-backdrop"
          onMouseDown={(event) => {
            if (event.target === event.currentTarget) setSelectedProfile(null);
          }}
        >
          <AccountDetails
            profile={selectedProfile}
            rate={rates[selectedProfile.id]}
            error={rateErrors[selectedProfile.id]}
            active={selectedProfile.id === snapshot?.activeId}
            loading={checkingId === selectedProfile.id}
            detailsLoading={detailsLoading}
            activating={activatingId === selectedProfile.id}
            refreshingToken={refreshingTokenId === selectedProfile.id}
            token={tokenStatuses[selectedProfile.id]}
            live={liveInfo[selectedProfile.id]}
            onClose={() => setSelectedProfile(null)}
            onRefresh={() => refreshLimits([selectedProfile])}
            onRefreshToken={() => refreshToken(selectedProfile)}
            onActivate={() => activate(selectedProfile)}
          />
        </div>
      )}

      {settingsOpen && (
        <div
          className="modal-backdrop"
          onMouseDown={(event) => {
            if (event.target === event.currentTarget) setSettingsOpen(false);
          }}
        >
          <section className="settings-modal">
            <div className="modal-head">
              <div>
                <p className="eyebrow">SETTINGS</p>
                <h2>Настройки</h2>
              </div>
              <button
                className="icon-button"
                onClick={() => setSettingsOpen(false)}
              >
                <X size={18} />
              </button>
            </div>
            <div className="settings-scroll">
              <div className="settings-section">
                <div className="section-copy broker-copy">
                  <ShieldCheck size={18} />
                  <span>
                    <b>Refresh Broker</b>
                    <small>
                      {broker?.running
                        ? broker.configured
                          ? "Работает и подключён к Codex"
                          : "Работает · нужно подключить к Codex"
                        : broker?.error || "Запускается локально на 127.0.0.1"}
                    </small>
                  </span>
                  <i className={broker?.running ? "online" : ""} />
                </div>
                <div className="broker-line">
                  <code>
                    {broker?.url || "http://127.0.0.1:1456/oauth/token"}
                  </code>
                  <button
                    className="primary"
                    disabled={configuringBroker || broker?.configured}
                    onClick={enableBroker}
                  >
                    {configuringBroker && (
                      <LoaderCircle className="spin" size={14} />
                    )}
                    {broker?.configured ? "Подключён" : "Подключить"}
                  </button>
                </div>
                <p className="broker-note">
                  После подключения перезапусти Codex и оставляй Switcher
                  запущенным: broker работает внутри приложения.
                </p>
              </div>

              <div className="settings-section">
                <div className="section-copy">
                  <Cookie size={18} />
                  <span>
                    <b>KeyCrop</b>
                    <small>
                      {keycrop?.connected
                        ? "Сессия подключена"
                        : "Вставь Cookie header или JSON EditThisCookie"}
                    </small>
                  </span>
                </div>
                <textarea
                  className="compact-textarea"
                  value={cookie}
                  onChange={(event) => setCookie(event.target.value)}
                  placeholder={
                    '[{"domain":"keycrop.net","name":"session","value":"…"}]'
                  }
                />
                <div className="form-actions">
                  <button
                    className="ghost"
                    onClick={async () => setCookie(await readText())}
                  >
                    Из буфера
                  </button>
                  <button
                    className="primary"
                    disabled={saving || !cookie.trim()}
                    onClick={connectKeyCrop}
                  >
                    Подключить
                  </button>
                </div>
              </div>

              <details className="settings-section">
                <summary>
                  <span>
                    <Plus size={18} /> Добавить auth.json вручную
                  </span>
                  <ChevronRight size={17} />
                </summary>
                <div className="details-body">
                  <input
                    value={profileName}
                    onChange={(event) => setProfileName(event.target.value)}
                    placeholder="Название (необязательно)"
                  />
                  <textarea
                    className="compact-textarea"
                    value={profileJson}
                    onChange={(event) => setProfileJson(event.target.value)}
                    placeholder={'{"tokens": {…}}'}
                  />
                  <div className="form-actions">
                    <button
                      className="primary"
                      disabled={saving || !profileJson.trim()}
                      onClick={addProfile}
                    >
                      Добавить
                    </button>
                  </div>
                </div>
              </details>

              <details className="settings-section danger-section">
                <summary>
                  <span>
                    <Trash2 size={18} /> Управление профилями
                  </span>
                  <ChevronRight size={17} />
                </summary>
                <div className="profile-management">
                  {profiles.map((profile) => (
                    <div key={profile.id}>
                      <span>{profile.name}</span>
                      <button
                        className="danger-icon"
                        aria-label={`Удалить ${profile.name}`}
                        onClick={() => removeProfile(profile)}
                      >
                        <Trash2 size={15} />
                      </button>
                    </div>
                  ))}
                </div>
              </details>
            </div>
          </section>
        </div>
      )}
    </div>
  );
}

function AccountDetails({
  profile,
  rate,
  error,
  active,
  loading,
  detailsLoading,
  activating,
  refreshingToken,
  token,
  live,
  onClose,
  onRefresh,
  onRefreshToken,
  onActivate,
}: {
  profile: Profile;
  rate?: RateLimit;
  error?: string;
  active: boolean;
  loading: boolean;
  detailsLoading: boolean;
  activating: boolean;
  refreshingToken: boolean;
  token?: TokenStatus;
  live?: AccountLiveInfo;
  onClose: () => void;
  onRefresh: () => void;
  onRefreshToken: () => void;
  onActivate: () => void;
}) {
  return (
    <section className="account-modal">
      <div className="modal-head account-modal-head">
        <div className="detail-identity">
          <span className="avatar">
            {profile.name.slice(0, 1).toUpperCase()}
          </span>
          <span>
            <p className="eyebrow">ACCOUNT DETAILS</p>
            <h2>{profile.name}</h2>
          </span>
        </div>
        <button className="icon-button" onClick={onClose} aria-label="Закрыть">
          <X size={18} />
        </button>
      </div>

      <div className="account-detail-body">
        <div className={`detail-status ${active ? "active" : ""}`}>
          <span>{active ? <Check size={16} /> : <Info size={16} />}</span>
          <div>
            <b>
              {active ? "Активен в Codex" : error || "Готов к переключению"}
            </b>
            <small>
              {(
                live?.livePlan ||
                token?.planType ||
                rate?.planType
              )?.toUpperCase() || "Тариф не определён"}
            </small>
          </div>
        </div>

        <div className="detail-limits">
          <DetailUsage title="Лимит на 5 часов" window={rate?.primary} />
          <DetailUsage title="Недельный лимит" window={rate?.secondary} />
        </div>

        <div className="token-health">
          <span className="token-icon">
            <KeyRound size={17} />
          </span>
          <div>
            <b>Access token</b>
            <small>
              {token
                ? token.accessSecondsLeft > 0
                  ? `Живёт ещё ${formatLifetime(token.accessSecondsLeft)}`
                  : "Срок действия истёк"
                : detailsLoading
                  ? "Читаю JWT…"
                  : "Срок неизвестен"}
            </small>
          </div>
          <span className={`refresh-state ${token?.hasRefresh ? "ok" : ""}`}>
            {token?.hasRefresh ? "REFRESH ЕСТЬ" : "НЕТ REFRESH"}
          </span>
          <button
            className="ghost compact-action"
            disabled={refreshingToken || !token?.hasRefresh}
            onClick={onRefreshToken}
          >
            {refreshingToken ? (
              <LoaderCircle className="spin" size={14} />
            ) : (
              <RefreshCw size={14} />
            )}
            Продлить
          </button>
        </div>

        <div className="live-facts">
          <div>
            <Globe2 size={15} />
            <span>
              <small>Страна счёта</small>
              <b>
                {live?.country || "—"}
                {live?.currency ? ` · ${live.currency}` : ""}
              </b>
            </span>
          </div>
          <div>
            <CalendarClock size={15} />
            <span>
              <small>Подписка до</small>
              <b>{token?.subscriptionUntil || "—"}</b>
            </span>
          </div>
          <div>
            <CreditCard size={15} />
            <span>
              <small>Способы оплаты</small>
              <b>{live ? live.paymentMethods.length : "—"}</b>
            </span>
          </div>
        </div>

        {!!live?.paymentMethods.length && (
          <div className="payment-list">
            {live.paymentMethods.map((method, index) => (
              <div key={`${method.label}-${method.last4 || index}`}>
                <CreditCard size={14} />
                <b>{method.label}</b>
                <span>
                  {method.last4 ? `···${method.last4}` : method.handle || ""}
                  {method.expires ? ` · до ${method.expires}` : ""}
                </span>
              </div>
            ))}
          </div>
        )}

        {!!live?.warnings.length && (
          <div className="detail-warnings">
            {live.warnings.map((warning) => (
              <span key={warning}>{warning}</span>
            ))}
          </div>
        )}

        <dl className="account-meta">
          <div>
            <dt>Последний refresh</dt>
            <dd>{token?.lastRefresh ? formatDate(token.lastRefresh) : "—"}</dd>
          </div>
          <div>
            <dt>Добавлен</dt>
            <dd>{formatDate(profile.createdAt)}</dd>
          </div>
          <div>
            <dt>ID профиля</dt>
            <dd title={profile.id}>{profile.id}</dd>
          </div>
          <div>
            <dt>Локальный файл</dt>
            <dd title={profile.fileName}>{profile.fileName}</dd>
          </div>
        </dl>
      </div>

      <div className="detail-actions">
        <button className="ghost" disabled={loading} onClick={onRefresh}>
          {loading ? (
            <LoaderCircle className="spin" size={15} />
          ) : (
            <RefreshCw size={15} />
          )}
          Проверить
        </button>
        <button
          className="primary"
          disabled={active || activating}
          onClick={onActivate}
        >
          {activating ? (
            <LoaderCircle className="spin" size={15} />
          ) : (
            <Check size={15} />
          )}
          {active ? "Уже активен" : "Переключиться"}
        </button>
      </div>
    </section>
  );
}

function DetailUsage({
  title,
  window,
}: {
  title: string;
  window?: RateLimit["primary"];
}) {
  const used = Math.min(100, Math.max(0, window?.usedPercent ?? 0));
  const remaining = Math.round(100 - used);
  return (
    <div className="detail-limit">
      <div>
        <span>{title}</span>
        <b>{window ? `${remaining}%` : "—"}</b>
      </div>
      <i>
        <em style={{ width: window ? `${used}%` : "0%" }} />
      </i>
      <small>
        <CalendarClock size={13} />
        {window
          ? `Сброс ${formatReset(window.resetsAt)}`
          : "Данные пока недоступны"}
      </small>
    </div>
  );
}

function formatDate(value: string) {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString("ru-RU");
}

function formatLifetime(seconds: number) {
  if (seconds <= 0) return "0 ч";
  const hours = Math.ceil(seconds / 3600);
  return hours >= 48 ? `${Math.round(hours / 24)} дн.` : `${hours} ч`;
}

function formatReset(timestamp: number) {
  const milliseconds =
    timestamp > 10_000_000_000 ? timestamp : timestamp * 1000;
  const date = new Date(milliseconds);
  return Number.isNaN(date.getTime())
    ? "неизвестен"
    : date.toLocaleString("ru-RU", {
        day: "2-digit",
        month: "2-digit",
        hour: "2-digit",
        minute: "2-digit",
      });
}

function PlanCell({ rate, loading }: { rate?: RateLimit; loading: boolean }) {
  if (loading && !rate)
    return (
      <span className="plan muted">
        <LoaderCircle className="spin" size={15} />
      </span>
    );
  return (
    <span className={`plan ${rate?.planType ? "known" : "muted"}`}>
      {rate?.planType?.toUpperCase() || "—"}
    </span>
  );
}

function UsageCell({ window }: { window?: RateLimit["primary"] }) {
  if (!window) return <span className="usage empty-usage">—</span>;
  const used = Math.min(100, Math.max(0, window.usedPercent));
  return (
    <span className="usage">
      <span>
        <b>{Math.round(100 - used)}%</b>
        <small>осталось</small>
      </span>
      <i>
        <em style={{ width: `${used}%` }} />
      </i>
    </span>
  );
}
