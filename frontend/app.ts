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

type Point = { x: number; y: number };

type DraftContext = {
  mode: "fork" | "combine";
  sourceIds: string[];
  label: string;
};

type PointerState =
  | { mode: "pan"; pointerId: number; startClient: Point; startPan: Point }
  | { mode: "drag"; pointerId: number; runId: string; startClient: Point; startPos: Point; moved: boolean }
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

// ─── Helpers ───

function $(id: string): HTMLElement | null {
  return document.getElementById(id);
}

function getConfig(): AppConfig {
  const el = $("app-config");
  if (!el?.textContent) throw new Error("Missing app config.");
  return JSON.parse(el.textContent) as AppConfig;
}

function escapeHtml(s: string): string {
  return s.replaceAll("&", "&amp;").replaceAll("<", "&lt;").replaceAll(">", "&gt;").replaceAll('"', "&quot;").replaceAll("'", "&#39;");
}

function redirectToApp(): void {
  if (window.location.pathname !== "/app") window.location.href = "/app";
}

// ─── Auth ───

async function ensureClerk(): Promise<ClerkLike | null> {
  const config = getConfig();
  if (!config.clerkPublishableKey) return null;
  if (clerk) return clerk;

  const module = await import("@clerk/clerk-js");
  const instance = new module.Clerk(config.clerkPublishableKey);
  await instance.load();
  instance.addListener(async ({ session }) => {
    if (!session || getConfig().hasServerSession) return;
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
    const res = await fetch("/auth/session", {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token }),
    });
    if (!res.ok) return false;
    await refreshPanel();
    return true;
  } finally {
    isSyncing = false;
  }
}

async function refreshPanel(): Promise<void> {
  const panel = $("auth-panel");
  if (!panel) return;
  const res = await fetch("/partials/auth-panel", { credentials: "include", headers: { Accept: "text/html" } });
  if (res.ok) panel.outerHTML = await res.text();
}

async function signIn(): Promise<void> {
  const instance = await ensureClerk();
  if (!instance) return;
  const synced = await syncServerSession();
  if (synced) { redirectToApp(); return; }
  instance.openSignIn();
}

async function signUp(): Promise<void> {
  const instance = await ensureClerk();
  if (!instance) return;
  instance.openSignUp();
}

async function signOut(): Promise<void> {
  const instance = await ensureClerk();
  await fetch("/auth/logout", { method: "POST", credentials: "include" });
  if (instance) await instance.signOut();
  window.location.href = "/";
}

// ─── Provider ───

function setProviderStatus(msg: string, tone: "muted" | "error" | "success" = "muted"): void {
  const el = $("provider-connect-status");
  if (!el) return;
  el.textContent = msg;
  el.classList.remove("is-error", "is-success");
  if (tone === "error") el.classList.add("is-error");
  if (tone === "success") el.classList.add("is-success");
}

async function refreshProviderPanel(): Promise<void> {
  const panel = $("provider-panel");
  if (!panel) return;
  const res = await fetch("/settings/provider", { credentials: "include", headers: { Accept: "text/html" } });
  if (res.ok) panel.outerHTML = await res.text();
}

async function connectCodex(): Promise<void> {
  setProviderStatus("Starting Codex device flow...");
  const res = await fetch("/settings/provider/codex/start", { method: "POST", credentials: "include" });
  if (!res.ok) { setProviderStatus("Failed to start Codex OAuth.", "error"); return; }
  const payload = (await res.json()) as CodexStartResponse;
  setProviderStatus(`Enter code ${payload.userCode} in the OpenAI window.`);
  window.open(payload.verifyUrl, "_blank", "noopener,noreferrer");
  startCodexPolling(payload.intervalSeconds);
}

function startCodexPolling(interval: number): void {
  if (authPollTimer !== null) window.clearTimeout(authPollTimer);
  const tick = async (): Promise<void> => {
    const res = await fetch("/settings/provider/codex/poll", { method: "POST", credentials: "include" });
    if (!res.ok) { setProviderStatus("Failed to verify.", "error"); return; }
    const p = (await res.json()) as CodexPollResponse;
    if (p.status === "connected") { setProviderStatus(p.message ?? "Connected.", "success"); await refreshProviderPanel(); return; }
    if (p.status === "pending") { setProviderStatus(p.message ?? "Waiting..."); authPollTimer = window.setTimeout(() => void tick(), interval * 1000); return; }
    setProviderStatus("No pending session.");
  };
  authPollTimer = window.setTimeout(() => void tick(), interval * 1000);
}

