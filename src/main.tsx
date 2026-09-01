import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import "./styles.css";

if (
  import.meta.env.DEV &&
  new URLSearchParams(window.location.search).has("readme-demo")
) {
  const profiles = [
    {
      id: "demo-aurora",
      name: "Aurora Plus",
      fileName: "aurora.json",
      createdAt: "2026-08-28T18:42:00Z",
    },
    {
      id: "demo-atlas",
      name: "Atlas Team",
      fileName: "atlas.json",
      createdAt: "2026-08-29T10:15:00Z",
    },
    {
      id: "demo-nova",
      name: "Nova Pro",
      fileName: "nova.json",
      createdAt: "2026-08-30T13:05:00Z",
    },
    {
      id: "demo-orbit",
      name: "Orbit Plus",
      fileName: "orbit.json",
      createdAt: "2026-08-31T09:20:00Z",
    },
  ];
  const rates: Record<string, unknown> = {
    "demo-aurora": {
      planType: "plus",
      primary: {
        usedPercent: 18,
        windowDurationMins: 300,
        resetsAt: 1788280200,
      },
      secondary: {
        usedPercent: 31,
        windowDurationMins: 10080,
        resetsAt: 1788854400,
      },
    },
    "demo-atlas": {
      planType: "team",
      primary: {
        usedPercent: 44,
        windowDurationMins: 300,
        resetsAt: 1788283800,
      },
      secondary: {
        usedPercent: 12,
        windowDurationMins: 10080,
        resetsAt: 1788940800,
      },
    },
    "demo-nova": {
      planType: "pro",
      primary: {
        usedPercent: 7,
        windowDurationMins: 300,
        resetsAt: 1788287400,
      },
      secondary: {
        usedPercent: 23,
        windowDurationMins: 10080,
        resetsAt: 1789027200,
      },
    },
    "demo-orbit": {
      planType: "plus",
      primary: {
        usedPercent: 63,
        windowDurationMins: 300,
        resetsAt: 1788291000,
      },
      secondary: {
        usedPercent: 48,
        windowDurationMins: 10080,
        resetsAt: 1789113600,
      },
    },
  };
  const invoke = async (
    command: string,
    args: Record<string, string> = {},
  ): Promise<unknown> => {
    if (command === "load_snapshot")
      return {
        profiles,
        activeId: "demo-aurora",
        codexAuthPath: "~/.codex/auth.json",
      };
    if (command === "keycrop_status")
      return { connected: true, user: { username: "demo" } };
    if (command === "keycrop_sync_accounts")
      return {
        purchasesSeen: 4,
        accountsFound: 4,
        imported: 0,
        duplicates: 4,
        captured: 0,
        recovered: 0,
        rejected: 0,
      };
    if (command === "probe_rate_limits") return rates[args.profileId];
    if (command === "broker_status")
      return {
        running: true,
        configured: true,
        url: "http://127.0.0.1:1456/oauth/token",
      };
    if (command === "profile_token_status")
      return {
        email: "demo@example.com",
        planType: "plus",
        subscriptionUntil: "2026-10-17",
        accessExpiresAt: 1788512400,
        idExpiresAt: 1788512400,
        accessSecondsLeft: 259200,
        hasRefresh: true,
        lastRefresh: "2026-09-01T10:35:31Z",
      };
    if (command === "account_live_info")
      return {
        token: await invoke("profile_token_status"),
        livePlan: "plus",
        country: "US",
        currency: "USD",
        paymentMethods: [
          { label: "Visa", last4: "4242", expires: "12/2030" },
          { label: "PayPal", handle: "de···@example.com" },
        ],
        warnings: [],
      };
    return null;
  };
  (
    window as unknown as { __TAURI_INTERNALS__: { invoke: typeof invoke } }
  ).__TAURI_INTERNALS__ = { invoke };
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
