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

type StormRun = {
  id: string;
  prompt: string;
  title: string;
  summary: string;
  assistantSummary: string;
  previewUrl: string;
  submitted: boolean;
  createdAt: string;
};

type CodexStartResponse = {
  verifyUrl: string;
  userCode: string;
  intervalSeconds: number;
};

type CodexPollResponse = {
  status: "idle" | "pending" | "connected";
  message?: string | null;
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
    designstormSettings: {
      connectCodex(): Promise<void>;
      disconnectProvider(): Promise<void>;
    };
  }
}

let clerk: ClerkLike | null = null;
let isSyncing = false;
let authPollTimer: number | null = null;
let activeRunId: string | null = null;

function getConfig(): AppConfig {
  const element = document.getElementById("app-config");
  if (!element?.textContent) {
    throw new Error("Missing app config.");
  }
  return JSON.parse(element.textContent) as AppConfig;
}

function redirectToApp(): void {
  if (window.location.pathname !== "/app") {
    window.location.href = "/app";
  }
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
    if (getConfig().hasServerSession) return;
    const synced = await syncServerSession();
    if (synced) {
      redirectToApp();
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
  panel.outerHTML = await response.text();
}

async function signIn(): Promise<void> {
  const instance = await ensureClerk();
  if (!instance) return;
  const synced = await syncServerSession();
  if (synced) {
    redirectToApp();
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

function providerStatusElement(): HTMLElement | null {
  return document.getElementById("provider-connect-status");
}

function setProviderStatus(message: string, tone: "muted" | "error" | "success" = "muted"): void {
  const element = providerStatusElement();
  if (!element) return;
  element.textContent = message;
  element.classList.remove("is-error", "is-success");
  if (tone === "error") element.classList.add("is-error");
  if (tone === "success") element.classList.add("is-success");
}

async function refreshProviderPanel(): Promise<void> {
  const panel = document.getElementById("provider-panel");
  if (!panel) return;
  const response = await fetch("/settings/provider", {
    credentials: "include",
    headers: {
      Accept: "text/html",
    },
  });
  if (!response.ok) return;
  panel.outerHTML = await response.text();
}

async function connectCodex(): Promise<void> {
  setProviderStatus("Starting Codex device flow...");
  const response = await fetch("/settings/provider/codex/start", {
    method: "POST",
    credentials: "include",
  });
  if (!response.ok) {
    setProviderStatus("Failed to start Codex OAuth.", "error");
    return;
  }

  const payload = (await response.json()) as CodexStartResponse;
  setProviderStatus(`Enter code ${payload.userCode} in the OpenAI window.`);
  window.open(payload.verifyUrl, "_blank", "noopener,noreferrer");
  startCodexPolling(payload.intervalSeconds);
}

function startCodexPolling(intervalSeconds: number): void {
  if (authPollTimer !== null) {
    window.clearTimeout(authPollTimer);
  }

  const tick = async (): Promise<void> => {
    const response = await fetch("/settings/provider/codex/poll", {
      method: "POST",
      credentials: "include",
    });
    if (!response.ok) {
      setProviderStatus("Failed to verify Codex authorization.", "error");
      return;
    }

    const payload = (await response.json()) as CodexPollResponse;
    if (payload.status === "connected") {
      setProviderStatus(payload.message ?? "Codex connected.", "success");
      await refreshProviderPanel();
      return;
    }

    if (payload.status === "pending") {
      setProviderStatus(payload.message ?? "Waiting for Codex approval...");
      authPollTimer = window.setTimeout(() => void tick(), intervalSeconds * 1000);
      return;
    }

    setProviderStatus("No pending Codex auth session.");
  };

  authPollTimer = window.setTimeout(() => void tick(), intervalSeconds * 1000);
}

async function disconnectProvider(): Promise<void> {
  const response = await fetch("/settings/provider/logout", {
    method: "POST",
    credentials: "include",
  });
  if (!response.ok) {
    setProviderStatus("Failed to disconnect provider.", "error");
    return;
  }
  await refreshProviderPanel();
  setProviderStatus("Provider disconnected.");
}

function stormStatusElement(): HTMLElement | null {
  return document.getElementById("storm-status");
}

function setStormStatus(message: string): void {
  const element = stormStatusElement();
  if (!element) return;
  element.textContent = message;
}

function previewFrame(): HTMLIFrameElement | null {
  return document.getElementById("storm-preview") as HTMLIFrameElement | null;
}

function renderNotes(run: StormRun): void {
  const notes = document.getElementById("storm-notes");
  if (!notes) return;
  notes.innerHTML = "";

  const summary = document.createElement("article");
  summary.className = "stage-note";
  summary.innerHTML = `<strong>Artifact</strong><div>${escapeHtml(run.summary)}</div>`;
  notes.appendChild(summary);

  const assistant = document.createElement("article");
  assistant.className = "stage-note";
  assistant.innerHTML = `<strong>Agent Summary</strong><div>${escapeHtml(run.assistantSummary || "No summary returned.")}</div>`;
  notes.appendChild(assistant);
}

function setActiveRun(run: StormRun): void {
  activeRunId = run.id;
  document.querySelectorAll<HTMLElement>(".storm-run-card").forEach((node) => {
    node.classList.toggle("is-active", node.dataset.runId === run.id);
  });

  const title = document.getElementById("active-run-title");
  const summary = document.getElementById("storm-summary");
  const iframe = previewFrame();

  if (title) title.textContent = run.title;
  if (summary) summary.textContent = run.summary;
  if (iframe) iframe.src = run.previewUrl;
  renderNotes(run);
}

function renderRuns(runs: StormRun[]): void {
  const container = document.getElementById("storm-runs");
  if (!container) return;

  if (runs.length === 0) {
    container.className = "storm-runs empty-state";
    container.textContent = "No runs yet.";
    const title = document.getElementById("active-run-title");
    const summary = document.getElementById("storm-summary");
    const notes = document.getElementById("storm-notes");
    const iframe = previewFrame();
    if (title) title.textContent = "No artifact selected";
    if (summary) summary.textContent = "Generate a storm to see the rendered HTML artifact here.";
    if (notes) notes.innerHTML = "";
    if (iframe) iframe.removeAttribute("src");
    return;
  }

  container.className = "storm-runs";
  container.innerHTML = "";

  runs.forEach((run, index) => {
    const card = document.createElement("article");
    card.className = "storm-run-card";
    card.dataset.runId = run.id;
    card.innerHTML = `
      <div class="run-row">
        <h3>${escapeHtml(run.title)}</h3>
        <span class="run-pill ${run.submitted ? "" : "muted"}">${run.submitted ? "submitted" : "draft"}</span>
      </div>
      <p>${escapeHtml(run.summary)}</p>
      <span class="meta-note">${new Date(run.createdAt).toLocaleString()}</span>
    `;
    card.addEventListener("click", () => setActiveRun(run));
    container.appendChild(card);

    if (index === 0 && (!activeRunId || activeRunId === run.id)) {
      setActiveRun(run);
    }
  });
}

async function loadStorms(): Promise<void> {
  const response = await fetch("/api/storms", {
    credentials: "include",
  });
  if (!response.ok) return;
  const runs = (await response.json()) as StormRun[];
  renderRuns(runs);
}

async function submitStorm(event: Event): Promise<void> {
  event.preventDefault();
  const textarea = document.getElementById("storm-prompt") as HTMLTextAreaElement | null;
  const submit = document.getElementById("storm-submit") as HTMLButtonElement | null;
  if (!textarea || !submit) return;

  const prompt = textarea.value.trim();
  if (!prompt) {
    setStormStatus("Seed prompt required.");
    return;
  }

  submit.disabled = true;
  setStormStatus("Generating storm...");

  try {
    const response = await fetch("/api/storms", {
      method: "POST",
      credentials: "include",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ prompt }),
    });

    if (!response.ok) {
      const error = await response.text();
      setStormStatus(error || "Storm generation failed.");
      return;
    }

    const payload = (await response.json()) as { run: StormRun };
    setStormStatus("Storm generated.");
    await loadStorms();
    setActiveRun(payload.run);
  } finally {
    submit.disabled = false;
  }
}

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function bindStormApp(): void {
  if (getConfig().currentPath !== "/app") return;
  const form = document.getElementById("storm-form");
  form?.addEventListener("submit", (event) => {
    void submitStorm(event);
  });
  void loadStorms();
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
      redirectToApp();
    }
  }

  bindStormApp();
}

window.designstormAuth = {
  signIn,
  signUp,
  signOut,
  refreshPanel,
};

window.designstormSettings = {
  connectCodex,
  disconnectProvider,
};

void bootstrap();
