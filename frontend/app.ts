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

type Point = {
  x: number;
  y: number;
};

type DraftContext = {
  mode: "fork" | "combine";
  sourceIds: string[];
  label: string;
};

type PointerState =
  | {
      mode: "pan";
      pointerId: number;
      startClient: Point;
      startPan: Point;
    }
  | {
      mode: "drag";
      pointerId: number;
      runId: string;
      startClient: Point;
      startPos: Point;
      moved: boolean;
    }
  | null;

type StormState = {
  runs: StormRun[];
  positions: Map<string, Point>;
  lineage: Map<string, string[]>;
  activeRunId: string | null;
  focusedRunId: string | null;
  combineSourceId: string | null;
  draftContext: DraftContext | null;
  pendingLineage: DraftContext | null;
  pan: Point;
  scale: number;
  pointerState: PointerState;
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

const BOARD_WIDTH = 2600;
const BOARD_HEIGHT = 1800;
const CARD_WIDTH = 310;
const CARD_HEIGHT = 332;
const INITIAL_PAN: Point = { x: 160, y: 120 };

let clerk: ClerkLike | null = null;
let isSyncing = false;
let authPollTimer: number | null = null;

const state: StormState = {
  runs: [],
  positions: new Map(),
  lineage: new Map(),
  activeRunId: null,
  focusedRunId: null,
  combineSourceId: null,
  draftContext: null,
  pendingLineage: null,
  pan: { ...INITIAL_PAN },
  scale: 1,
  pointerState: null,
};

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
  if (!config.clerkPublishableKey) return null;
  if (clerk) return clerk;

  const module = await import("@clerk/clerk-js");
  const instance = new module.Clerk(config.clerkPublishableKey);
  await instance.load();
  instance.addListener(async ({ session }) => {
    if (!session) return;
    if (getConfig().hasServerSession) return;
    const synced = await syncServerSession();
    if (synced) redirectToApp();
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
    if (!token) return false;

    const response = await fetch("/auth/session", {
      method: "POST",
      credentials: "include",
      headers: {
        "Content-Type": "application/json",
      },
      body: JSON.stringify({ token }),
    });

    if (!response.ok) return false;
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
  if (instance) await instance.signOut();
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

function runsContainer(): HTMLElement | null {
  return document.getElementById("storm-runs");
}

function boardElement(): HTMLElement | null {
  return document.getElementById("storm-board");
}

function canvasElement(): HTMLElement | null {
  return document.getElementById("storm-canvas");
}

function previewFrame(): HTMLIFrameElement | null {
  return document.getElementById("storm-preview") as HTMLIFrameElement | null;
}

function focusFrame(): HTMLIFrameElement | null {
  return document.getElementById("storm-focus-preview") as HTMLIFrameElement | null;
}

function stormStatusElement(): HTMLElement | null {
  return document.getElementById("storm-status");
}

function setStormStatus(message: string): void {
  const element = stormStatusElement();
  if (!element) return;
  element.textContent = message;
}

function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function initialsFromName(name: string): string {
  return name
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((part) => part[0]?.toUpperCase() ?? "")
    .join("");
}

function setAvatarInitials(): void {
  const element = document.getElementById("avatar-fallback");
  if (!element) return;
  const name = element.getAttribute("data-name") ?? "Design Storm";
  element.textContent = initialsFromName(name);
}

function getRun(runId: string | null): StormRun | null {
  if (!runId) return null;
  return state.runs.find((run) => run.id === runId) ?? null;
}

function assignPosition(run: StormRun, index: number, sourceIds?: string[]): void {
  if (state.positions.has(run.id)) return;

  if (sourceIds?.length) {
    const sourcePositions = sourceIds
      .map((sourceId) => state.positions.get(sourceId))
      .filter((point): point is Point => Boolean(point));
    if (sourcePositions.length > 0) {
      const centroid = sourcePositions.reduce(
        (memo, point) => ({ x: memo.x + point.x, y: memo.y + point.y }),
        { x: 0, y: 0 },
      );
      const offsetBase = sourceIds.length > 1 ? 170 : 250;
      state.positions.set(run.id, {
        x: centroid.x / sourcePositions.length + offsetBase,
        y: centroid.y / sourcePositions.length + 110,
      });
      return;
    }
  }

  const column = index % 4;
  const row = Math.floor(index / 4);
  state.positions.set(run.id, {
    x: 220 + column * 360 + (row % 2) * 70,
    y: 240 + row * 290 + (column % 2) * 28,
  });
}

function updateBoardTransform(): void {
  const board = boardElement();
  if (!board) return;
  board.style.transform = `translate(${state.pan.x}px, ${state.pan.y}px) scale(${state.scale})`;
  const zoom = document.getElementById("storm-zoom-readout");
  if (zoom) zoom.textContent = `${Math.round(state.scale * 100)}%`;
}

function syncUrl(replace = false): void {
  const url = new URL(window.location.href);
  url.searchParams.delete("inspect");
  url.searchParams.delete("focus");

  if (state.focusedRunId) {
    url.searchParams.set("focus", state.focusedRunId);
  } else if (state.activeRunId) {
    url.searchParams.set("inspect", state.activeRunId);
  }

  const next = `${url.pathname}${url.search}${url.hash}`;
  const current = `${window.location.pathname}${window.location.search}${window.location.hash}`;
  if (next === current) return;

  if (replace) {
    window.history.replaceState({}, "", next);
  } else {
    window.history.pushState({}, "", next);
  }
}

function applyUrlState(): void {
  const url = new URL(window.location.href);
  const focusedId = url.searchParams.get("focus");
  const inspectId = url.searchParams.get("inspect");

  if (focusedId && getRun(focusedId)) {
    state.focusedRunId = focusedId;
    state.activeRunId = focusedId;
    return;
  }

  if (inspectId && getRun(inspectId)) {
    state.focusedRunId = null;
    state.activeRunId = inspectId;
    return;
  }

  state.focusedRunId = null;
  if (state.activeRunId && !getRun(state.activeRunId)) {
    state.activeRunId = null;
  }
}

function buildForkPrompt(run: StormRun): string {
  return [
    `Fork the design language from "${run.title}".`,
    `Keep the strongest ideas from this artifact: ${run.summary}`,
    `Original seed: ${run.prompt}`,
    "Push it into a clearly distinct next branch rather than a mild variation.",
  ].join("\n\n");
}

function buildCombinePrompt(left: StormRun, right: StormRun): string {
  return [
    `Combine these two design directions into a new branch.`,
    `Direction A: "${left.title}" — ${left.summary}`,
    `Direction B: "${right.title}" — ${right.summary}`,
    `Original seed A: ${left.prompt}`,
    `Original seed B: ${right.prompt}`,
    "Do not average them out. Take structural logic from one and atmospheric tone from the other, then build a coherent hybrid design-language document.",
  ].join("\n\n");
}

function showDraftContext(context: DraftContext): void {
  state.draftContext = context;
  state.pendingLineage = context;
  const textarea = document.getElementById("storm-prompt") as HTMLTextAreaElement | null;
  if (!textarea) return;

  if (context.mode === "fork") {
    const source = getRun(context.sourceIds[0]);
    if (source) {
      textarea.value = buildForkPrompt(source);
      setStormStatus(`Forking from ${source.title}. Refine the prompt and generate a new branch.`);
    }
  } else {
    const left = getRun(context.sourceIds[0]);
    const right = getRun(context.sourceIds[1]);
    if (left && right) {
      textarea.value = buildCombinePrompt(left, right);
      setStormStatus(`Combining ${left.title} with ${right.title}. Refine the prompt and generate the hybrid branch.`);
    }
  }

  textarea.focus();
  renderDraftContext();
}

function clearDraftContext(options?: { keepStatus?: boolean }): void {
  state.draftContext = null;
  state.pendingLineage = null;
  state.combineSourceId = null;
  renderDraftContext();
  renderRuns();
  if (!options?.keepStatus) {
    setStormStatus("Seed a direction or select a card to branch from it.");
  }
}

function renderDraftContext(): void {
  const container = document.getElementById("storm-draft-context");
  const clear = document.getElementById("storm-clear-context") as HTMLButtonElement | null;
  if (!container || !clear) return;

  const context = state.draftContext;
  if (!context) {
    container.hidden = true;
    container.innerHTML = "";
    clear.disabled = true;
    return;
  }

  clear.disabled = false;
  container.hidden = false;

  if (context.mode === "fork") {
    const source = getRun(context.sourceIds[0]);
    container.innerHTML = `
      <span class="draft-pill">Fork</span>
      <div class="draft-copy">
        <strong>${escapeHtml(context.label)}</strong>
        <p>${escapeHtml(source?.summary ?? "Create a distinct next branch from the selected artifact.")}</p>
      </div>
    `;
    return;
  }

  const left = getRun(context.sourceIds[0]);
  const right = getRun(context.sourceIds[1]);
  container.innerHTML = `
    <span class="draft-pill">Combine</span>
    <div class="draft-copy">
      <strong>${escapeHtml(context.label)}</strong>
      <p>${escapeHtml(left?.title ?? "Artifact A")} + ${escapeHtml(right?.title ?? "Artifact B")}</p>
    </div>
  `;
}

function setActiveRun(runId: string | null, options?: { syncHistory?: boolean }): void {
  state.activeRunId = runId;
  if (!runId) {
    state.focusedRunId = null;
  }
  renderRuns();
  renderInspector();
  renderFocus();
  if (options?.syncHistory) syncUrl(false);
}

function openFullscreen(runId: string): void {
  state.activeRunId = runId;
  state.focusedRunId = runId;
  renderRuns();
  renderInspector();
  renderFocus();
  syncUrl(false);
}

function closeFullscreen(options?: { syncHistory?: boolean }): void {
  state.focusedRunId = null;
  renderFocus();
  if (options?.syncHistory ?? true) syncUrl(false);
}

function beginCombine(runId: string): void {
  if (state.combineSourceId === runId) {
    state.combineSourceId = null;
    setStormStatus("Combine cancelled.");
    renderRuns();
    return;
  }
  state.combineSourceId = runId;
  const run = getRun(runId);
  setStormStatus(`Combine mode: select another artifact to hybridize with ${run?.title ?? "this run"}.`);
  renderRuns();
}

function maybeComposeCombine(targetRunId: string): boolean {
  const sourceId = state.combineSourceId;
  if (!sourceId || sourceId === targetRunId) return false;
  const source = getRun(sourceId);
  const target = getRun(targetRunId);
  if (!source || !target) return false;

  state.combineSourceId = null;
  showDraftContext({
    mode: "combine",
    sourceIds: [source.id, target.id],
    label: `Hybridizing ${source.title} and ${target.title}`,
  });
  setActiveRun(target.id, { syncHistory: true });
  return true;
}

function handleRunAction(runId: string, action: string): void {
  const run = getRun(runId);
  if (!run) return;

  if (action === "inspect") {
    setActiveRun(run.id, { syncHistory: true });
    return;
  }

  if (action === "fullscreen") {
    openFullscreen(run.id);
    return;
  }

  if (action === "fork") {
    showDraftContext({
      mode: "fork",
      sourceIds: [run.id],
      label: `Forking ${run.title}`,
    });
    setActiveRun(run.id, { syncHistory: true });
    return;
  }

  if (action === "combine") {
    beginCombine(run.id);
  }
}

function renderConnections(): void {
  const svg = document.getElementById("storm-lines") as SVGElement | null;
  if (!svg) return;
  svg.setAttribute("viewBox", `0 0 ${BOARD_WIDTH} ${BOARD_HEIGHT}`);
  svg.innerHTML = "";

  state.lineage.forEach((parents, runId) => {
    const child = state.positions.get(runId);
    if (!child) return;

    parents.forEach((parentId) => {
      const parent = state.positions.get(parentId);
      if (!parent) return;

      const startX = parent.x + CARD_WIDTH * 0.5;
      const startY = parent.y + CARD_HEIGHT * 0.56;
      const endX = child.x + CARD_WIDTH * 0.5;
      const endY = child.y + 20;
      const controlY = startY + (endY - startY) * 0.45;

      const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
      path.setAttribute(
        "d",
        `M ${startX} ${startY} C ${startX} ${controlY}, ${endX} ${controlY}, ${endX} ${endY}`,
      );
      path.setAttribute("class", "storm-line");
      svg.appendChild(path);
    });
  });
}

function updateNodePosition(runId: string): void {
  const node = runsContainer()?.querySelector<HTMLElement>(`.storm-node[data-run-id="${runId}"]`);
  const point = state.positions.get(runId);
  if (!node || !point) return;
  node.style.transform = `translate(${point.x}px, ${point.y}px)`;
}

function renderRuns(): void {
  const container = runsContainer();
  const emptyState = document.getElementById("storm-empty-state");
  if (!container || !emptyState) return;

  if (state.runs.length === 0) {
    container.innerHTML = "";
    emptyState.hidden = false;
    renderConnections();
    return;
  }

  emptyState.hidden = true;
  container.innerHTML = "";

  state.runs.forEach((run, index) => {
    const lineage = state.lineage.get(run.id);
    assignPosition(run, index, lineage);
    const point = state.positions.get(run.id) ?? { x: 240, y: 240 };

    const card = document.createElement("article");
    card.className = "storm-node";
    if (run.id === state.activeRunId) card.classList.add("is-active");
    if (run.id === state.combineSourceId) card.classList.add("is-combine-source");
    card.dataset.runId = run.id;
    card.style.transform = `translate(${point.x}px, ${point.y}px)`;
    card.innerHTML = `
      <div class="storm-node-shell">
        <div class="storm-node-meta">
          <span class="meta-note">${new Date(run.createdAt).toLocaleDateString()}</span>
          <span class="run-pill ${run.submitted ? "" : "muted"}">${run.submitted ? "submitted" : "draft"}</span>
        </div>

        <div class="storm-node-preview">
          <iframe src="${escapeHtml(run.previewUrl)}" loading="lazy" tabindex="-1" aria-hidden="true"></iframe>
        </div>

        <div class="storm-node-copy">
          <h3>${escapeHtml(run.title)}</h3>
          <p>${escapeHtml(run.summary)}</p>
        </div>

        <div class="storm-node-actions">
          <button class="node-action" type="button" data-run-action="inspect">Inspect</button>
          <button class="node-action" type="button" data-run-action="fullscreen">Open</button>
          <button class="node-action" type="button" data-run-action="fork">Fork</button>
          <button class="node-action" type="button" data-run-action="combine">${run.id === state.combineSourceId ? "Cancel" : "Combine"}</button>
        </div>
      </div>
    `;
    container.appendChild(card);
  });

  renderConnections();
}

function renderInspector(): void {
  const panel = document.getElementById("storm-inspector");
  const title = document.getElementById("active-run-title");
  const summary = document.getElementById("storm-summary");
  const prompt = document.getElementById("inspector-prompt");
  const agentSummary = document.getElementById("storm-notes");
  const created = document.getElementById("inspector-created");
  const statusPill = document.getElementById("inspector-status-pill");
  const iframe = previewFrame();
  const fork = document.getElementById("inspector-fork") as HTMLButtonElement | null;
  const combine = document.getElementById("inspector-combine") as HTMLButtonElement | null;
  const fullscreen = document.getElementById("inspector-fullscreen") as HTMLButtonElement | null;

  const run = getRun(state.activeRunId);
  if (!panel || !title || !summary || !prompt || !agentSummary || !created || !statusPill || !iframe || !fork || !combine || !fullscreen) {
    return;
  }

  if (!run) {
    panel.classList.add("is-empty");
    title.textContent = "No artifact selected";
    summary.textContent = "Select an artifact card to inspect it in place.";
    prompt.textContent = "The selected artifact’s original seed will appear here.";
    agentSummary.textContent = "The agent summary will appear here after generation.";
    created.textContent = "Waiting for a run";
    statusPill.textContent = "Canvas";
    statusPill.className = "run-pill muted";
    iframe.removeAttribute("src");
    fork.disabled = true;
    combine.disabled = true;
    fullscreen.disabled = true;
    return;
  }

  panel.classList.remove("is-empty");
  title.textContent = run.title;
  summary.textContent = run.summary;
  prompt.textContent = run.prompt;
  agentSummary.textContent = run.assistantSummary || "No summary returned.";
  created.textContent = new Date(run.createdAt).toLocaleString();
  statusPill.textContent = run.submitted ? "Submitted" : "Draft";
  statusPill.className = `run-pill ${run.submitted ? "" : "muted"}`;
  iframe.src = run.previewUrl;
  fork.disabled = false;
  combine.disabled = false;
  fullscreen.disabled = false;
}

function renderFocus(): void {
  const overlay = document.getElementById("storm-focus");
  const frame = focusFrame();
  const title = document.getElementById("storm-focus-title");
  const run = getRun(state.focusedRunId);
  if (!overlay || !frame || !title) return;

  if (!run) {
    overlay.hidden = true;
    overlay.setAttribute("aria-hidden", "true");
    frame.removeAttribute("src");
    return;
  }

  overlay.hidden = false;
  overlay.setAttribute("aria-hidden", "false");
  frame.src = run.previewUrl;
  title.textContent = run.title;
}

async function loadStorms(): Promise<void> {
  const response = await fetch("/api/storms", {
    credentials: "include",
  });
  if (!response.ok) return;
  const runs = (await response.json()) as StormRun[];
  state.runs = runs.sort((left, right) => Date.parse(left.createdAt) - Date.parse(right.createdAt));
  applyUrlState();
  renderRuns();
  renderInspector();
  renderFocus();
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
    const pending = state.pendingLineage;
    if (pending) {
      state.lineage.set(payload.run.id, pending.sourceIds);
      assignPosition(payload.run, state.runs.length, pending.sourceIds);
    }

    clearDraftContext({ keepStatus: true });
    textarea.value = "";
    setStormStatus("Storm generated.");
    await loadStorms();
    setActiveRun(payload.run.id, { syncHistory: true });
  } finally {
    submit.disabled = false;
  }
}

function toggleAccountSheet(force?: boolean): void {
  const sheet = document.getElementById("account-sheet");
  const button = document.getElementById("avatar-button");
  if (!sheet || !button) return;

  const nextOpen = force ?? sheet.hidden;
  sheet.hidden = !nextOpen;
  button.setAttribute("aria-expanded", nextOpen ? "true" : "false");
}

function closeAccountSheet(): void {
  toggleAccountSheet(false);
}

function resetView(): void {
  state.pan = { ...INITIAL_PAN };
  state.scale = 1;
  updateBoardTransform();
}

function bindCanvasInteractions(): void {
  const canvas = canvasElement();
  if (!canvas) return;

  canvas.addEventListener("wheel", (event) => {
    event.preventDefault();
    const nextScale = Math.max(0.68, Math.min(1.42, state.scale - event.deltaY * 0.001));
    state.scale = Number(nextScale.toFixed(2));
    updateBoardTransform();
  });

  canvas.addEventListener("pointerdown", (event) => {
    const target = event.target as HTMLElement;
    if (target.closest(".storm-node")) return;

    state.pointerState = {
      mode: "pan",
      pointerId: event.pointerId,
      startClient: { x: event.clientX, y: event.clientY },
      startPan: { ...state.pan },
    };
    canvas.setPointerCapture(event.pointerId);
  });

  canvas.addEventListener("pointermove", (event) => {
    if (!state.pointerState || state.pointerState.mode !== "pan" || state.pointerState.pointerId !== event.pointerId) {
      return;
    }

    state.pan = {
      x: state.pointerState.startPan.x + (event.clientX - state.pointerState.startClient.x),
      y: state.pointerState.startPan.y + (event.clientY - state.pointerState.startClient.y),
    };
    updateBoardTransform();
  });

  canvas.addEventListener("pointerup", (event) => {
    if (state.pointerState?.pointerId === event.pointerId) {
      state.pointerState = null;
      canvas.releasePointerCapture(event.pointerId);
    }
  });
}

function bindNodeInteractions(): void {
  const container = runsContainer();
  if (!container) return;

  container.addEventListener("click", (event) => {
    const target = event.target as HTMLElement;
    const action = target.closest<HTMLElement>("[data-run-action]")?.dataset.runAction;
    const node = target.closest<HTMLElement>(".storm-node");
    const runId = node?.dataset.runId;
    if (!runId) return;

    if (action) {
      handleRunAction(runId, action);
      return;
    }

    if (state.pointerState?.mode === "drag" && state.pointerState.runId === runId && state.pointerState.moved) {
      return;
    }

    if (maybeComposeCombine(runId)) return;
    setActiveRun(runId, { syncHistory: true });
  });

  container.addEventListener("dblclick", (event) => {
    const target = event.target as HTMLElement;
    const node = target.closest<HTMLElement>(".storm-node");
    const runId = node?.dataset.runId;
    if (!runId) return;
    openFullscreen(runId);
  });

  container.addEventListener("pointerdown", (event) => {
    const target = event.target as HTMLElement;
    if (target.closest("[data-run-action]")) return;

    const node = target.closest<HTMLElement>(".storm-node");
    const runId = node?.dataset.runId;
    if (!node || !runId) return;

    const point = state.positions.get(runId);
    if (!point) return;

    state.pointerState = {
      mode: "drag",
      pointerId: event.pointerId,
      runId,
      startClient: { x: event.clientX, y: event.clientY },
      startPos: { ...point },
      moved: false,
    };

    node.setPointerCapture(event.pointerId);
  });

  container.addEventListener("pointermove", (event) => {
    if (!state.pointerState || state.pointerState.mode !== "drag" || state.pointerState.pointerId !== event.pointerId) {
      return;
    }

    const dx = (event.clientX - state.pointerState.startClient.x) / state.scale;
    const dy = (event.clientY - state.pointerState.startClient.y) / state.scale;
    const nextPoint = {
      x: state.pointerState.startPos.x + dx,
      y: state.pointerState.startPos.y + dy,
    };

    if (Math.abs(dx) > 4 || Math.abs(dy) > 4) {
      state.pointerState.moved = true;
    }

    state.positions.set(state.pointerState.runId, nextPoint);
    updateNodePosition(state.pointerState.runId);
    renderConnections();
  });

  container.addEventListener("pointerup", (event) => {
    if (!state.pointerState || state.pointerState.mode !== "drag" || state.pointerState.pointerId !== event.pointerId) {
      return;
    }
    const node = (event.target as HTMLElement).closest<HTMLElement>(".storm-node");
    node?.releasePointerCapture(event.pointerId);
    state.pointerState = null;
  });
}

function bindAppChrome(): void {
  const form = document.getElementById("storm-form");
  const clearDraft = document.getElementById("storm-clear-context");
  const reset = document.getElementById("storm-reset-view");
  const avatar = document.getElementById("avatar-button");
  const accountSheet = document.getElementById("account-sheet");
  const fullscreenBack = document.getElementById("storm-focus-back");
  const overlay = document.getElementById("storm-focus");
  const inspectorFork = document.getElementById("inspector-fork");
  const inspectorCombine = document.getElementById("inspector-combine");
  const inspectorFullscreen = document.getElementById("inspector-fullscreen");

  form?.addEventListener("submit", (event) => {
    void submitStorm(event);
  });

  clearDraft?.addEventListener("click", () => clearDraftContext());
  reset?.addEventListener("click", resetView);

  avatar?.addEventListener("click", (event) => {
    event.stopPropagation();
    toggleAccountSheet();
  });

  accountSheet?.addEventListener("click", (event) => {
    const target = event.target as HTMLElement;
    if (target.closest("[data-action='close-account-sheet']")) {
      closeAccountSheet();
    }
  });

  document.addEventListener("click", (event) => {
    const target = event.target as HTMLElement;
    if (target.closest("#account-sheet") || target.closest("#avatar-button")) return;
    closeAccountSheet();
  });

  overlay?.addEventListener("click", (event) => {
    const target = event.target as HTMLElement;
    if (target.closest("[data-action='close-focus']")) {
      closeFullscreen();
    }
  });

  fullscreenBack?.addEventListener("click", () => closeFullscreen());

  inspectorFork?.addEventListener("click", () => {
    if (state.activeRunId) handleRunAction(state.activeRunId, "fork");
  });

  inspectorCombine?.addEventListener("click", () => {
    if (state.activeRunId) handleRunAction(state.activeRunId, "combine");
  });

  inspectorFullscreen?.addEventListener("click", () => {
    if (state.activeRunId) handleRunAction(state.activeRunId, "fullscreen");
  });

  window.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      if (state.focusedRunId) {
        closeFullscreen();
        return;
      }
      closeAccountSheet();
    }
  });

  window.addEventListener("popstate", () => {
    applyUrlState();
    renderRuns();
    renderInspector();
    renderFocus();
  });
}

function bindStormApp(): void {
  if (getConfig().currentPath !== "/app") return;
  setAvatarInitials();
  renderDraftContext();
  updateBoardTransform();
  bindCanvasInteractions();
  bindNodeInteractions();
  bindAppChrome();
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
