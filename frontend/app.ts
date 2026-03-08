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

type RadialItem = {
  id: string;
  angle: number;
  label: string;
  icon: string;
  variant?: "default" | "primary" | "danger";
  disabled?: boolean;
  action: () => void;
};

type RadialMenuState = {
  open: boolean;
  position: Point;
  selectedIndex: number | null;
};

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
  radialMenu: RadialMenuState;
  lastCursor: Point;
  awaitingGeneratedRun: boolean;
};

declare global {
  interface Window {
    Clerk?: new (publishableKey: string) => ClerkLike;
    designstormAuth: {
      signIn(): Promise<void>;
      signUp(): Promise<void>;
      signOut(): Promise<void>;
    };
    designstormSettings: {
      connectCodex(): Promise<void>;
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
  radialMenu: { open: false, position: { x: 0, y: 0 }, selectedIndex: null },
  lastCursor: { x: 0, y: 0 },
  awaitingGeneratedRun: false,
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

function setBoundValue(id: string, value: string): void {
  const input = $(id) as HTMLInputElement | HTMLTextAreaElement | null;
  if (!input) return;
  input.value = value;
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.dispatchEvent(new Event("change", { bubbles: true }));
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
    return true;
  } finally {
    isSyncing = false;
  }
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
    if (p.status === "connected") {
      setProviderStatus(p.message ?? "Connected.", "success");
      ($("provider-refresh") as HTMLButtonElement | null)?.click();
      return;
    }
    if (p.status === "pending") { setProviderStatus(p.message ?? "Waiting..."); authPollTimer = window.setTimeout(() => void tick(), interval * 1000); return; }
    setProviderStatus("No pending session.");
  };
  authPollTimer = window.setTimeout(() => void tick(), interval * 1000);
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
  setBoundValue("storm-status-signal", msg);
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
  setBoundValue("storm-draft-mode", ctx.mode);
  setBoundValue("storm-source-ids", ctx.sourceIds.join(","));
  if (ctx.mode === "fork") {
    const src = getRun(ctx.sourceIds[0]);
    if (src) {
      setBoundValue("storm-prompt", buildForkPrompt(src));
      setStatus(`Forking from ${src.title}.`);
    }
  } else {
    const a = getRun(ctx.sourceIds[0]);
    const b = getRun(ctx.sourceIds[1]);
    if (a && b) {
      setBoundValue("storm-prompt", buildCombinePrompt(a, b));
      setStatus(`Combining ${a.title} + ${b.title}.`);
    }
  }
  const ta = $("storm-prompt") as HTMLTextAreaElement | null;
  ta?.focus();
  renderDraftContext();
}

function clearDraftContext(opts?: { keepStatus?: boolean }): void {
  state.draftContext = null;
  state.pendingLineage = null;
  state.combineSourceId = null;
  setBoundValue("storm-draft-mode", "");
  setBoundValue("storm-source-ids", "");
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

function hydrateBoardFromDom(): void {
  const container = $("storm-runs");
  const previousCount = state.runs.length;
  if (!container) {
    state.runs = [];
    state.lineage = new Map();
    renderConnections();
    return;
  }

  const nodes = Array.from(container.querySelectorAll<HTMLElement>(".storm-node[data-run-id]"));
  const lineage = new Map<string, string[]>();
  const runs = nodes.map((node) => {
    const run: StormRun = {
      id: node.dataset.runId ?? "",
      prompt: node.dataset.runPrompt ?? "",
      title: node.dataset.runTitle ?? "Storm Artifact",
      summary: node.dataset.runSummary ?? "",
      assistantSummary: node.dataset.runAssistantSummary ?? "",
      previewUrl: node.dataset.runPreviewUrl ?? "",
      submitted: node.dataset.runSubmitted === "true",
      createdAt: node.dataset.runCreatedAt ?? new Date().toISOString(),
    };
    lineage.set(
      run.id,
      (node.dataset.runParentIds ?? "")
        .split(",")
        .map((value) => value.trim())
        .filter(Boolean),
    );
    return run;
  });

  runs.sort((a, b) => Date.parse(a.createdAt) - Date.parse(b.createdAt));
  state.runs = runs;
  state.lineage = lineage;

  if (state.activeRunId && !getRun(state.activeRunId)) state.activeRunId = null;
  if (state.focusedRunId && !getRun(state.focusedRunId)) state.focusedRunId = null;

  if (state.awaitingGeneratedRun && runs.length > previousCount) {
    const latest = runs.at(-1) ?? null;
    state.awaitingGeneratedRun = false;
    clearDraftContext({ keepStatus: true });
    hideComposer({ preserveDraft: true });
    if (latest) {
      setStatus("Storm generated.");
      setActiveRun(latest.id, { sync: true });
    }
  }

  applyUrlState();
  renderRuns();
  renderInspector();
  renderFocus();
}

function renderRuns(): void {
  const container = $("storm-runs");
  if (!container) return;
  if (state.runs.length === 0) {
    renderConnections();
    return;
  }

  const nodesById = new Map(
    Array.from(container.querySelectorAll<HTMLElement>(".storm-node[data-run-id]"))
      .map((node) => [node.dataset.runId ?? "", node]),
  );

  state.runs.forEach((run, i) => {
    const lineage = state.lineage.get(run.id);
    assignPosition(run, i, lineage);
    const pt = state.positions.get(run.id) ?? { x: 240, y: 240 };
    const card = nodesById.get(run.id);
    if (!card) return;
    card.classList.toggle("is-active", run.id === state.activeRunId);
    card.classList.toggle("is-combine-source", run.id === state.combineSourceId);
    card.style.transform = `translate(${pt.x}px, ${pt.y}px)`;
    const combineButton = card.querySelector<HTMLElement>("[data-run-action='combine']");
    if (combineButton) combineButton.textContent = run.id === state.combineSourceId ? "Cancel" : "Combine";
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
    if (e.button === 2) return;
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
  const generateButton = $("storm-submit");
  generateButton?.addEventListener("click", (e) => {
    const ta = $("storm-prompt") as HTMLTextAreaElement | null;
    const prompt = ta?.value.trim() ?? "";
    if (!prompt) {
      e.preventDefault();
      e.stopImmediatePropagation();
      state.awaitingGeneratedRun = false;
      setStatus("Seed prompt required.");
      return;
    }
    state.awaitingGeneratedRun = true;
    setStatus("Generating storm...");
  });
  $("storm-clear-context")?.addEventListener("click", () => clearDraftContext());
  $("storm-composer-backdrop")?.addEventListener("click", () => hideComposer());

  // Fullscreen overlay
  $("storm-focus")?.addEventListener("click", (e) => {
    if ((e.target as HTMLElement).closest("[data-action='close-focus']")) closeFullscreen();
  });
  $("storm-focus-back")?.addEventListener("click", () => closeFullscreen());

  // Inspector actions
  $("inspector-fork")?.addEventListener("click", () => { if (state.activeRunId) handleRunAction(state.activeRunId, "fork"); });
  $("inspector-combine")?.addEventListener("click", () => { if (state.activeRunId) handleRunAction(state.activeRunId, "combine"); });
  $("inspector-fullscreen")?.addEventListener("click", () => { if (state.activeRunId) handleRunAction(state.activeRunId, "fullscreen"); });

  // Keyboard — escape priority: radial > composer > fullscreen (popovers handled by Datastar)
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      if (state.radialMenu.open) { closeRadialMenu(); e.preventDefault(); return; }
      const form = $("storm-form");
      if (form && !form.hidden) { hideComposer(); e.preventDefault(); return; }
      if (state.focusedRunId) closeFullscreen();
    }
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

// ─── Composer show/hide ───

function showComposer(): void {
  const form = $("storm-form");
  const backdrop = $("storm-composer-backdrop");
  if (!form) return;

  // Position near the radial menu invocation point
  const pos = state.radialMenu.position;
  const formWidth = 340;
  const formHeight = 220;
  const pad = 16;
  const vw = window.innerWidth;
  const vh = window.innerHeight;

  // Place below-right of cursor by default, clamp to viewport
  let x = pos.x - formWidth / 2;
  let y = pos.y + 20;
  if (x + formWidth + pad > vw) x = vw - formWidth - pad;
  if (x < pad) x = pad;
  if (y + formHeight + pad > vh) y = pos.y - formHeight - 20;
  if (y < pad) y = pad;

  form.style.left = `${x}px`;
  form.style.top = `${y}px`;
  form.style.removeProperty("bottom");
  form.style.removeProperty("right");
  form.hidden = false;
  if (backdrop) backdrop.hidden = false;

  const ta = $("storm-prompt") as HTMLTextAreaElement | null;
  ta?.focus();
}

function hideComposer(opts?: { preserveDraft?: boolean }): void {
  const form = $("storm-form");
  const backdrop = $("storm-composer-backdrop");
  if (!form) return;
  form.hidden = true;
  if (backdrop) backdrop.hidden = true;
  if (!opts?.preserveDraft) clearDraftContext({ keepStatus: true });
}

// ─── Radial menu geometry ───

const RADIAL_RADIUS = 100;
const RADIAL_DEAD_ZONE = 15;
const RADIAL_MAX_DIST = 200;

function findClosestItem(offsetX: number, offsetY: number, items: RadialItem[], deadZone: number): { index: number; angle: number } | null {
  const distance = Math.sqrt(offsetX * offsetX + offsetY * offsetY);
  if (distance < deadZone) return null;
  let angle = Math.atan2(-offsetY, offsetX) * (180 / Math.PI);
  angle = (90 - angle + 360) % 360;
  let closestIndex = 0;
  let closestDiff = 360;
  for (let i = 0; i < items.length; i++) {
    let diff = Math.abs(items[i].angle - angle);
    if (diff > 180) diff = 360 - diff;
    if (diff < closestDiff) { closestDiff = diff; closestIndex = i; }
  }
  return { index: closestIndex, angle };
}

function renderSlicePath(cx: number, cy: number, startAngle: number, endAngle: number, innerR: number, outerR: number): string {
  const startRad = ((startAngle - 90) * Math.PI) / 180;
  const endRad = ((endAngle - 90) * Math.PI) / 180;
  const x1 = cx + Math.cos(startRad) * innerR, y1 = cy + Math.sin(startRad) * innerR;
  const x2 = cx + Math.cos(startRad) * outerR, y2 = cy + Math.sin(startRad) * outerR;
  const x3 = cx + Math.cos(endRad) * outerR, y3 = cy + Math.sin(endRad) * outerR;
  const x4 = cx + Math.cos(endRad) * innerR, y4 = cy + Math.sin(endRad) * innerR;
  return `M ${x1} ${y1} L ${x2} ${y2} A ${outerR} ${outerR} 0 0 1 ${x3} ${y3} L ${x4} ${y4} A ${innerR} ${innerR} 0 0 0 ${x1} ${y1} Z`;
}

// ─── Radial menu items ───

function getRadialItems(): RadialItem[] {
  if (!state.activeRunId) {
    return [{ id: "generate", angle: 0, label: "Generate", icon: "✦", variant: "primary", action: showComposer }];
  }
  return [
    { id: "fork", angle: 0, label: "Fork", icon: "⑂", action: () => { if (state.activeRunId) { handleRunAction(state.activeRunId, "fork"); showComposer(); } } },
    { id: "fullscreen", angle: 90, label: "Fullscreen", icon: "⛶", action: () => { if (state.activeRunId) openFullscreen(state.activeRunId); } },
    { id: "combine", angle: 180, label: "Combine", icon: "⊕", action: () => { if (state.activeRunId) beginCombine(state.activeRunId); } },
    { id: "generate", angle: 270, label: "Generate", icon: "✦", variant: "primary", action: showComposer },
  ];
}

// ─── Radial menu rendering ───

function renderRadialMenu(items: RadialItem[]): void {
  const center = $("radial-center");
  const itemsContainer = $("radial-items");
  const svg = $("radial-svg") as unknown as SVGSVGElement | null;
  if (!center || !itemsContainer || !svg) return;

  const pos = state.radialMenu.position;
  center.style.left = `${pos.x}px`;
  center.style.top = `${pos.y}px`;

  // Clear and rebuild items
  itemsContainer.innerHTML = "";
  items.forEach((item, i) => {
    const rads = ((item.angle - 90) * Math.PI) / 180;
    const x = pos.x + Math.cos(rads) * RADIAL_RADIUS;
    const y = pos.y + Math.sin(rads) * RADIAL_RADIUS;
    const div = document.createElement("div");
    div.className = "radial-item";
    if (state.radialMenu.selectedIndex === i) div.classList.add("is-selected");
    if (item.disabled) div.classList.add("is-disabled");
    if (item.variant && item.variant !== "default") div.dataset.variant = item.variant;
    div.style.left = `${x}px`;
    div.style.top = `${y}px`;
    const iconSpan = document.createElement("span");
    iconSpan.textContent = item.icon;
    const labelSpan = document.createElement("span");
    labelSpan.textContent = item.label;
    div.appendChild(iconSpan);
    div.appendChild(labelSpan);
    itemsContainer.appendChild(div);
  });

  // Update SVG
  while (svg.firstChild) svg.removeChild(svg.firstChild);
  const sliceAngle = 360 / items.length;

  // Divider lines
  if (items.length > 1) {
    items.forEach((item) => {
      const dividerAngle = item.angle - sliceAngle / 2;
      const rads = ((dividerAngle - 90) * Math.PI) / 180;
      const line = document.createElementNS("http://www.w3.org/2000/svg", "line");
      line.setAttribute("x1", String(pos.x + Math.cos(rads) * 20));
      line.setAttribute("y1", String(pos.y + Math.sin(rads) * 20));
      line.setAttribute("x2", String(pos.x + Math.cos(rads) * (RADIAL_RADIUS - 10)));
      line.setAttribute("y2", String(pos.y + Math.sin(rads) * (RADIAL_RADIUS - 10)));
      line.setAttribute("stroke", "rgba(255, 255, 255, 0.08)");
      line.setAttribute("stroke-width", "1");
      svg.appendChild(line);
    });
  }

  // Selected slice highlight + direction indicator
  if (state.radialMenu.selectedIndex !== null) {
    const item = items[state.radialMenu.selectedIndex];
    if (item && !item.disabled) {
      const startAngle = item.angle - sliceAngle / 2;
      const endAngle = item.angle + sliceAngle / 2;
      const pathD = renderSlicePath(pos.x, pos.y, startAngle, endAngle, 18, RADIAL_RADIUS - 5);
      const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
      path.setAttribute("d", pathD);
      path.setAttribute("fill", "rgba(91, 156, 184, 0.1)");
      path.setAttribute("stroke", "rgba(91, 156, 184, 0.4)");
      path.setAttribute("stroke-width", "1");
      svg.appendChild(path);

      const rads = ((item.angle - 90) * Math.PI) / 180;
      const len = RADIAL_RADIUS - 15;
      const line = document.createElementNS("http://www.w3.org/2000/svg", "line");
      line.setAttribute("x1", String(pos.x));
      line.setAttribute("y1", String(pos.y));
      line.setAttribute("x2", String(pos.x + Math.cos(rads) * len));
      line.setAttribute("y2", String(pos.y + Math.sin(rads) * len));
      line.setAttribute("stroke", "rgba(91, 156, 184, 0.6)");
      line.setAttribute("stroke-width", "2");
      line.setAttribute("stroke-linecap", "square");
      svg.appendChild(line);
    }
  }
}

// ─── Radial menu open/close ───

let radialCleanup: (() => void) | null = null;

function openRadialMenu(x: number, y: number): void {
  state.radialMenu = { open: true, position: { x, y }, selectedIndex: null };
  const menu = $("radial-menu");
  if (menu) { menu.hidden = false; menu.setAttribute("aria-hidden", "false"); }
  const items = getRadialItems();
  renderRadialMenu(items);
  bindRadialMenuListeners(items);
}

function closeRadialMenu(): void {
  state.radialMenu.open = false;
  state.radialMenu.selectedIndex = null;
  const menu = $("radial-menu");
  if (menu) { menu.hidden = true; menu.setAttribute("aria-hidden", "true"); }
  if (radialCleanup) { radialCleanup(); radialCleanup = null; }
}

function executeRadialSelected(items: RadialItem[]): boolean {
  const idx = state.radialMenu.selectedIndex;
  if (idx !== null) {
    const item = items[idx];
    if (item && !item.disabled) { item.action(); return true; }
  }
  return false;
}

function bindRadialMenuListeners(items: RadialItem[]): void {
  if (radialCleanup) { radialCleanup(); radialCleanup = null; }

  const handleMouseMove = (e: MouseEvent) => {
    const offsetX = e.clientX - state.radialMenu.position.x;
    const offsetY = e.clientY - state.radialMenu.position.y;
    const distance = Math.hypot(offsetX, offsetY);
    if (distance > RADIAL_MAX_DIST) { closeRadialMenu(); return; }
    const result = findClosestItem(offsetX, offsetY, items, RADIAL_DEAD_ZONE);
    state.radialMenu.selectedIndex = result?.index ?? null;
    renderRadialMenu(items);
  };

  const handleClick = (e: MouseEvent) => {
    e.preventDefault();
    e.stopPropagation();
    executeRadialSelected(items);
    closeRadialMenu();
  };

  const handleMouseUp = (e: MouseEvent) => {
    if (e.button === 2) {
      if (executeRadialSelected(items)) closeRadialMenu();
    }
  };

  const handleMouseDown = (e: MouseEvent) => {
    if (e.button === 2) { e.preventDefault(); closeRadialMenu(); }
  };

  const handleContextMenu = (e: MouseEvent) => { e.preventDefault(); };

  const handleKeyDown = (e: KeyboardEvent) => {
    if (e.key === "Escape") { e.preventDefault(); closeRadialMenu(); return; }
    if (e.key === "`" || e.code === "Backquote") { e.preventDefault(); executeRadialSelected(items); closeRadialMenu(); return; }
    if (e.key === "Tab") {
      e.preventDefault();
      const enabled = items.map((item, i) => ({ i, disabled: item.disabled })).filter((x) => !x.disabled).map((x) => x.i);
      if (enabled.length === 0) return;
      const cur = state.radialMenu.selectedIndex;
      const pos = cur !== null ? enabled.indexOf(cur) : -1;
      const next = e.shiftKey ? (pos <= 0 ? enabled.length - 1 : pos - 1) : (pos < 0 || pos >= enabled.length - 1 ? 0 : pos + 1);
      state.radialMenu.selectedIndex = enabled[next];
      renderRadialMenu(items);
      return;
    }
    if (e.key === "Enter") { e.preventDefault(); executeRadialSelected(items); closeRadialMenu(); }
  };

  const handleKeyUp = (e: KeyboardEvent) => {
    if (e.key === "`" || e.code === "Backquote") {
      e.preventDefault();
      if (executeRadialSelected(items)) closeRadialMenu();
    }
  };

  document.addEventListener("mousemove", handleMouseMove);
  document.addEventListener("click", handleClick);
  document.addEventListener("mouseup", handleMouseUp);
  document.addEventListener("mousedown", handleMouseDown);
  document.addEventListener("contextmenu", handleContextMenu);
  document.addEventListener("keydown", handleKeyDown);
  document.addEventListener("keyup", handleKeyUp);

  radialCleanup = () => {
    document.removeEventListener("mousemove", handleMouseMove);
    document.removeEventListener("click", handleClick);
    document.removeEventListener("mouseup", handleMouseUp);
    document.removeEventListener("mousedown", handleMouseDown);
    document.removeEventListener("contextmenu", handleContextMenu);
    document.removeEventListener("keydown", handleKeyDown);
    document.removeEventListener("keyup", handleKeyUp);
  };
}

// ─── Radial menu triggers ───

function bindRadialMenu(): void {
  const canvas = $("storm-canvas");
  if (!canvas) return;

  // Track cursor position
  canvas.addEventListener("mousemove", (e) => { state.lastCursor = { x: e.clientX, y: e.clientY }; }, { passive: true });

  // Right-click on canvas
  canvas.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    if (state.radialMenu.open) return;
    // If right-clicked on a node, select it first
    const node = (e.target as HTMLElement).closest<HTMLElement>(".storm-node");
    if (node?.dataset.runId) setActiveRun(node.dataset.runId);
    openRadialMenu(e.clientX, e.clientY);
  });

  // Backtick key
  window.addEventListener("keydown", (e) => {
    if (state.radialMenu.open) return;
    if (e.key === "`" || e.code === "Backquote") {
      // Don't trigger if typing in textarea
      if ((e.target as HTMLElement).tagName === "TEXTAREA" || (e.target as HTMLElement).tagName === "INPUT") return;
      e.preventDefault();
      openRadialMenu(state.lastCursor.x, state.lastCursor.y);
    }
  });
}

function bindBoardObserver(): void {
  const board = $("storm-board");
  if (!board) return;
  const observer = new MutationObserver((mutations) => {
    if (mutations.some((mutation) => mutation.type === "childList")) {
      hydrateBoardFromDom();
    }
  });
  observer.observe(board, { childList: true, subtree: true });
}

function bindStormApp(): void {
  if (getConfig().currentPath !== "/app") return;
  setAvatarInitials();
  renderDraftContext();
  updateBoardTransform();
  bindBoardObserver();
  bindCanvasInteractions();
  bindNodeInteractions();
  bindAppChrome();
  bindRadialMenu();
  hydrateBoardFromDom();
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

window.designstormAuth = { signIn, signUp, signOut };
window.designstormSettings = { connectCodex };

void bootstrap();