async function disconnectProvider(): Promise<void> {
  const res = await fetch("/settings/provider/logout", { method: "POST", credentials: "include" });
  if (!res.ok) { setProviderStatus("Failed to disconnect.", "error"); return; }
  const panel = $("provider-panel");
  if (panel) panel.outerHTML = await res.text();
}

// ─── Storm state ───

function getRun(id: string | null): StormRun | null {
  if (!id) return null;
  return state.runs.find((r) => r.id === id) ?? null;
}

function assignPosition(run: StormRun, index: number, sourceIds?: string[]): void {
  if (state.positions.has(run.id)) return;
  if (sourceIds?.length) {
    const pts = sourceIds.map((id) => state.positions.get(id)).filter((p): p is Point => Boolean(p));
    if (pts.length > 0) {
      const c = pts.reduce((a, p) => ({ x: a.x + p.x, y: a.y + p.y }), { x: 0, y: 0 });
      const off = sourceIds.length > 1 ? 170 : 250;
      state.positions.set(run.id, { x: c.x / pts.length + off, y: c.y / pts.length + 110 });
      return;
    }
  }
  const col = index % 4;
  const row = Math.floor(index / 4);
  state.positions.set(run.id, { x: 220 + col * 360 + (row % 2) * 70, y: 240 + row * 290 + (col % 2) * 28 });
}

function setStatus(msg: string): void {
  const el = $("storm-status");
  if (el) el.textContent = msg;
}

function setGenerating(value: boolean): void {
  const btn = $("storm-submit") as HTMLButtonElement | null;
  if (btn) btn.disabled = value;
}

// ─── URL sync ───

function syncUrl(replace = false): void {
  const url = new URL(window.location.href);
  url.searchParams.delete("inspect");
  url.searchParams.delete("focus");
  if (state.focusedRunId) url.searchParams.set("focus", state.focusedRunId);
  else if (state.activeRunId) url.searchParams.set("inspect", state.activeRunId);
  const next = `${url.pathname}${url.search}${url.hash}`;
  if (next === `${window.location.pathname}${window.location.search}${window.location.hash}`) return;
  if (replace) window.history.replaceState({}, "", next);
  else window.history.pushState({}, "", next);
}

function applyUrlState(): void {
  const url = new URL(window.location.href);
  const focusId = url.searchParams.get("focus");
  const inspectId = url.searchParams.get("inspect");
  if (focusId && getRun(focusId)) { state.focusedRunId = focusId; state.activeRunId = focusId; return; }
  if (inspectId && getRun(inspectId)) { state.focusedRunId = null; state.activeRunId = inspectId; return; }
  state.focusedRunId = null;
  if (state.activeRunId && !getRun(state.activeRunId)) state.activeRunId = null;
}

// ─── Draft context ───

function buildForkPrompt(run: StormRun): string {
  return [`Fork the design language from "${run.title}".`, `Keep the strongest ideas: ${run.summary}`, `Original seed: ${run.prompt}`, "Push it into a clearly distinct next branch."].join("\n\n");
}

function buildCombinePrompt(a: StormRun, b: StormRun): string {
  return [`Combine these two design directions.`, `A: "${a.title}" — ${a.summary}`, `B: "${b.title}" — ${b.summary}`, `Seed A: ${a.prompt}`, `Seed B: ${b.prompt}`, "Take structural logic from one and atmospheric tone from the other."].join("\n\n");
}

function showDraftContext(ctx: DraftContext): void {
  state.draftContext = ctx;
  state.pendingLineage = ctx;
  const ta = $("storm-prompt") as HTMLTextAreaElement | null;
  if (!ta) return;
  if (ctx.mode === "fork") {
    const src = getRun(ctx.sourceIds[0]);
    if (src) { ta.value = buildForkPrompt(src); setStatus(`Forking from ${src.title}.`); }
  } else {
    const a = getRun(ctx.sourceIds[0]);
    const b = getRun(ctx.sourceIds[1]);
    if (a && b) { ta.value = buildCombinePrompt(a, b); setStatus(`Combining ${a.title} + ${b.title}.`); }
  }
  ta.focus();
  renderDraftContext();
}

