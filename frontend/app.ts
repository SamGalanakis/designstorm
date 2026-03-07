type AppConfig = {
  clerkPublishableKey: string;
  appUrl: string;
  hasServerSession: boolean;
  currentPath: string;
};

type ClerkLike = {
  session?: {
    getToken(): Promise<string | null>;
  } | null;
  load(): Promise<void>;
  openSignIn(): void;
  openSignUp(): void;
  signOut(): Promise<void>;
  addListener(listener: (payload: { session?: unknown }) => void): void;
};

declare global {
  interface Window {
    Clerk?: new (publishableKey: string) => ClerkLike;
    designstormAuth: {
      signIn(): Promise<void>;
      signUp(): Promise<void>;
      signOut(): Promise<void>;
      refreshPanel(): Promise<void>;
    };
  }
}

let clerk: ClerkLike | null = null;
let isSyncing = false;

function getConfig(): AppConfig {
  const element = document.getElementById("app-config");
  if (!element?.textContent) {
    throw new Error("Missing app config.");
  }
  return JSON.parse(element.textContent) as AppConfig;
}

async function ensureClerk(): Promise<ClerkLike | null> {
  const config = getConfig();
  if (!config.clerkPublishableKey) {
    return null;
  }

  if (clerk) return clerk;

  const module = await import("@clerk/clerk-js");
  const instance = new module.Clerk(config.clerkPublishableKey);
  await instance.load();
  instance.addListener(async ({ session }) => {
    if (!session) return;
    const synced = await syncServerSession();
    if (synced) {
      window.location.href = "/app";
    }
  });
  clerk = instance;
  return clerk;
}

async function syncServerSession(): Promise<boolean> {
  if (isSyncing) return false;
  isSyncing = true;
  try {
    const instance = await ensureClerk();
    const token = await instance?.session?.getToken();
    if (!token) {
      return false;
    }

    const response = await fetch("/auth/session", {
      method: "POST",
      credentials: "include",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ token }),
    });

    if (!response.ok) {
      return false;
    }

    await refreshPanel();
    return true;
  } finally {
    isSyncing = false;
  }
}

async function refreshPanel(): Promise<void> {
  const panel = document.getElementById("auth-panel");
  if (!panel) return;

  const response = await fetch("/partials/auth-panel", {
    credentials: "include",
    headers: {
      Accept: "text/html",
    },
  });
  if (!response.ok) return;
  const html = await response.text();
  panel.outerHTML = html;
}

async function signIn(): Promise<void> {
  const instance = await ensureClerk();
  if (!instance) return;
  const synced = await syncServerSession();
  if (synced) {
    window.location.href = "/app";
    return;
  }
  instance.openSignIn();
}

async function signUp(): Promise<void> {
  const instance = await ensureClerk();
  if (!instance) return;
  instance.openSignUp();
}

async function signOut(): Promise<void> {
  const instance = await ensureClerk();
  await fetch("/auth/logout", {
    method: "POST",
    credentials: "include",
  });
  if (instance) {
    await instance.signOut();
  }
  window.location.href = "/";
}

async function bootstrap(): Promise<void> {
  const instance = await ensureClerk();
  if (!instance) return;

  const authResponse = await fetch("/auth/me", {
    credentials: "include",
  });
  const authState = authResponse.ok
    ? ((await authResponse.json()) as { authenticated: boolean })
    : { authenticated: false };

  if (!authState.authenticated && instance.session) {
    const synced = await syncServerSession();
    if (synced && getConfig().currentPath === "/") {
      window.location.href = "/app";
    }
  }
}

window.designstormAuth = {
  signIn,
  signUp,
  signOut,
  refreshPanel,
};

void bootstrap();

