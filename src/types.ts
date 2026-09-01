export type Profile = {
  id: string;
  name: string;
  fileName: string;
  createdAt: string;
  colorName?: string | null;
};

export type AppSnapshot = {
  profiles: Profile[];
  activeId?: string | null;
  codexAuthPath: string;
};

export type KeyCropUser = {
  id?: string;
  email?: string;
  username?: string;
  role?: string;
  [key: string]: unknown;
};

export type KeyCropStatus = {
  connected: boolean;
  user?: KeyCropUser | null;
};

export type RateWindow = {
  usedPercent: number;
  windowDurationMins: number;
  resetsAt: number;
};

export type RateLimit = {
  planType?: string;
  primary?: RateWindow | null;
  secondary?: RateWindow | null;
};

export type TokenStatus = {
  email?: string | null;
  planType?: string | null;
  subscriptionUntil?: string | null;
  accessExpiresAt: number;
  idExpiresAt: number;
  accessSecondsLeft: number;
  hasRefresh: boolean;
  lastRefresh?: string | null;
};

export type PaymentMethodInfo = {
  label: string;
  last4?: string | null;
  handle?: string | null;
  expires?: string | null;
};

export type AccountLiveInfo = {
  token: TokenStatus;
  livePlan?: string | null;
  country?: string | null;
  currency?: string | null;
  paymentMethods: PaymentMethodInfo[];
  warnings: string[];
};

export type BrokerStatus = {
  running: boolean;
  configured: boolean;
  url: string;
  error?: string | null;
};