function clearDraftContext(opts?: { keepStatus?: boolean }): void {
  state.draftContext = null;
  state.pendingLineage = null;
  state.combineSourceId = null;
  renderDraftContext();
  renderRuns();
  if (!opts?.keepStatus) setStatus("Seed a direction or select a card to branch from it.");
}

function renderDraftContext(): void {
  const container = $("storm-draft-context");
  const clearBtn = $("storm-clear-context") as HTMLButtonElement | null;
  if (!container || !clearBtn) return;
  const ctx = state.draftContext;
  if (!ctx) { container.hidden = true; container.innerHTML = ""; clearBtn.disabled = true; return; }
  clearBtn.disabled = false;
  container.hidden = false;
  if (ctx.mode === "fork") {
    const src = getRun(ctx.sourceIds[0]);
    container.innerHTML = `<span class="draft-pill">Fork</span><div class="draft-copy"><strong>${escapeHtml(ctx.label)}</strong><p>${escapeHtml(src?.summary ?? "Branch from selected artifact.")}</p></div>`;
  } else {
    const a = getRun(ctx.sourceIds[0]);
    const b = getRun(ctx.sourceIds[1]);
    container.innerHTML = `<span class="draft-pill">Combine</span><div class="draft-copy"><strong>${escapeHtml(ctx.label)}</strong><p>${escapeHtml(a?.title ?? "A")} + ${escapeHtml(b?.title ?? "B")}</p></div>`;
  }
}

// ─── Run actions ───

function setActiveRun(id: string | null, opts?: { sync?: boolean }): void {
  state.activeRunId = id;
  if (!id) state.focusedRunId = null;
  renderRuns();
  renderInspector();
  renderFocus();
  if (opts?.sync) syncUrl(false);
}

function openFullscreen(id: string): void {
  state.activeRunId = id;
  state.focusedRunId = id;
  renderRuns();
  renderInspector();
  renderFocus();
  syncUrl(false);
}

function closeFullscreen(): void {
  state.focusedRunId = null;
  renderFocus();
  syncUrl(false);
}

function beginCombine(runId: string): void {
  if (state.combineSourceId === runId) { state.combineSourceId = null; setStatus("Combine cancelled."); renderRuns(); return; }
  state.combineSourceId = runId;
  const r = getRun(runId);
  setStatus(`Combine: select another artifact to hybridize with ${r?.title ?? "this run"}.`);
  renderRuns();
}

function maybeComposeCombine(targetId: string): boolean {
  const srcId = state.combineSourceId;
  if (!srcId || srcId === targetId) return false;
  const src = getRun(srcId);
  const tgt = getRun(targetId);
  if (!src || !tgt) return false;
  state.combineSourceId = null;
  showDraftContext({ mode: "combine", sourceIds: [src.id, tgt.id], label: `Hybridizing ${src.title} and ${tgt.title}` });
  setActiveRun(tgt.id, { sync: true });
  return true;
}

function handleRunAction(runId: string, action: string): void {
  const run = getRun(runId);
  if (!run) return;
  if (action === "inspect") { setActiveRun(run.id, { sync: true }); return; }
  if (action === "fullscreen") { openFullscreen(run.id); return; }
  if (action === "fork") { showDraftContext({ mode: "fork", sourceIds: [run.id], label: `Forking ${run.title}` }); setActiveRun(run.id, { sync: true }); return; }
  if (action === "combine") beginCombine(run.id);
}

// ─── Rendering ───

function updateBoardTransform(): void {
  const board = $("storm-board");
  if (!board) return;
  board.style.transform = `translate(${state.pan.x}px, ${state.pan.y}px) scale(${state.scale})`;
  const zoom = $("storm-zoom-readout");
  if (zoom) zoom.textContent = `${Math.round(state.scale * 100)}%`;
}

function renderConnections(): void {
  const svg = $("storm-lines") as SVGElement | null;
  if (!svg) return;
  svg.setAttribute("viewBox", `0 0 ${BOARD_WIDTH} ${BOARD_HEIGHT}`);
  svg.innerHTML = "";
  state.lineage.forEach((parents, runId) => {
    const child = state.positions.get(runId);
    if (!child) return;
    parents.forEach((pid) => {
      const parent = state.positions.get(pid);
      if (!parent) return;
      const sx = parent.x + CARD_WIDTH * 0.5, sy = parent.y + CARD_HEIGHT * 0.56;
      const ex = child.x + CARD_WIDTH * 0.5, ey = child.y + 20;
      const cy = sy + (ey - sy) * 0.45;
      const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
      path.setAttribute("d", `M ${sx} ${sy} C ${sx} ${cy}, ${ex} ${cy}, ${ex} ${ey}`);
      path.setAttribute("class", "storm-line");
      svg.appendChild(path);
    });
  });
}

function renderRuns(): void {
  const container = $("storm-runs");
  const empty = $("storm-empty-state");
  if (!container || !empty) return;
  if (state.runs.length === 0) { container.innerHTML = ""; empty.hidden = false; renderConnections(); return; }
  empty.hidden = true;
  container.innerHTML = "";
  state.runs.forEach((run, i) => {
    const lineage = state.lineage.get(run.id);
    assignPosition(run, i, lineage);
    const pt = state.positions.get(run.id) ?? { x: 240, y: 240 };
    const card = document.createElement("article");
    card.className = "storm-node";
    if (run.id === state.activeRunId) card.classList.add("is-active");
    if (run.id === state.combineSourceId) card.classList.add("is-combine-source");
    card.dataset.runId = run.id;
    card.style.transform = `translate(${pt.x}px, ${pt.y}px)`;
    card.innerHTML = `
      <div class="storm-node-shell">
        <div class="storm-node-meta">
          <span class="meta-note">${new Date(run.createdAt).toLocaleDateString()}</span>
          <span class="pill ${run.submitted ? "pill-accent" : "pill-muted"}">${run.submitted ? "submitted" : "draft"}</span>
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
      </div>`;
    container.appendChild(card);
  });
  renderConnections();
}

function renderInspector(): void {
  const panel = $("storm-inspector");
  const title = $("active-run-title");
  const summary = $("storm-summary");
  const prompt = $("inspector-prompt");
  const notes = $("storm-notes");
  const created = $("inspector-created");
  const pill = $("inspector-status-pill");
  const chips = $("inspector-chips");
  const seedCard = $("inspector-seed");
  const notesCard = $("inspector-notes");
  const iframe = $("storm-preview") as HTMLIFrameElement | null;
  const fork = $("inspector-fork") as HTMLButtonElement | null;
  const combine = $("inspector-combine") as HTMLButtonElement | null;
  const fs = $("inspector-fullscreen") as HTMLButtonElement | null;
  const run = getRun(state.activeRunId);
  if (!panel || !title || !summary || !prompt || !notes || !created || !pill || !chips || !seedCard || !notesCard || !iframe || !fork || !combine || !fs) return;
  if (!run) {
    panel.classList.add("is-empty");
    title.textContent = "Select an artifact";
    summary.textContent = "";
    chips.hidden = true;
    seedCard.hidden = true;
    notesCard.hidden = true;
    iframe.removeAttribute("src");
    fork.disabled = combine.disabled = fs.disabled = true;
    return;
  }
  panel.classList.remove("is-empty");
  title.textContent = run.title;
  summary.textContent = run.summary;
  chips.hidden = false;
  pill.textContent = run.submitted ? "Submitted" : "Draft";
  pill.className = `pill ${run.submitted ? "pill-accent" : "pill-muted"}`;
  created.textContent = new Date(run.createdAt).toLocaleString();
  seedCard.hidden = false;
  prompt.textContent = run.prompt;
  notesCard.hidden = !run.assistantSummary;
  notes.textContent = run.assistantSummary || "";
  iframe.src = run.previewUrl;
  fork.disabled = combine.disabled = fs.disabled = false;
}

function renderFocus(): void {
  const overlay = $("storm-focus");
  const frame = $("storm-focus-preview") as HTMLIFrameElement | null;
  const title = $("storm-focus-title");
  const run = getRun(state.focusedRunId);
  if (!overlay || !frame || !title) return;
  if (!run) { overlay.hidden = true; overlay.setAttribute("aria-hidden", "true"); frame.removeAttribute("src"); return; }
  overlay.hidden = false;
  overlay.setAttribute("aria-hidden", "false");
  frame.src = run.previewUrl;
  title.textContent = run.title;
}

// ─── Data fetching ───

async function loadStorms(): Promise<void> {
  const res = await fetch("/api/storms", { credentials: "include" });
  if (!res.ok) return;
  const runs = (await res.json()) as StormRun[];
  state.runs = runs.sort((a, b) => Date.parse(a.createdAt) - Date.parse(b.createdAt));
  applyUrlState();
  renderRuns();
  renderInspector();
  renderFocus();
}

async function submitStorm(e: Event): Promise<void> {
  e.preventDefault();
  const ta = $("storm-prompt") as HTMLTextAreaElement | null;
  const btn = $("storm-submit") as HTMLButtonElement | null;
  if (!ta || !btn) return;
  const prompt = ta.value.trim();
  if (!prompt) { setStatus("Seed prompt required."); return; }
  setGenerating(true);
  setStatus("Generating storm...");
  try {
    const res = await fetch("/api/storms", {
      method: "POST", credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ prompt }),
    });
    if (!res.ok) { setStatus((await res.text()) || "Generation failed."); return; }
    const payload = (await res.json()) as { run: StormRun };
    const pending = state.pendingLineage;
    if (pending) { state.lineage.set(payload.run.id, pending.sourceIds); assignPosition(payload.run, state.runs.length, pending.sourceIds); }
    clearDraftContext({ keepStatus: true });
    ta.value = "";
    setStatus("Storm generated.");
    await loadStorms();
    setActiveRun(payload.run.id, { sync: true });
  } finally {
    setGenerating(false);
  }
}

// ─── Event bindings ───

function bindCanvasInteractions(): void {
  const canvas = $("storm-canvas");
  if (!canvas) return;

  canvas.addEventListener("wheel", (e) => {
    e.preventDefault();
    state.scale = Number(Math.max(0.5, Math.min(1.5, state.scale - e.deltaY * 0.001)).toFixed(2));
    updateBoardTransform();
  }, { passive: false });

  canvas.addEventListener("pointerdown", (e) => {
    if ((e.target as HTMLElement).closest(".storm-node")) return;
    state.pointerState = { mode: "pan", pointerId: e.pointerId, startClient: { x: e.clientX, y: e.clientY }, startPan: { ...state.pan } };
    canvas.setPointerCapture(e.pointerId);
  });

  canvas.addEventListener("pointermove", (e) => {
    if (!state.pointerState || state.pointerState.mode !== "pan" || state.pointerState.pointerId !== e.pointerId) return;
    state.pan = { x: state.pointerState.startPan.x + (e.clientX - state.pointerState.startClient.x), y: state.pointerState.startPan.y + (e.clientY - state.pointerState.startClient.y) };
    updateBoardTransform();
  });

  canvas.addEventListener("pointerup", (e) => {
    if (state.pointerState?.pointerId === e.pointerId) { state.pointerState = null; canvas.releasePointerCapture(e.pointerId); }
  });
}

function bindNodeInteractions(): void {
  const container = $("storm-runs");
  if (!container) return;

  container.addEventListener("click", (e) => {
    const target = e.target as HTMLElement;
    const action = target.closest<HTMLElement>("[data-run-action]")?.dataset.runAction;
    const node = target.closest<HTMLElement>(".storm-node");
    const runId = node?.dataset.runId;
    if (!runId) return;
    if (action) { handleRunAction(runId, action); return; }
    if (state.pointerState?.mode === "drag" && state.pointerState.runId === runId && state.pointerState.moved) return;
    if (maybeComposeCombine(runId)) return;
    setActiveRun(runId, { sync: true });
  });

  container.addEventListener("dblclick", (e) => {
    const node = (e.target as HTMLElement).closest<HTMLElement>(".storm-node");
    if (node?.dataset.runId) openFullscreen(node.dataset.runId);
  });

  container.addEventListener("pointerdown", (e) => {
    const target = e.target as HTMLElement;
    if (target.closest("[data-run-action]")) return;
    const node = target.closest<HTMLElement>(".storm-node");
    const runId = node?.dataset.runId;
    if (!node || !runId) return;
    const pt = state.positions.get(runId);
    if (!pt) return;
    state.pointerState = { mode: "drag", pointerId: e.pointerId, runId, startClient: { x: e.clientX, y: e.clientY }, startPos: { ...pt }, moved: false };
    node.setPointerCapture(e.pointerId);
  });

  container.addEventListener("pointermove", (e) => {
    if (!state.pointerState || state.pointerState.mode !== "drag" || state.pointerState.pointerId !== e.pointerId) return;
    const dx = (e.clientX - state.pointerState.startClient.x) / state.scale;
    const dy = (e.clientY - state.pointerState.startClient.y) / state.scale;
    if (Math.abs(dx) > 4 || Math.abs(dy) > 4) state.pointerState.moved = true;
    const next = { x: state.pointerState.startPos.x + dx, y: state.pointerState.startPos.y + dy };
    state.positions.set(state.pointerState.runId, next);
    const node = container.querySelector<HTMLElement>(`.storm-node[data-run-id="${state.pointerState.runId}"]`);
    if (node) node.style.transform = `translate(${next.x}px, ${next.y}px)`;
    renderConnections();
  });

  container.addEventListener("pointerup", (e) => {
    if (!state.pointerState || state.pointerState.mode !== "drag" || state.pointerState.pointerId !== e.pointerId) return;
    (e.target as HTMLElement).closest<HTMLElement>(".storm-node")?.releasePointerCapture(e.pointerId);
    state.pointerState = null;
  });
}

function bindAppChrome(): void {
  // Form
  $("storm-form")?.addEventListener("submit", (e) => void submitStorm(e));
  $("storm-clear-context")?.addEventListener("click", () => clearDraftContext());
  $("storm-reset-view")?.addEventListener("click", () => { state.pan = { ...INITIAL_PAN }; state.scale = 1; updateBoardTransform(); });

  // Fullscreen overlay
  $("storm-focus")?.addEventListener("click", (e) => {
    if ((e.target as HTMLElement).closest("[data-action='close-focus']")) closeFullscreen();
  });
  $("storm-focus-back")?.addEventListener("click", () => closeFullscreen());

  // Inspector actions
  $("inspector-fork")?.addEventListener("click", () => { if (state.activeRunId) handleRunAction(state.activeRunId, "fork"); });
  $("inspector-combine")?.addEventListener("click", () => { if (state.activeRunId) handleRunAction(state.activeRunId, "combine"); });
  $("inspector-fullscreen")?.addEventListener("click", () => { if (state.activeRunId) handleRunAction(state.activeRunId, "fullscreen"); });

  // Keyboard — fullscreen escape (popovers handled by Datastar)
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && state.focusedRunId) closeFullscreen();
  });

  // History
  window.addEventListener("popstate", () => { applyUrlState(); renderRuns(); renderInspector(); renderFocus(); });
}

function setAvatarInitials(): void {
  const el = $("avatar-fallback");
  if (!el) return;
  const name = el.getAttribute("data-name") ?? "DS";
  el.textContent = name.split(/\s+/).filter(Boolean).slice(0, 2).map((p) => p[0]?.toUpperCase() ?? "").join("");
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

// ─── Bootstrap ───

async function bootstrap(): Promise<void> {
  const instance = await ensureClerk();
  if (!instance) return;
  const res = await fetch("/auth/me", { credentials: "include" });
  const auth = res.ok ? ((await res.json()) as { authenticated: boolean }) : { authenticated: false };
  if (!auth.authenticated && instance.session) {
    const synced = await syncServerSession();
    if (synced && getConfig().currentPath === "/") redirectToApp();
  }
  bindStormApp();
}

window.designstormAuth = { signIn, signUp, signOut, refreshPanel };
window.designstormSettings = { connectCodex, disconnectProvider };

void bootstrap();
