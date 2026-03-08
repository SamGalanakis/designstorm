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

type ClientTelemetryPayload = {
  eventType: string;
  message?: string | null;
  details?: Record<string, unknown> | null;
  href: string;
  userAgent: string;
  timestamp: string;
};

type Point = { x: number; y: number };

type DraftContext = {
  mode: "fork" | "combine";
  sourceIds: string[];
  label: string;
};

type BoardNode = {
  id: string;
  nodeType: "entropy" | "user_input" | "generate";
  positionX: number;
  positionY: number;
  content: Record<string, unknown>;
  locked: boolean;
};

type BoardEdge = {
  id: string;
  sourceId: string;
  sourceType: string;
  targetId: string;
  targetType: string;
  sourceAnchorSide: string | null;
  sourceAnchorT: number | null;
  targetAnchorSide: string | null;
  targetAnchorT: number | null;
};

type AnchorSide = "top" | "bottom" | "left" | "right";

type AnchorPoint = {
  side: AnchorSide;
  t: number;
  worldX: number;
  worldY: number;
};

type PointerState =
  | { mode: "pan"; pointerId: number; startClient: Point; startPan: Point }
  | { mode: "drag"; pointerId: number; runId: string; startClient: Point; startPos: Point; moved: boolean }
  | { mode: "drag-board-node"; pointerId: number; nodeId: string; startClient: Point; startPos: Point; moved: boolean }
  | { mode: "wire"; pointerId: number; sourceRunId: string; sourceType: string; sourceAnchor: AnchorPoint; startWorld: Point; currentWorld: Point; targetRunId: string | null; targetType: string | null; targetAnchor: AnchorPoint | null }
  | { mode: "resize"; pointerId: number; nodeId: string; startClient: Point; startSize: { w: number; h: number } }
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
  boardNodes: BoardNode[];
  edges: BoardEdge[];
  positions: Map<string, Point>;
  lineage: Map<string, string[]>;
  activeRunId: string | null;
  activeNodeId: string | null;
  focusedRunId: string | null;
  combineSourceId: string | null;
  draftContext: DraftContext | null;
  pendingLineage: DraftContext | null;
  pan: Point;
  scale: number;
  pointerState: PointerState;
  radialMenu: RadialMenuState;
  lastCursor: Point;
  spacePanHeld: boolean;
};

type WorldBounds = {
  minX: number;
  minY: number;
  maxX: number;
  maxY: number;
};

type ChunkCoord = {
  x: number;
  y: number;
};

type CreateBoardNodeOptions = {
  sourceId?: string;
  sourceType?: string;
  sourceAnchor?: AnchorPoint;
};

type ChunkRecord = {
  key: string;
  coord: ChunkCoord;
  canvas: HTMLCanvasElement;
  ctx: CanvasRenderingContext2D;
  state: "queued" | "rendering" | "ready";
  lastUsedAt: number;
};

type BackgroundRenderer = {
  canvas: HTMLCanvasElement;
  gl: WebGL2RenderingContext;
  program: WebGLProgram;
  buffer: WebGLBuffer;
  uChunkOrigin: WebGLUniformLocation;
  uWorldPerPixel: WebGLUniformLocation;
  uSeed: WebGLUniformLocation;
};

type BackgroundState = {
  container: HTMLElement | null;
  displayCanvas: HTMLCanvasElement | null;
  renderer: BackgroundRenderer | null;
  chunks: Map<string, ChunkRecord>;
  queue: string[];
  queued: Set<string>;
  renderRaf: number | null;
  drawRaf: number | null;
  fallback: boolean;
  seed: number;
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
const ENTROPY_NODE_WIDTH = 240;
const ENTROPY_NODE_HEIGHT = 180;
const GENERATE_NODE_WIDTH = 200;
const GENERATE_NODE_HEIGHT = 140;
const INPUT_NODE_WIDTH = 220;
const INPUT_NODE_HEIGHT = 120;
const EDGE_HANDLE_PROXIMITY = 30;
const INITIAL_PAN: Point = { x: 160, y: 120 };
const BACKGROUND_CHUNK_WORLD_SIZE = 1536;
const BACKGROUND_CHUNK_PIXEL_SIZE = 768;
const BACKGROUND_CHUNK_OVERSCAN = 1;
const BACKGROUND_CHUNK_BLEED_PX = 2;
const MAX_BACKGROUND_CHUNKS = 36;
const CONNECTION_PADDING = 240;
const BOARD_PAN_WORLD_PADDING = 1400;
const EMPTY_BOARD_PAN_WORLD_RADIUS = 1800;
const MIN_BOARD_SCALE = 0.35;
const MAX_BOARD_SCALE = 2.5;
const WHEEL_ZOOM_SENSITIVITY = 0.0015;

let clerk: ClerkLike | null = null;
let isSyncing = false;
let authPollTimer: number | null = null;
const telemetryCooldowns = new Map<string, number>();

const state: StormState = {
  runs: [],
  boardNodes: [],
  edges: [],
  positions: new Map(),
  lineage: new Map(),
  activeRunId: null,
  activeNodeId: null,
  focusedRunId: null,
  combineSourceId: null,
  draftContext: null,
  pendingLineage: null,
  pan: { ...INITIAL_PAN },
  scale: 1,
  pointerState: null,
  radialMenu: { open: false, position: { x: 0, y: 0 }, selectedIndex: null },
  lastCursor: { x: 0, y: 0 },
  spacePanHeld: false,
};

const backgroundState: BackgroundState = {
  container: null,
  displayCanvas: null,
  renderer: null,
  chunks: new Map(),
  queue: [],
  queued: new Set(),
  renderRaf: null,
  drawRaf: null,
  fallback: false,
  seed: 7.137,
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

function isEditableTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
}

function normalizeWheelDelta(value: number, deltaMode: number): number {
  if (deltaMode === WheelEvent.DOM_DELTA_LINE) return value * 16;
  if (deltaMode === WheelEvent.DOM_DELTA_PAGE) return value * window.innerHeight;
  return value;
}

function zoomBoardAtPoint(canvas: HTMLElement, clientX: number, clientY: number, deltaY: number): void {
  const rect = canvas.getBoundingClientRect();
  const localX = clientX - rect.left;
  const localY = clientY - rect.top;
  const prevScale = state.scale;
  const worldX = (localX - state.pan.x) / prevScale;
  const worldY = (localY - state.pan.y) / prevScale;
  const factor = Math.pow(2, -deltaY * WHEEL_ZOOM_SENSITIVITY);
  const nextScale = Math.min(MAX_BOARD_SCALE, Math.max(MIN_BOARD_SCALE, prevScale * factor));
  if (Math.abs(nextScale - prevScale) < 0.0001) return;
  state.scale = nextScale;
  state.pan = {
    x: localX - worldX * nextScale,
    y: localY - worldY * nextScale,
  };
  updateBoardTransform();
}

function createWorldBounds(minX: number, minY: number, maxX: number, maxY: number): WorldBounds {
  return { minX, minY, maxX, maxY };
}

function padWorldBounds(bounds: WorldBounds, padding: number): WorldBounds {
  return {
    minX: bounds.minX - padding,
    minY: bounds.minY - padding,
    maxX: bounds.maxX + padding,
    maxY: bounds.maxY + padding,
  };
}

function getWorldBoundsSize(bounds: WorldBounds): { width: number; height: number } {
  return {
    width: Math.max(1, Math.ceil(bounds.maxX - bounds.minX)),
    height: Math.max(1, Math.ceil(bounds.maxY - bounds.minY)),
  };
}

function getVisibleWorldBounds(): WorldBounds {
  const canvas = $("storm-canvas");
  if (!canvas) return createWorldBounds(0, 0, BOARD_WIDTH, BOARD_HEIGHT);
  const rect = canvas.getBoundingClientRect();
  const minX = -state.pan.x / state.scale;
  const minY = -state.pan.y / state.scale;
  return createWorldBounds(minX, minY, minX + rect.width / state.scale, minY + rect.height / state.scale);
}

function getNodeDimensions(id: string): { w: number; h: number } {
  const boardNode = state.boardNodes.find((n) => n.id === id);
  if (boardNode) {
    if (boardNode.nodeType === "generate") return { w: GENERATE_NODE_WIDTH, h: GENERATE_NODE_HEIGHT };
    if (boardNode.nodeType === "user_input") return { w: INPUT_NODE_WIDTH, h: INPUT_NODE_HEIGHT };
    return { w: ENTROPY_NODE_WIDTH, h: ENTROPY_NODE_HEIGHT };
  }
  return { w: CARD_WIDTH, h: CARD_HEIGHT };
}

function getAnchorWorldPos(nodeId: string, side: AnchorSide, t: number): { x: number; y: number } {
  const pos = state.positions.get(nodeId);
  if (!pos) return { x: 0, y: 0 };
  const { w, h } = getNodeDimensions(nodeId);
  switch (side) {
    case "top": return { x: pos.x + w * t, y: pos.y };
    case "bottom": return { x: pos.x + w * t, y: pos.y + h };
    case "left": return { x: pos.x, y: pos.y + h * t };
    case "right": return { x: pos.x + w, y: pos.y + h * t };
  }
}

function getEdgeProximity(nodeId: string, worldPt: Point): AnchorPoint | null {
  const pos = state.positions.get(nodeId);
  if (!pos) return null;
  const { w, h } = getNodeDimensions(nodeId);

  const sides: { side: AnchorSide; dist: number; t: number; wx: number; wy: number }[] = [];

  // Top edge
  if (worldPt.x >= pos.x && worldPt.x <= pos.x + w) {
    const d = Math.abs(worldPt.y - pos.y);
    if (d < EDGE_HANDLE_PROXIMITY) sides.push({ side: "top", dist: d, t: (worldPt.x - pos.x) / w, wx: worldPt.x, wy: pos.y });
  }
  // Bottom edge
  if (worldPt.x >= pos.x && worldPt.x <= pos.x + w) {
    const d = Math.abs(worldPt.y - (pos.y + h));
    if (d < EDGE_HANDLE_PROXIMITY) sides.push({ side: "bottom", dist: d, t: (worldPt.x - pos.x) / w, wx: worldPt.x, wy: pos.y + h });
  }
  // Left edge
  if (worldPt.y >= pos.y && worldPt.y <= pos.y + h) {
    const d = Math.abs(worldPt.x - pos.x);
    if (d < EDGE_HANDLE_PROXIMITY) sides.push({ side: "left", dist: d, t: (worldPt.y - pos.y) / h, wx: pos.x, wy: worldPt.y });
  }
  // Right edge
  if (worldPt.y >= pos.y && worldPt.y <= pos.y + h) {
    const d = Math.abs(worldPt.x - (pos.x + w));
    if (d < EDGE_HANDLE_PROXIMITY) sides.push({ side: "right", dist: d, t: (worldPt.y - pos.y) / h, wx: pos.x + w, wy: worldPt.y });
  }

  if (sides.length === 0) return null;
  sides.sort((a, b) => a.dist - b.dist);
  const best = sides[0];
  return { side: best.side, t: Math.max(0.1, Math.min(0.9, best.t)), worldX: best.wx, worldY: best.wy };
}

function getOccupiedWorldBounds(): WorldBounds | null {
  if (state.positions.size === 0) return null;
  let minX = Number.POSITIVE_INFINITY;
  let minY = Number.POSITIVE_INFINITY;
  let maxX = Number.NEGATIVE_INFINITY;
  let maxY = Number.NEGATIVE_INFINITY;

  state.positions.forEach((point, id) => {
    const { w, h } = getNodeDimensions(id);
    minX = Math.min(minX, point.x);
    minY = Math.min(minY, point.y);
    maxX = Math.max(maxX, point.x + w);
    maxY = Math.max(maxY, point.y + h);
  });

  if (!Number.isFinite(minX) || !Number.isFinite(minY) || !Number.isFinite(maxX) || !Number.isFinite(maxY)) {
    return null;
  }
  return createWorldBounds(minX, minY, maxX, maxY);
}

function getConnectionWorldBounds(): WorldBounds {
  const runBounds = getOccupiedWorldBounds();
  if (!runBounds) return createWorldBounds(0, 0, BOARD_WIDTH, BOARD_HEIGHT);
  return padWorldBounds(runBounds, CONNECTION_PADDING);
}

function getOccupiedNodeBounds(): WorldBounds[] {
  const bounds: WorldBounds[] = [];
  state.positions.forEach((point, id) => {
    const { w, h } = getNodeDimensions(id);
    bounds.push(createWorldBounds(point.x, point.y, point.x + w, point.y + h));
  });
  return bounds;
}

function getPanConstraintRegions(): WorldBounds[] {
  const occupied = getOccupiedNodeBounds();
  if (occupied.length > 0) {
    return occupied.map((bounds) => padWorldBounds(bounds, BOARD_PAN_WORLD_PADDING));
  }

  const centerX = BOARD_WIDTH * 0.5;
  const centerY = BOARD_HEIGHT * 0.5;
  return [
    createWorldBounds(
      centerX - EMPTY_BOARD_PAN_WORLD_RADIUS,
      centerY - EMPTY_BOARD_PAN_WORLD_RADIUS,
      centerX + EMPTY_BOARD_PAN_WORLD_RADIUS,
      centerY + EMPTY_BOARD_PAN_WORLD_RADIUS,
    ),
  ];
}

function pointWithinBounds(point: Point, bounds: WorldBounds): boolean {
  return point.x >= bounds.minX && point.x <= bounds.maxX && point.y >= bounds.minY && point.y <= bounds.maxY;
}

function clampPointToBounds(point: Point, bounds: WorldBounds): Point {
  return {
    x: Math.min(bounds.maxX, Math.max(bounds.minX, point.x)),
    y: Math.min(bounds.maxY, Math.max(bounds.minY, point.y)),
  };
}

function clampPanToWorldBounds(pan: Point, scale: number): Point {
  const canvas = $("storm-canvas");
  const regions = getPanConstraintRegions();
  if (!canvas || regions.length === 0) return pan;

  const rect = canvas.getBoundingClientRect();
  if (rect.width <= 0 || rect.height <= 0) return pan;

  const viewportCenterWorld = {
    x: (rect.width * 0.5 - pan.x) / scale,
    y: (rect.height * 0.5 - pan.y) / scale,
  };
  const containingRegion = regions.find((bounds) => pointWithinBounds(viewportCenterWorld, bounds));
  const clampedCenterWorld = containingRegion
    ? viewportCenterWorld
    : regions.reduce<{ point: Point; distSq: number }>((best, bounds) => {
      const projected = clampPointToBounds(viewportCenterWorld, bounds);
      const dx = projected.x - viewportCenterWorld.x;
      const dy = projected.y - viewportCenterWorld.y;
      const distSq = dx * dx + dy * dy;
      if (distSq < best.distSq) return { point: projected, distSq };
      return best;
    }, { point: clampPointToBounds(viewportCenterWorld, regions[0]), distSq: Number.POSITIVE_INFINITY }).point;

  return {
    x: rect.width * 0.5 - clampedCenterWorld.x * scale,
    y: rect.height * 0.5 - clampedCenterWorld.y * scale,
  };
}

let boardViewAnimationFrame: number | null = null;

function stopBoardViewAnimation(): void {
  if (boardViewAnimationFrame !== null) {
    cancelAnimationFrame(boardViewAnimationFrame);
    boardViewAnimationFrame = null;
  }
}

function animateBoardView(targetPan: Point, targetScale: number, duration = 260): void {
  stopBoardViewAnimation();
  const startPan = { ...state.pan };
  const startScale = state.scale;
  const startedAt = performance.now();

  const step = (now: number) => {
    const t = Math.min(1, (now - startedAt) / duration);
    const eased = 1 - Math.pow(1 - t, 3);
    state.pan = {
      x: startPan.x + (targetPan.x - startPan.x) * eased,
      y: startPan.y + (targetPan.y - startPan.y) * eased,
    };
    state.scale = startScale + (targetScale - startScale) * eased;
    updateBoardTransform();
    if (t < 1) boardViewAnimationFrame = requestAnimationFrame(step);
    else boardViewAnimationFrame = null;
  };

  boardViewAnimationFrame = requestAnimationFrame(step);
}

function centerRunInView(runId: string, opts?: { animate?: boolean; targetScale?: number }): void {
  const canvas = $("storm-canvas");
  const point = state.positions.get(runId);
  if (!canvas || !point) return;
  const rect = canvas.getBoundingClientRect();
  const nextScale = opts?.targetScale ?? state.scale;
  const targetPan = {
    x: rect.width * 0.5 - (point.x + CARD_WIDTH * 0.5) * nextScale,
    y: rect.height * 0.5 - (point.y + CARD_HEIGHT * 0.5) * nextScale,
  };

  if (opts?.animate === false) {
    stopBoardViewAnimation();
    state.scale = nextScale;
    state.pan = targetPan;
    updateBoardTransform();
    return;
  }

  animateBoardView(targetPan, nextScale);
}

function getChunkKey(coord: ChunkCoord): string {
  return `${coord.x}:${coord.y}`;
}

function getChunkWorldOrigin(coord: ChunkCoord): Point {
  return {
    x: coord.x * BACKGROUND_CHUNK_WORLD_SIZE,
    y: coord.y * BACKGROUND_CHUNK_WORLD_SIZE,
  };
}

function getChunkDistanceSq(coord: ChunkCoord, bounds: WorldBounds): number {
  const centerX = (bounds.minX + bounds.maxX) * 0.5;
  const centerY = (bounds.minY + bounds.maxY) * 0.5;
  const origin = getChunkWorldOrigin(coord);
  const chunkCenterX = origin.x + BACKGROUND_CHUNK_WORLD_SIZE * 0.5;
  const chunkCenterY = origin.y + BACKGROUND_CHUNK_WORLD_SIZE * 0.5;
  const dx = chunkCenterX - centerX;
  const dy = chunkCenterY - centerY;
  return dx * dx + dy * dy;
}

function getChunkWorldPerPixel(): number {
  return BACKGROUND_CHUNK_WORLD_SIZE / BACKGROUND_CHUNK_PIXEL_SIZE;
}

function getChunkBleedWorld(): number {
  return getChunkWorldPerPixel() * BACKGROUND_CHUNK_BLEED_PX;
}

function getRenderedChunkPixelSize(): number {
  return BACKGROUND_CHUNK_PIXEL_SIZE + BACKGROUND_CHUNK_BLEED_PX * 2;
}

function getBackgroundDisplayScale(): number {
  return Math.min(window.devicePixelRatio || 1, 1.5);
}

function intersectsWorldBounds(a: WorldBounds, b: WorldBounds): boolean {
  return a.minX < b.maxX && a.maxX > b.minX && a.minY < b.maxY && a.maxY > b.minY;
}

function clearBackgroundChunks(): void {
  backgroundState.chunks.forEach((record) => record.canvas.remove());
  backgroundState.chunks.clear();
  backgroundState.queue = [];
  backgroundState.queued.clear();
  if (backgroundState.renderRaf !== null) {
    cancelAnimationFrame(backgroundState.renderRaf);
    backgroundState.renderRaf = null;
  }
  if (backgroundState.drawRaf !== null) {
    cancelAnimationFrame(backgroundState.drawRaf);
    backgroundState.drawRaf = null;
  }
}

function setBackgroundFallback(reason: string): void {
  clearBackgroundChunks();
  backgroundState.fallback = true;
  backgroundState.renderer = null;
  backgroundState.displayCanvas?.remove();
  backgroundState.displayCanvas = null;
  backgroundState.container?.classList.add("is-fallback");
  reportClientEvent("board_background_fallback", { reason }, { cooldownMs: 10000 });
}

function compileBackgroundShader(gl: WebGL2RenderingContext, type: number, source: string): WebGLShader | null {
  const shader = gl.createShader(type);
  if (!shader) return null;
  gl.shaderSource(shader, source);
  gl.compileShader(shader);
  if (!gl.getShaderParameter(shader, gl.COMPILE_STATUS)) {
    console.error(gl.getShaderInfoLog(shader));
    gl.deleteShader(shader);
    return null;
  }
  return shader;
}

function createBackgroundRenderer(canvas: HTMLCanvasElement): BackgroundRenderer | null {
  const gl = canvas.getContext("webgl2", {
    alpha: false,
    antialias: false,
    depth: false,
    stencil: false,
    preserveDrawingBuffer: false,
    desynchronized: true,
    powerPreference: "high-performance",
  });
  if (!gl) return null;

  const vs = `#version 300 es
    in vec2 a_pos;
    void main() { gl_Position = vec4(a_pos, 0.0, 1.0); }
  `;

  const fs = `#version 300 es
    precision highp float;
    uniform vec2 u_chunk_origin;
    uniform float u_world_per_pixel;
    uniform float u_seed;
    out vec4 fragColor;

    vec3 mod289(vec3 x) { return x - floor(x * (1.0 / 289.0)) * 289.0; }
    vec4 mod289(vec4 x) { return x - floor(x * (1.0 / 289.0)) * 289.0; }
    vec4 permute(vec4 x) { return mod289(((x * 34.0) + 10.0) * x); }
    vec4 taylorInvSqrt(vec4 r) { return 1.79284291400159 - 0.85373472095314 * r; }

    float snoise(vec3 v) {
      const vec2 C = vec2(1.0 / 6.0, 1.0 / 3.0);
      const vec4 D = vec4(0.0, 0.5, 1.0, 2.0);
      vec3 i = floor(v + dot(v, C.yyy));
      vec3 x0 = v - i + dot(i, C.xxx);
      vec3 g = step(x0.yzx, x0.xyz);
      vec3 l = 1.0 - g;
      vec3 i1 = min(g.xyz, l.zxy);
      vec3 i2 = max(g.xyz, l.zxy);
      vec3 x1 = x0 - i1 + C.xxx;
      vec3 x2 = x0 - i2 + C.yyy;
      vec3 x3 = x0 - D.yyy;
      i = mod289(i);
      vec4 p = permute(permute(permute(
        i.z + vec4(0.0, i1.z, i2.z, 1.0))
        + i.y + vec4(0.0, i1.y, i2.y, 1.0))
        + i.x + vec4(0.0, i1.x, i2.x, 1.0));
      float n_ = 0.142857142857;
      vec3 ns = n_ * D.wyz - D.xzx;
      vec4 j = p - 49.0 * floor(p * ns.z * ns.z);
      vec4 x_ = floor(j * ns.z);
      vec4 y_ = floor(j - 7.0 * x_);
      vec4 x = x_ * ns.x + ns.yyyy;
      vec4 y = y_ * ns.x + ns.yyyy;
      vec4 h = 1.0 - abs(x) - abs(y);
      vec4 b0 = vec4(x.xy, y.xy);
      vec4 b1 = vec4(x.zw, y.zw);
      vec4 s0 = floor(b0) * 2.0 + 1.0;
      vec4 s1 = floor(b1) * 2.0 + 1.0;
      vec4 sh = -step(h, vec4(0.0));
      vec4 a0 = b0.xzyw + s0.xzyw * sh.xxyy;
      vec4 a1 = b1.xzyw + s1.xzyw * sh.zzww;
      vec3 p0 = vec3(a0.xy, h.x);
      vec3 p1 = vec3(a0.zw, h.y);
      vec3 p2 = vec3(a1.xy, h.z);
      vec3 p3 = vec3(a1.zw, h.w);
      vec4 norm = taylorInvSqrt(vec4(dot(p0,p0), dot(p1,p1), dot(p2,p2), dot(p3,p3)));
      p0 *= norm.x;
      p1 *= norm.y;
      p2 *= norm.z;
      p3 *= norm.w;
      vec4 m = max(0.6 - vec4(dot(x0,x0), dot(x1,x1), dot(x2,x2), dot(x3,x3)), 0.0);
      m = m * m;
      return 42.0 * dot(m * m, vec4(dot(p0,x0), dot(p1,x1), dot(p2,x2), dot(p3,x3)));
    }

    float fbm5(vec3 p) {
      float v = 0.0;
      float a = 0.5;
      for (int i = 0; i < 5; i++) {
        v += a * snoise(p);
        p = p * 2.0 + vec3(100.0);
        a *= 0.5;
      }
      return v;
    }

    vec3 oklabToLinearSrgb(vec3 lab) {
      float l_ = lab.x + 0.3963377774 * lab.y + 0.2158037573 * lab.z;
      float m_ = lab.x - 0.1055613458 * lab.y - 0.0638541728 * lab.z;
      float s_ = lab.x - 0.0894841775 * lab.y - 1.2914855480 * lab.z;
      float l3 = l_ * l_ * l_;
      float m3 = m_ * m_ * m_;
      float s3 = s_ * s_ * s_;
      return vec3(
        +4.0767416621 * l3 - 3.3077115913 * m3 + 0.2309699292 * s3,
        -1.2684380046 * l3 + 2.6097574011 * m3 - 0.3413193965 * s3,
        -0.0041960863 * l3 - 0.7034186147 * m3 + 1.7076147010 * s3
      );
    }

    vec3 oklchToSrgb(float L, float C, float h) {
      float a = C * cos(h);
      float b = C * sin(h);
      vec3 lin = oklabToLinearSrgb(vec3(L, a, b));
      return pow(clamp(lin, 0.0, 1.0), vec3(1.0 / 2.2));
    }

    void main() {
      vec2 world = u_chunk_origin + gl_FragCoord.xy * u_world_per_pixel;
      float seed = u_seed;
      vec2 field = world / 900.0;

      vec2 q = vec2(
        snoise(vec3(field * 2.8 + vec2(4.0, -3.0), seed)),
        snoise(vec3(field * 2.8 + vec2(17.0, 9.0), seed + 13.0))
      );
      vec2 r = vec2(
        snoise(vec3((field + q * 0.14) * 2.8 + vec2(1.7, 9.2), seed + 29.0)),
        snoise(vec3((field + q * 0.14) * 2.8 + vec2(9.2, -3.4), seed + 43.0))
      );
      vec2 warped = field + r * 0.12;

      float h0 = fbm5(vec3(warped * 4.0, seed + 7.0));
      float eps = 0.006;
      float hx = fbm5(vec3((warped + vec2(eps, 0.0)) * 4.0, seed + 7.0));
      float hy = fbm5(vec3((warped + vec2(0.0, eps)) * 4.0, seed + 7.0));
      vec3 normal = normalize(vec3((h0 - hx) / eps * 0.15, (h0 - hy) / eps * 0.15, 1.0));
      vec3 lightDir = normalize(vec3(0.16, 0.08, 1.0));
      float light = 0.52 + max(dot(normal, lightDir), 0.0) * 0.28;

      float hueDriver = (q.x * 0.4 + r.x * 0.3 + h0 * 0.3) * 0.5 + 0.5;
      float hueRad = radians(195.0 + hueDriver * 60.0 + seed * 18.0);

      float L = 0.16 + light * 0.055 + h0 * 0.018;
      float C = 0.03 + hueDriver * 0.02;
      vec3 color = oklchToSrgb(L, C, hueRad);

      float grain = fbm5(vec3(world / 2.8, seed + 71.0)) * 0.5 + 0.5;
      color += (grain - 0.5) * 0.08;

      fragColor = vec4(clamp(color, 0.0, 1.0), 1.0);
    }
  `;

  const vertexShader = compileBackgroundShader(gl, gl.VERTEX_SHADER, vs);
  const fragmentShader = compileBackgroundShader(gl, gl.FRAGMENT_SHADER, fs);
  if (!vertexShader || !fragmentShader) return null;

  const program = gl.createProgram();
  if (!program) return null;
  gl.attachShader(program, vertexShader);
  gl.attachShader(program, fragmentShader);
  gl.linkProgram(program);
  if (!gl.getProgramParameter(program, gl.LINK_STATUS)) {
    console.error(gl.getProgramInfoLog(program));
    return null;
  }

  const buffer = gl.createBuffer();
  if (!buffer) return null;
  gl.bindBuffer(gl.ARRAY_BUFFER, buffer);
  gl.bufferData(gl.ARRAY_BUFFER, new Float32Array([-1, -1, 1, -1, -1, 1, 1, 1]), gl.STATIC_DRAW);
  gl.useProgram(program);
  const loc = gl.getAttribLocation(program, "a_pos");
  gl.enableVertexAttribArray(loc);
  gl.vertexAttribPointer(loc, 2, gl.FLOAT, false, 0, 0);
  gl.viewport(0, 0, canvas.width, canvas.height);

  const uChunkOrigin = gl.getUniformLocation(program, "u_chunk_origin");
  const uWorldPerPixel = gl.getUniformLocation(program, "u_world_per_pixel");
  const uSeed = gl.getUniformLocation(program, "u_seed");
  if (!uChunkOrigin || !uWorldPerPixel || !uSeed) return null;

  return {
    canvas,
    gl,
    program,
    buffer,
    uChunkOrigin,
    uWorldPerPixel,
    uSeed,
  };
}

function renderBackgroundChunk(record: ChunkRecord): void {
  const renderer = backgroundState.renderer;
  if (!renderer) return;
  const origin = getChunkWorldOrigin(record.coord);
  const bleedWorld = getChunkBleedWorld();
  const { gl, program, uChunkOrigin, uWorldPerPixel, uSeed, canvas } = renderer;
  gl.useProgram(program);
  gl.viewport(0, 0, canvas.width, canvas.height);
  gl.uniform2f(uChunkOrigin, origin.x - bleedWorld, origin.y - bleedWorld);
  gl.uniform1f(uWorldPerPixel, getChunkWorldPerPixel());
  gl.uniform1f(uSeed, backgroundState.seed);
  gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  record.ctx.clearRect(0, 0, record.canvas.width, record.canvas.height);
  record.ctx.drawImage(canvas, 0, 0, record.canvas.width, record.canvas.height);
}

function ensureBackgroundDisplayCanvas(): void {
  if (!backgroundState.container || backgroundState.displayCanvas) return;
  const canvas = document.createElement("canvas");
  canvas.className = "board-background-canvas";
  canvas.style.opacity = "0";
  backgroundState.container.appendChild(canvas);
  backgroundState.displayCanvas = canvas;
}

function resizeBackgroundDisplayCanvas(): boolean {
  ensureBackgroundDisplayCanvas();
  const canvas = backgroundState.displayCanvas;
  if (!canvas || !backgroundState.container) return false;
  const rect = backgroundState.container.getBoundingClientRect();
  const scale = getBackgroundDisplayScale();
  const nextWidth = Math.max(1, Math.round(rect.width * scale));
  const nextHeight = Math.max(1, Math.round(rect.height * scale));
  if (canvas.width === nextWidth && canvas.height === nextHeight) return false;
  canvas.width = nextWidth;
  canvas.height = nextHeight;
  backgroundState.renderer?.gl.viewport(0, 0, nextWidth, nextHeight);
  return true;
}

function drawBackgroundPresentation(): void {
  backgroundState.drawRaf = null;
  if (backgroundState.fallback) return;
  const renderer = backgroundState.renderer;
  const canvas = backgroundState.displayCanvas;
  if (!renderer || !canvas) return;

  resizeBackgroundDisplayCanvas();
  const bounds = getVisibleWorldBounds();
  const { gl, program, uChunkOrigin, uWorldPerPixel, uSeed } = renderer;
  gl.useProgram(program);
  gl.viewport(0, 0, canvas.width, canvas.height);
  gl.uniform2f(uChunkOrigin, bounds.minX, bounds.minY);
  gl.uniform1f(uWorldPerPixel, (bounds.maxX - bounds.minX) / Math.max(1, canvas.width));
  gl.uniform1f(uSeed, backgroundState.seed);
  gl.drawArrays(gl.TRIANGLE_STRIP, 0, 4);
  canvas.style.opacity = "1";
}

function requestBackgroundPresentationDraw(): void {
  if (backgroundState.fallback) return;
  if (backgroundState.drawRaf !== null) return;
  backgroundState.drawRaf = requestAnimationFrame(drawBackgroundPresentation);
}

function removeBackgroundChunk(key: string): void {
  const record = backgroundState.chunks.get(key);
  if (!record) return;
  record.canvas.remove();
  backgroundState.chunks.delete(key);
  backgroundState.queued.delete(key);
  backgroundState.queue = backgroundState.queue.filter((entry) => entry !== key);
}

function ensureBackgroundChunk(coord: ChunkCoord): ChunkRecord | null {
  const key = getChunkKey(coord);
  const existing = backgroundState.chunks.get(key);
  if (existing) {
    existing.lastUsedAt = performance.now();
    return existing;
  }

  const canvas = document.createElement("canvas");
  const ctx = canvas.getContext("2d", { alpha: false, desynchronized: true });
  if (!ctx) return null;
  const renderedChunkPixelSize = getRenderedChunkPixelSize();
  canvas.width = renderedChunkPixelSize;
  canvas.height = renderedChunkPixelSize;
  ctx.imageSmoothingEnabled = true;

  const record: ChunkRecord = {
    key,
    coord,
    canvas,
    ctx,
    state: "queued",
    lastUsedAt: performance.now(),
  };
  backgroundState.chunks.set(key, record);
  return record;
}

function scheduleBackgroundRender(): void {
  if (backgroundState.renderRaf !== null || backgroundState.fallback || !backgroundState.renderer) return;
  backgroundState.renderRaf = requestAnimationFrame(() => {
    backgroundState.renderRaf = null;
    const nextKey = backgroundState.queue.shift();
    if (!nextKey) return;
    backgroundState.queued.delete(nextKey);
    const record = backgroundState.chunks.get(nextKey);
    if (record) {
      record.state = "rendering";
      renderBackgroundChunk(record);
      record.state = "ready";
      record.lastUsedAt = performance.now();
      requestBackgroundPresentationDraw();
    }
    if (backgroundState.queue.length > 0) scheduleBackgroundRender();
  });
}

function evictBackgroundChunks(required: Set<string>, bounds: WorldBounds): void {
  if (backgroundState.chunks.size <= MAX_BACKGROUND_CHUNKS) return;
  const removable = Array.from(backgroundState.chunks.values())
    .filter((record) => !required.has(record.key))
    .sort((a, b) => {
      if (a.lastUsedAt !== b.lastUsedAt) return a.lastUsedAt - b.lastUsedAt;
      return getChunkDistanceSq(b.coord, bounds) - getChunkDistanceSq(a.coord, bounds);
    });

  while (backgroundState.chunks.size > MAX_BACKGROUND_CHUNKS && removable.length > 0) {
    const record = removable.shift();
    if (!record) break;
    removeBackgroundChunk(record.key);
  }
}

function syncBoardBackground(): void {
  if (backgroundState.fallback || !backgroundState.container || !backgroundState.renderer) return;
  requestBackgroundPresentationDraw();
}

function initBoardBackground(): void {
  backgroundState.container = $("storm-background");
  if (!backgroundState.container) return;
  backgroundState.container.classList.remove("is-fallback");
  backgroundState.fallback = false;
  ensureBackgroundDisplayCanvas();
  resizeBackgroundDisplayCanvas();
  if (!backgroundState.displayCanvas) {
    setBackgroundFallback("display_canvas_unavailable");
    return;
  }
  backgroundState.renderer = createBackgroundRenderer(backgroundState.displayCanvas);
  if (!backgroundState.renderer) {
    setBackgroundFallback("webgl_unavailable");
    return;
  }
  drawBackgroundPresentation();
  window.addEventListener("resize", () => {
    resizeBackgroundDisplayCanvas();
    syncBoardBackground();
  }, { passive: true });
}

function reportClientEvent(
  eventType: string,
  details?: Record<string, unknown>,
  opts?: { cooldownMs?: number; message?: string },
): void {
  const key = `${eventType}:${JSON.stringify(details ?? {})}`;
  const now = Date.now();
  const cooldownMs = opts?.cooldownMs ?? 0;
  const previous = telemetryCooldowns.get(key) ?? 0;
  if (cooldownMs > 0 && now - previous < cooldownMs) return;
  telemetryCooldowns.set(key, now);

  const payload: ClientTelemetryPayload = {
    eventType,
    message: opts?.message ?? null,
    details: details ?? null,
    href: window.location.href,
    userAgent: window.navigator.userAgent,
    timestamp: new Date(now).toISOString(),
  };

  try {
    const body = JSON.stringify(payload);
    const blob = new Blob([body], { type: "application/json" });
    if (navigator.sendBeacon) {
      navigator.sendBeacon("/telemetry/client", blob);
      return;
    }
    void fetch("/telemetry/client", {
      method: "POST",
      credentials: "include",
      keepalive: true,
      headers: { "Content-Type": "application/json" },
      body,
    });
  } catch {
    // Telemetry must never break the app.
  }
}

function setBoundValue(id: string, value: string): void {
  const input = $(id) as HTMLInputElement | HTMLTextAreaElement | null;
  if (!input) return;
  input.value = value;
  input.dispatchEvent(new Event("input", { bubbles: true }));
  input.dispatchEvent(new Event("change", { bubbles: true }));
}

function getBoundValue(id: string): string {
  const input = $(id) as HTMLInputElement | HTMLTextAreaElement | null;
  return input?.value ?? "";
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

function assignPosition(
  run: StormRun,
  index: number,
  sourceIds?: string[],
  positions: Map<string, Point> = state.positions,
): void {
  if (positions.has(run.id)) return;
  if (sourceIds?.length) {
    const pts = sourceIds.map((id) => positions.get(id)).filter((p): p is Point => Boolean(p));
    if (pts.length > 0) {
      const c = pts.reduce((a, p) => ({ x: a.x + p.x, y: a.y + p.y }), { x: 0, y: 0 });
      const off = sourceIds.length > 1 ? 170 : 250;
      positions.set(run.id, { x: c.x / pts.length + off, y: c.y / pts.length + 110 });
      return;
    }
  }
  const col = index % 4;
  const row = Math.floor(index / 4);
  positions.set(run.id, { x: 220 + col * 360 + (row % 2) * 70, y: 240 + row * 290 + (col % 2) * 28 });
}

function setStatus(msg: string): void {
  const el = $("storm-status");
  if (el) el.textContent = msg;
  setBoundValue("storm-status-signal", msg);
}

function setIframeSource(frame: HTMLIFrameElement, src: string | null): void {
  const current = frame.getAttribute("src");
  if (!src) {
    if (current !== null) frame.removeAttribute("src");
    return;
  }
  if (current === src) return;
  frame.setAttribute("src", src);
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
  renderFocus();

  if (opts?.sync) syncUrl(false);
}

function openFullscreen(id: string): void {
  state.activeRunId = id;
  state.focusedRunId = id;
  renderRuns();
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
  state.pan = clampPanToWorldBounds(state.pan, state.scale);
  board.style.transform = `translate(${state.pan.x}px, ${state.pan.y}px) scale(${state.scale})`;
  syncBoardBackground();
}

function getControlOffset(side: AnchorSide, tension: number): Point {
  switch (side) {
    case "top": return { x: 0, y: -Math.abs(tension) };
    case "bottom": return { x: 0, y: Math.abs(tension) };
    case "left": return { x: -Math.abs(tension), y: 0 };
    case "right": return { x: Math.abs(tension), y: 0 };
  }
}

function renderConnections(): void {
  const svg = $("storm-lines") as unknown as SVGSVGElement | null;
  if (!svg) return;
  const bounds = getConnectionWorldBounds();
  const { width, height } = getWorldBoundsSize(bounds);
  svg.style.left = `${bounds.minX}px`;
  svg.style.top = `${bounds.minY}px`;
  svg.style.width = `${width}px`;
  svg.style.height = `${height}px`;
  svg.setAttribute("viewBox", `0 0 ${width} ${height}`);
  svg.innerHTML = "";

  // Lineage lines (design → design)
  state.lineage.forEach((parents, runId) => {
    const child = state.positions.get(runId);
    if (!child) return;
    parents.forEach((pid) => {
      const parent = state.positions.get(pid);
      if (!parent) return;
      const sx = parent.x + CARD_WIDTH * 0.5 - bounds.minX;
      const sy = parent.y + CARD_HEIGHT - bounds.minY;
      const ex = child.x + CARD_WIDTH * 0.5 - bounds.minX;
      const ey = child.y - bounds.minY;
      const tension = Math.min(Math.abs(ey - sy) * 0.4, 80);
      const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
      path.setAttribute("d", `M ${sx} ${sy} C ${sx} ${sy + tension}, ${ex} ${ey - tension}, ${ex} ${ey}`);
      path.setAttribute("class", "storm-line");
      svg.appendChild(path);
    });
  });

  // Board edge lines
  state.edges.forEach((edge) => {
    const srcPos = state.positions.get(edge.sourceId);
    const tgtPos = state.positions.get(edge.targetId);
    if (!srcPos || !tgtPos) return;
    const srcDim = getNodeDimensions(edge.sourceId);
    const tgtDim = getNodeDimensions(edge.targetId);

    let sx: number, sy: number, ex: number, ey: number;
    let srcSide: AnchorSide = "bottom";
    let tgtSide: AnchorSide = "top";

    if (edge.sourceAnchorSide && edge.sourceAnchorT != null) {
      srcSide = edge.sourceAnchorSide as AnchorSide;
      const anchor = getAnchorWorldPos(edge.sourceId, srcSide, edge.sourceAnchorT);
      sx = anchor.x - bounds.minX;
      sy = anchor.y - bounds.minY;
    } else {
      sx = srcPos.x + srcDim.w * 0.5 - bounds.minX;
      sy = srcPos.y + srcDim.h - bounds.minY;
    }

    if (edge.targetAnchorSide && edge.targetAnchorT != null) {
      tgtSide = edge.targetAnchorSide as AnchorSide;
      const anchor = getAnchorWorldPos(edge.targetId, tgtSide, edge.targetAnchorT);
      ex = anchor.x - bounds.minX;
      ey = anchor.y - bounds.minY;
    } else {
      ex = tgtPos.x + tgtDim.w * 0.5 - bounds.minX;
      ey = tgtPos.y - bounds.minY;
    }

    const dx = ex - sx;
    const dy = ey - sy;
    const dist = Math.sqrt(dx * dx + dy * dy);
    const tension = Math.min(dist * 0.4, 80);

    const srcCtrl = getControlOffset(srcSide, tension);
    const tgtCtrl = getControlOffset(tgtSide, -tension);

    const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
    path.setAttribute("d", `M ${sx} ${sy} C ${sx + srcCtrl.x} ${sy + srcCtrl.y}, ${ex + tgtCtrl.x} ${ey + tgtCtrl.y}, ${ex} ${ey}`);
    path.setAttribute("class", "board-edge-line");
    svg.appendChild(path);
  });
}

function renderWire(): void {
  const svg = $("storm-lines") as unknown as SVGSVGElement | null;
  if (!svg) return;
  // Remove any previous wire elements
  svg.querySelectorAll(".storm-wire, .storm-wire-head").forEach((el) => el.remove());
  if (!state.pointerState || state.pointerState.mode !== "wire") return;

  const { sourceAnchor, currentWorld } = state.pointerState;
  const sx = sourceAnchor.worldX;
  const sy = sourceAnchor.worldY;
  const ex = currentWorld.x;
  const ey = currentWorld.y;

  // Read current SVG offset from its style
  const svgLeft = parseFloat(svg.style.left) || 0;
  const svgTop = parseFloat(svg.style.top) || 0;

  // Convert world coords to SVG-local coords
  const lsx = sx - svgLeft;
  const lsy = sy - svgTop;
  const lex = ex - svgLeft;
  const ley = ey - svgTop;

  const dist = Math.sqrt((lex - lsx) ** 2 + (ley - lsy) ** 2);
  const tension = Math.min(dist * 0.4, 80);
  const srcCtrl = getControlOffset(sourceAnchor.side, tension);

  const path = document.createElementNS("http://www.w3.org/2000/svg", "path");
  path.setAttribute("d", `M ${lsx} ${lsy} C ${lsx + srcCtrl.x} ${lsy + srcCtrl.y}, ${lex} ${ley}, ${lex} ${ley}`);
  path.setAttribute("class", "storm-wire");
  svg.appendChild(path);

  // Small circle at the end
  const head = document.createElementNS("http://www.w3.org/2000/svg", "circle");
  head.setAttribute("cx", String(lex));
  head.setAttribute("cy", String(ley));
  head.setAttribute("r", "4");
  head.setAttribute("class", "storm-wire-head");
  svg.appendChild(head);
}

function clientToWorld(clientX: number, clientY: number): Point {
  return {
    x: (clientX - state.pan.x) / state.scale,
    y: (clientY - state.pan.y) / state.scale,
  };
}

function hitTestNode(worldPt: Point, excludeId?: string): string | null {
  for (const run of state.runs) {
    if (run.id === excludeId) continue;
    const pos = state.positions.get(run.id);
    if (!pos) continue;
    if (worldPt.x >= pos.x && worldPt.x <= pos.x + CARD_WIDTH && worldPt.y >= pos.y && worldPt.y <= pos.y + CARD_HEIGHT) {
      return run.id;
    }
  }
  for (const node of state.boardNodes) {
    if (node.id === excludeId) continue;
    const pos = state.positions.get(node.id);
    if (!pos) continue;
    const dim = getNodeDimensions(node.id);
    if (worldPt.x >= pos.x && worldPt.x <= pos.x + dim.w && worldPt.y >= pos.y && worldPt.y <= pos.y + dim.h) {
      return node.id;
    }
  }
  return null;
}

function getNodeType(id: string): string {
  if (state.runs.find((r) => r.id === id)) return "design";
  const bn = state.boardNodes.find((n) => n.id === id);
  return bn?.nodeType ?? "design";
}

function hydrateBoardFromDom(): void {
  const container = $("storm-runs");
  if (!container) {
    state.runs = [];
    state.boardNodes = [];
    state.edges = [];
    state.lineage = new Map();
    state.positions = new Map();
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

  const boardNodeEls = Array.from(container.querySelectorAll<HTMLElement>(".board-node[data-node-id]"));
  const boardNodes = boardNodeEls.map((el) => {
    let content: Record<string, unknown> = {};
    try { content = JSON.parse(el.dataset.content ?? "{}"); } catch { /* noop */ }
    return {
      id: el.dataset.nodeId ?? "",
      nodeType: (el.dataset.nodeType ?? "entropy") as BoardNode["nodeType"],
      positionX: parseFloat(el.dataset.positionX ?? "0"),
      positionY: parseFloat(el.dataset.positionY ?? "0"),
      content,
      locked: el.dataset.locked === "true",
    };
  });
  state.boardNodes = boardNodes;

  const previousPositions = state.positions;
  const nextPositions = new Map<string, Point>();

  runs.forEach((run, index) => {
    const existing = previousPositions.get(run.id);
    if (existing) {
      nextPositions.set(run.id, existing);
      return;
    }
    assignPosition(run, index, lineage.get(run.id), nextPositions);
  });

  boardNodes.forEach((node) => {
    nextPositions.set(node.id, { x: node.positionX, y: node.positionY });
  });

  state.positions = nextPositions;

  // Hydrate edges
  const edgesScript = container.querySelector<HTMLScriptElement>("#board-edges-data");
  if (edgesScript?.textContent) {
    try { state.edges = JSON.parse(edgesScript.textContent); } catch { state.edges = []; }
  } else {
    state.edges = [];
  }

  reportClientEvent(
    "board_hydrated",
    {
      runCount: runs.length,
      boardNodeCount: state.boardNodes.length,
      edgeCount: state.edges.length,
      activeRunId: state.activeRunId,
      focusedRunId: state.focusedRunId,
    },
    { cooldownMs: 4000 },
  );

  if (state.activeRunId && !getRun(state.activeRunId)) state.activeRunId = null;
  if (state.activeNodeId && !state.boardNodes.find((n) => n.id === state.activeNodeId)) state.activeNodeId = null;
  if (state.focusedRunId && !getRun(state.focusedRunId)) state.focusedRunId = null;

  applyUrlState();
  renderRuns();
  renderBoardNodes();
  updateGenerateInputs();
  renderInspector();
  renderFocus();


  const latestRunId = getBoundValue("storm-latest-run-id").trim();
  if (latestRunId && getRun(latestRunId)) {
    clearDraftContext({ keepStatus: true });
    hideComposer({ preserveDraft: true });
    setActiveRun(latestRunId, { sync: true });
    centerRunInView(latestRunId);
    setBoundValue("storm-latest-run-id", "");
  }

  updateBoardTransform();
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
  });
  renderConnections();
}

function renderBoardNodes(): void {
  const container = $("storm-runs");
  if (!container) return;

  const nodesById = new Map(
    Array.from(container.querySelectorAll<HTMLElement>(".board-node[data-node-id]"))
      .map((el) => [el.dataset.nodeId ?? "", el]),
  );

  state.boardNodes.forEach((node) => {
    const pt = state.positions.get(node.id) ?? { x: node.positionX, y: node.positionY };
    const el = nodesById.get(node.id);
    if (!el) return;
    el.classList.toggle("is-active", node.id === state.activeNodeId);
    el.classList.toggle("is-locked", node.locked);
    el.style.transform = `translate(${pt.x}px, ${pt.y}px)`;
  });
}

function renderInspector(): void {
  const panel = $("storm-inspector");
  const title = $("active-run-title");
  const summary = $("storm-summary");
  const prompt = $("inspector-prompt");
  const notes = $("storm-notes");
  const created = $("inspector-created");
  const statusLabel = $("inspector-status-label");
  const details = $("inspector-details");
  const seedCard = $("inspector-seed");
  const notesCard = $("inspector-notes");
  const iframe = $("storm-preview") as HTMLIFrameElement | null;
  const fork = $("inspector-fork") as HTMLButtonElement | null;
  const fs = $("inspector-fullscreen") as HTMLButtonElement | null;
  const run = getRun(state.activeRunId);
  if (!panel || !title || !summary || !prompt || !notes || !created || !statusLabel || !details || !seedCard || !notesCard || !iframe || !fork || !fs) return;
  if (!run) {
    panel.classList.add("is-empty");
    title.textContent = "Select an artifact";
    summary.textContent = "";
    details.hidden = true;
    seedCard.hidden = true;
    notesCard.hidden = true;
    setIframeSource(iframe, null);
    fork.disabled = fs.disabled = true;
    return;
  }
  panel.classList.remove("is-empty");
  title.textContent = run.title;
  summary.textContent = run.summary;
  details.hidden = false;
  statusLabel.textContent = run.submitted ? "Submitted" : "Draft";
  created.textContent = new Date(run.createdAt).toLocaleString();
  seedCard.hidden = false;
  prompt.textContent = run.prompt;
  notesCard.hidden = !run.assistantSummary;
  notes.textContent = run.assistantSummary || "";
  setIframeSource(iframe, run.previewUrl);
  fork.disabled = fs.disabled = false;
}


function renderFocus(): void {
  const overlay = $("storm-focus");
  const frame = $("storm-focus-preview") as HTMLIFrameElement | null;
  const title = $("storm-focus-title");
  const run = getRun(state.focusedRunId);
  if (!overlay || !frame || !title) return;
  if (!run) {
    overlay.hidden = true;
    overlay.setAttribute("aria-hidden", "true");
    setIframeSource(frame, null);
    return;
  }
  overlay.hidden = false;
  overlay.setAttribute("aria-hidden", "false");
  setIframeSource(frame, run.previewUrl);
  title.textContent = run.title;
}

// ─── Event bindings ───

function bindCanvasInteractions(): void {
  const canvas = $("storm-canvas");
  if (!canvas) return;

  canvas.addEventListener("wheel", (e) => {
    stopBoardViewAnimation();
    e.preventDefault();
    const deltaX = normalizeWheelDelta(e.deltaX, e.deltaMode);
    const deltaY = normalizeWheelDelta(e.deltaY, e.deltaMode);
    if (e.metaKey || e.ctrlKey) {
      zoomBoardAtPoint(canvas, e.clientX, e.clientY, deltaY);
      return;
    }

    let panX = deltaX;
    let panY = deltaY;
    if (e.shiftKey && Math.abs(deltaX) < 1) {
      panX = deltaY;
      panY = 0;
    }

    state.pan = {
      x: state.pan.x - panX,
      y: state.pan.y - panY,
    };
    updateBoardTransform();
  }, { passive: false });

  canvas.addEventListener("pointerdown", (e) => {
    stopBoardViewAnimation();
    if (e.button === 2) return;
    if (((e.target as HTMLElement).closest(".storm-node") || (e.target as HTMLElement).closest(".board-node")) && !state.spacePanHeld && e.button !== 1) return;
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

  canvas.addEventListener("pointercancel", (e) => {
    if (state.pointerState?.pointerId === e.pointerId) state.pointerState = null;
  });

  window.addEventListener("keydown", (e) => {
    if (e.code !== "Space" || isEditableTarget(e.target)) return;
    state.spacePanHeld = true;
    e.preventDefault();
  });

  window.addEventListener("keyup", (e) => {
    if (e.code !== "Space") return;
    state.spacePanHeld = false;
  });

  window.addEventListener("blur", () => {
    state.spacePanHeld = false;
  });
}

let pendingWireSource: { id: string; type: string; anchor: AnchorPoint; dropWorld: Point } | null = null;

function bindNodeInteractions(): void {
  const container = $("storm-runs");
  if (!container) return;

  let wireJustCompleted = false;

  container.addEventListener("click", (e) => {
    if (state.spacePanHeld) return;
    if (wireJustCompleted) { wireJustCompleted = false; return; }
    const target = e.target as HTMLElement;
    if (target.closest(".edge-handle")) return;
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
    if (state.spacePanHeld) return;
    const node = (e.target as HTMLElement).closest<HTMLElement>(".storm-node");
    if (node?.dataset.runId) openFullscreen(node.dataset.runId);
  });

  container.addEventListener("pointerdown", (e) => {
    if (state.spacePanHeld) return;
    const target = e.target as HTMLElement;
    if (target.closest("[data-run-action]")) return;

    // Wire mode: dragging from a dynamic edge handle
    if (target.closest(".edge-handle")) {
      const handleEl = target.closest<HTMLElement>(".edge-handle");
      const stormNode = target.closest<HTMLElement>(".storm-node");
      const boardNode = target.closest<HTMLElement>(".board-node");
      const nodeEl = stormNode ?? boardNode;
      const nodeId = stormNode?.dataset.runId ?? boardNode?.dataset.nodeId;
      if (!nodeEl || !nodeId || !handleEl) return;
      e.preventDefault();
      e.stopPropagation();
      const side = (handleEl.dataset.side ?? "bottom") as AnchorSide;
      const t = parseFloat(handleEl.dataset.t ?? "0.5");
      const anchorPos = getAnchorWorldPos(nodeId, side, t);
      const srcType = getNodeType(nodeId);
      const sourceAnchor: AnchorPoint = { side, t, worldX: anchorPos.x, worldY: anchorPos.y };
      state.pointerState = { mode: "wire", pointerId: e.pointerId, sourceRunId: nodeId, sourceType: srcType, sourceAnchor, startWorld: { x: anchorPos.x, y: anchorPos.y }, currentWorld: { x: anchorPos.x, y: anchorPos.y }, targetRunId: null, targetType: null, targetAnchor: null };
      nodeEl.classList.add("is-wire-source");
      document.body.classList.add("is-wiring");
      nodeEl.setPointerCapture(e.pointerId);
      return;
    }

    // Edge proximity wire initiation: pointerdown near node edge
    {
      const world = clientToWorld(e.clientX, e.clientY);
      // Check all nodes for edge proximity
      const allNodeIds = [...state.runs.map((r) => r.id), ...state.boardNodes.map((n) => n.id)];
      for (const nid of allNodeIds) {
        const anchor = getEdgeProximity(nid, world);
        if (anchor) {
          const nodeEl = container.querySelector<HTMLElement>(`.storm-node[data-run-id="${nid}"]`)
            ?? container.querySelector<HTMLElement>(`.board-node[data-node-id="${nid}"]`);
          if (!nodeEl) continue;
          e.preventDefault();
          e.stopPropagation();
          const srcType = getNodeType(nid);
          state.pointerState = { mode: "wire", pointerId: e.pointerId, sourceRunId: nid, sourceType: srcType, sourceAnchor: anchor, startWorld: { x: anchor.worldX, y: anchor.worldY }, currentWorld: { x: anchor.worldX, y: anchor.worldY }, targetRunId: null, targetType: null, targetAnchor: null };
          nodeEl.classList.add("is-wire-source");
          document.body.classList.add("is-wiring");
          nodeEl.setPointerCapture(e.pointerId);
          return;
        }
      }
    }

    const node = target.closest<HTMLElement>(".storm-node");
    const runId = node?.dataset.runId;
    if (!node || !runId) return;
    const pt = state.positions.get(runId);
    if (!pt) return;
    state.pointerState = { mode: "drag", pointerId: e.pointerId, runId, startClient: { x: e.clientX, y: e.clientY }, startPos: { ...pt }, moved: false };
    node.setPointerCapture(e.pointerId);
  });

  container.addEventListener("pointermove", (e) => {
    if (!state.pointerState || state.pointerState.pointerId !== e.pointerId) return;

    if (state.pointerState.mode === "wire") {
      const world = clientToWorld(e.clientX, e.clientY);
      state.pointerState.currentWorld = world;
      const hit = hitTestNode(world, state.pointerState.sourceRunId);

      // Update target highlight and target anchor
      if (hit !== state.pointerState.targetRunId) {
        if (state.pointerState.targetRunId) {
          const prevEl = container.querySelector(`.storm-node[data-run-id="${state.pointerState.targetRunId}"]`)
            ?? container.querySelector(`.board-node[data-node-id="${state.pointerState.targetRunId}"]`);
          prevEl?.classList.remove("is-wire-target", "is-wire-invalid");
        }
        state.pointerState.targetRunId = hit;
        state.pointerState.targetType = hit ? getNodeType(hit) : null;
        if (hit) {
          const hitEl = container.querySelector(`.storm-node[data-run-id="${hit}"]`)
            ?? container.querySelector(`.board-node[data-node-id="${hit}"]`);
          const hitType = getNodeType(hit);
          const valid = canConnect(state.pointerState.sourceType, hitType);
          hitEl?.classList.add(valid ? "is-wire-target" : "is-wire-invalid");
        }
      }
      // Track target anchor for snapping
      if (hit) {
        state.pointerState.targetAnchor = getEdgeProximity(hit, world);
      } else {
        state.pointerState.targetAnchor = null;
      }
      renderWire();
      return;
    }

    if (state.pointerState.mode === "drag-board-node") {
      const dx = (e.clientX - state.pointerState.startClient.x) / state.scale;
      const dy = (e.clientY - state.pointerState.startClient.y) / state.scale;
      if (Math.abs(dx) > 4 || Math.abs(dy) > 4) state.pointerState.moved = true;
      const next = { x: state.pointerState.startPos.x + dx, y: state.pointerState.startPos.y + dy };
      state.positions.set(state.pointerState.nodeId, next);
      const el = container.querySelector<HTMLElement>(`.board-node[data-node-id="${state.pointerState.nodeId}"]`);
      if (el) el.style.transform = `translate(${next.x}px, ${next.y}px)`;
      renderConnections();
      return;
    }

    if (state.pointerState.mode !== "drag") return;
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
    if (!state.pointerState || state.pointerState.pointerId !== e.pointerId) return;

    if (state.pointerState.mode === "wire") {
      const { sourceRunId, sourceType, sourceAnchor, targetRunId, targetType, targetAnchor } = state.pointerState;
      // Clean up wire visuals
      const srcEl = container.querySelector(`.storm-node[data-run-id="${sourceRunId}"]`)
        ?? container.querySelector(`.board-node[data-node-id="${sourceRunId}"]`);
      srcEl?.classList.remove("is-wire-source");
      if (targetRunId) {
        const tgtEl = container.querySelector(`.storm-node[data-run-id="${targetRunId}"]`)
          ?? container.querySelector(`.board-node[data-node-id="${targetRunId}"]`);
        tgtEl?.classList.remove("is-wire-target", "is-wire-invalid");
      }
      document.body.classList.remove("is-wiring");
      removeEdgeHandles();
      state.pointerState = null;
      wireJustCompleted = true;
      renderWire();

      if (targetRunId && targetRunId !== sourceRunId) {
        const resolvedTargetType = targetType ?? getNodeType(targetRunId);
        if (!canConnect(sourceType, resolvedTargetType)) {
          // Invalid connection — ignore silently
        } else {
          const finalTargetAnchor = targetAnchor ?? { side: "top" as AnchorSide, t: 0.5, worldX: 0, worldY: 0 };
          createBoardEdge(
            sourceRunId, sourceType, targetRunId, resolvedTargetType,
            sourceAnchor, finalTargetAnchor
          );
        }
      } else if (!targetRunId) {
        // Dropped on empty canvas: open radial menu to pick node type, then auto-connect
        const dropWorld = clientToWorld(e.clientX, e.clientY);
        pendingWireSource = { id: sourceRunId, type: sourceType, anchor: sourceAnchor, dropWorld };
        state.activeRunId = null;
        state.activeNodeId = null;
        openRadialMenu(e.clientX, e.clientY);
      }
      (e.target as HTMLElement).closest<HTMLElement>(".storm-node, .board-node")?.releasePointerCapture(e.pointerId);
      return;
    }

    if (state.pointerState.mode === "drag-board-node") {
      const { nodeId } = state.pointerState;
      (e.target as HTMLElement).closest<HTMLElement>(".board-node")?.releasePointerCapture(e.pointerId);
      const pos = state.positions.get(nodeId);
      if (pos && state.pointerState.moved) {
        fetch(`/nodes/${nodeId}/position`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          credentials: "include",
          body: JSON.stringify({ position_x: pos.x, position_y: pos.y }),
        }).catch(() => {});
      }
      state.pointerState = null;
      updateBoardTransform();
      return;
    }

    if (state.pointerState.mode !== "drag") return;
    (e.target as HTMLElement).closest<HTMLElement>(".storm-node")?.releasePointerCapture(e.pointerId);
    state.pointerState = null;
    updateBoardTransform();
  });
}

// ─── Board node interactions (entropy, etc.) ───

function createBoardNode(nodeType: string, worldPos: Point, opts?: CreateBoardNodeOptions): void {
  const form = $("board-node-create-form") as HTMLFormElement | null;
  const typeInput = $("board-node-create-type") as HTMLInputElement | null;
  const xInput = $("board-node-create-x") as HTMLInputElement | null;
  const yInput = $("board-node-create-y") as HTMLInputElement | null;
  const sourceIdInput = $("board-node-create-source-id") as HTMLInputElement | null;
  const sourceTypeInput = $("board-node-create-source-type") as HTMLInputElement | null;
  const sourceAnchorSideInput = $("board-node-create-source-anchor-side") as HTMLInputElement | null;
  const sourceAnchorTInput = $("board-node-create-source-anchor_t") as HTMLInputElement | null;
  const submit = $("board-node-create-submit") as HTMLButtonElement | null;
  if (!form || !typeInput || !xInput || !yInput || !sourceIdInput || !sourceTypeInput || !sourceAnchorSideInput || !sourceAnchorTInput || !submit) {
    console.error("Board node create form is missing.");
    return;
  }

  const hasSource = Boolean(opts?.sourceId && opts?.sourceType);
  typeInput.value = nodeType;
  xInput.value = String(worldPos.x);
  yInput.value = String(worldPos.y);
  sourceIdInput.value = opts?.sourceId ?? "";
  sourceTypeInput.value = opts?.sourceType ?? "";
  sourceAnchorSideInput.value = opts?.sourceAnchor?.side ?? "";
  sourceAnchorTInput.value = opts?.sourceAnchor ? String(opts.sourceAnchor.t) : "";

  sourceIdInput.disabled = !hasSource;
  sourceTypeInput.disabled = !hasSource;
  sourceAnchorSideInput.disabled = !hasSource || !opts?.sourceAnchor?.side;
  sourceAnchorTInput.disabled = !hasSource || !opts?.sourceAnchor;

  if (typeof form.requestSubmit === "function") {
    form.requestSubmit(submit);
    return;
  }

  form.dispatchEvent(new Event("submit", { bubbles: true, cancelable: true }));
}

async function createBoardEdge(
  sourceId: string, sourceType: string, targetId: string, targetType: string,
  sourceAnchor?: AnchorPoint, targetAnchor?: AnchorPoint
): Promise<void> {
  try {
    const resp = await fetch("/edges", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({
        source_id: sourceId, source_type: sourceType,
        target_id: targetId, target_type: targetType,
        source_anchor_side: sourceAnchor?.side ?? null,
        source_anchor_t: sourceAnchor?.t ?? null,
        target_anchor_side: targetAnchor?.side ?? null,
        target_anchor_t: targetAnchor?.t ?? null,
      }),
    });
    if (!resp.ok) { console.error("Failed to create edge:", resp.statusText); return; }
    const edge: BoardEdge = await resp.json();
    state.edges.push(edge);
    renderConnections();
    updateGenerateInputs();
  } catch (err) {
    console.error("Failed to create edge:", err);
  }
}

// ─── Image popover ───

function showImagePopover(src: string): void {
  const existing = document.getElementById("image-popover");
  existing?.remove();

  const overlay = document.createElement("div");
  overlay.id = "image-popover";
  overlay.className = "image-popover-overlay";
  overlay.innerHTML = `<img src="${src}" class="image-popover-img" />`;
  overlay.addEventListener("click", () => overlay.remove());
  document.addEventListener("keydown", function handler(e) {
    if (e.key === "Escape") { overlay.remove(); document.removeEventListener("keydown", handler); }
  });
  document.body.appendChild(overlay);
}

// ─── Edge connection rules ───

const VALID_CONNECTIONS: Record<string, string[]> = {
  entropy: ["generate"],
  user_input: ["generate"],
  design: ["generate"],
  generate: [],  // generate outputs are auto-created by the run handler
};

function canConnect(sourceType: string, targetType: string): boolean {
  return VALID_CONNECTIONS[sourceType]?.includes(targetType) ?? false;
}

// ─── Delete board node ───

async function deleteBoardNode(nodeId: string): Promise<void> {
  try {
    const resp = await fetch(`/nodes/${nodeId}`, { method: "DELETE", credentials: "include" });
    if (!resp.ok) {
      console.error("Failed to delete board node:", resp.status, resp.statusText);
      return;
    }
  } catch (err) {
    console.error("Failed to delete board node:", err);
    return;
  }

  state.boardNodes = state.boardNodes.filter((n) => n.id !== nodeId);
  state.edges = state.edges.filter((e) => e.sourceId !== nodeId && e.targetId !== nodeId);
  state.positions.delete(nodeId);
  if (state.activeNodeId === nodeId) state.activeNodeId = null;

  const el = document.querySelector(`.board-node[data-node-id="${nodeId}"]`);
  el?.remove();
  renderConnections();
  updateGenerateInputs();
}

async function deleteRun(runId: string): Promise<void> {
  try {
    const resp = await fetch(`/storms/${runId}`, { method: "DELETE", credentials: "include" });
    if (!resp.ok) {
      console.error("Failed to delete run:", resp.status, resp.statusText);
      return;
    }
  } catch (err) {
    console.error("Failed to delete run:", err);
    return;
  }

  state.runs = state.runs.filter((run) => run.id !== runId);
  state.edges = state.edges.filter((edge) => edge.sourceId !== runId && edge.targetId !== runId);
  state.positions.delete(runId);
  state.lineage.delete(runId);
  state.lineage.forEach((parents, childId) => {
    const nextParents = parents.filter((parentId) => parentId !== runId);
    if (nextParents.length !== parents.length) state.lineage.set(childId, nextParents);
  });
  if (state.activeRunId === runId) state.activeRunId = null;
  if (state.focusedRunId === runId) state.focusedRunId = null;
  if (state.combineSourceId === runId) state.combineSourceId = null;

  const el = document.querySelector(`.storm-node[data-run-id="${runId}"]`);
  el?.remove();
  renderRuns();
  renderConnections();
  renderInspector();
  renderFocus();

  syncUrl(false);
}

// ─── Dynamic edge handles ───

let activeEdgeHandle: HTMLElement | null = null;

function showEdgeHandle(nodeEl: HTMLElement, anchor: AnchorPoint): void {
  removeEdgeHandles();
  const handle = document.createElement("div");
  handle.className = "edge-handle is-visible";
  handle.dataset.side = anchor.side;
  handle.dataset.t = String(anchor.t);

  const { w, h } = getNodeDimensions(nodeEl.dataset.runId ?? nodeEl.dataset.nodeId ?? "");
  switch (anchor.side) {
    case "top": handle.style.left = `${anchor.t * w - 5}px`; handle.style.top = "-5px"; break;
    case "bottom": handle.style.left = `${anchor.t * w - 5}px`; handle.style.bottom = "-5px"; break;
    case "left": handle.style.top = `${anchor.t * h - 5}px`; handle.style.left = "-5px"; break;
    case "right": handle.style.top = `${anchor.t * h - 5}px`; handle.style.right = "-5px"; break;
  }

  // Append to the shell element (first child)
  const shell = nodeEl.firstElementChild as HTMLElement;
  if (shell) shell.appendChild(handle);
  activeEdgeHandle = handle;
}

function removeEdgeHandles(): void {
  if (activeEdgeHandle) {
    activeEdgeHandle.remove();
    activeEdgeHandle = null;
  }
}

function updateGenerateInputs(): void {
  for (const node of state.boardNodes) {
    if (node.nodeType !== "generate") continue;
    const inputsEl = document.getElementById(`gen-inputs-${node.id}`);
    if (!inputsEl) continue;

    const connectedEdges = state.edges.filter((e) => e.targetId === node.id);
    if (connectedEdges.length === 0) {
      inputsEl.innerHTML = '<span class="generate-placeholder">Wire inputs here</span>';
      continue;
    }

    inputsEl.innerHTML = connectedEdges.map((edge) => {
      const srcNode = state.boardNodes.find((n) => n.id === edge.sourceId);
      const srcRun = state.runs.find((r) => r.id === edge.sourceId);
      let label = edge.sourceType;
      if (srcNode?.nodeType === "entropy") label = (srcNode.content.title as string) ?? "Entropy";
      else if (srcRun) label = srcRun.title;
      return `<span class="generate-input-chip">${escapeHtml(label)}</span>`;
    }).join("");
  }
}

function bindEdgeHover(): void {
  const canvas = $("storm-canvas");
  if (!canvas) return;

  canvas.addEventListener("pointermove", (e) => {
    if (state.pointerState) return; // don't show handles while dragging/wiring
    const world = clientToWorld(e.clientX, e.clientY);
    const container = $("storm-runs");
    if (!container) return;

    // Check all nodes for edge proximity
    const allNodes = [
      ...state.runs.map((r) => ({ id: r.id, sel: `.storm-node[data-run-id="${r.id}"]` })),
      ...state.boardNodes.map((n) => ({ id: n.id, sel: `.board-node[data-node-id="${n.id}"]` })),
    ];

    for (const { id, sel } of allNodes) {
      const anchor = getEdgeProximity(id, world);
      if (anchor) {
        const nodeEl = container.querySelector<HTMLElement>(sel);
        if (nodeEl) showEdgeHandle(nodeEl, anchor);
        return;
      }
    }
    removeEdgeHandles();
  }, { passive: true });
}

function bindBoardNodeInteractions(): void {
  const container = $("storm-runs");
  if (!container) return;

  container.addEventListener("click", (e) => {
    const target = e.target as HTMLElement;
    const boardNode = target.closest<HTMLElement>(".board-node");
    if (!boardNode) return;
    const nodeId = boardNode.dataset.nodeId;
    if (!nodeId) return;

    // Reroll/lock/run buttons are handled by Datastar or JS
    if (target.closest(".entropy-btn")) return;
    if (target.closest(".edge-handle")) return;
    if (target.closest(".input-body")) return; // let contenteditable handle clicks
    if (target.closest(".generate-run-btn")) return; // handled by Datastar @post

    // Select the node
    state.activeNodeId = nodeId;
    state.activeRunId = null;
    renderBoardNodes();
    renderRuns();
  });

  // Save input node text on blur
  container.addEventListener("focusout", (e) => {
    const target = e.target as HTMLElement;
    if (!target.classList.contains("input-body")) return;
    const boardNode = target.closest<HTMLElement>(".board-node");
    const nodeId = boardNode?.dataset.nodeId;
    if (!nodeId) return;
    const text = target.textContent?.trim() ?? "";
    const node = state.boardNodes.find((n) => n.id === nodeId);
    if (node) node.content.text = text;
    // Persist to backend
    fetch(`/nodes/${nodeId}/content`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      credentials: "include",
      body: JSON.stringify({ text }),
    }).catch(() => {});
  });

  // Paste images into input nodes
  container.addEventListener("paste", (e) => {
    const target = e.target as HTMLElement;
    if (!target.classList.contains("input-body")) return;
    const items = e.clipboardData?.items;
    if (!items) return;

    for (const item of Array.from(items)) {
      if (!item.type.startsWith("image/")) continue;
      e.preventDefault();
      const file = item.getAsFile();
      if (!file) continue;

      const reader = new FileReader();
      reader.onload = () => {
        const dataUrl = reader.result as string;
        const img = document.createElement("img");
        img.src = dataUrl;
        img.className = "input-image-thumb";
        img.title = "Double-click to enlarge";
        target.appendChild(img);

        // Persist content with images
        const boardNode = target.closest<HTMLElement>(".board-node");
        const nodeId = boardNode?.dataset.nodeId;
        if (nodeId) {
          const node = state.boardNodes.find((n) => n.id === nodeId);
          const images = ((node?.content.images as string[]) ?? []).concat(dataUrl);
          if (node) node.content.images = images;
          fetch(`/nodes/${nodeId}/content`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            credentials: "include",
            body: JSON.stringify({ text: target.textContent?.trim() ?? "", images }),
          }).catch(() => {});
        }
      };
      reader.readAsDataURL(file);
    }
  });

  // Double-click image thumbnail to show popover
  container.addEventListener("dblclick", (e) => {
    const target = e.target as HTMLElement;
    if (target.tagName === "IMG" && target.classList.contains("input-image-thumb")) {
      e.preventDefault();
      e.stopPropagation();
      showImagePopover((target as HTMLImageElement).src);
    }
  });

  container.addEventListener("pointerdown", (e) => {
    if (state.spacePanHeld) return;
    const target = e.target as HTMLElement;
    if (target.closest("[data-node-action]")) return;
    if (target.closest(".edge-handle")) return;
    if (target.closest(".input-body")) return; // don't drag while editing text

    const boardNode = target.closest<HTMLElement>(".board-node");
    if (!boardNode) return;
    const nodeId = boardNode.dataset.nodeId;
    if (!nodeId) return;
    const pt = state.positions.get(nodeId);
    if (!pt) return;
    state.pointerState = { mode: "drag-board-node", pointerId: e.pointerId, nodeId, startClient: { x: e.clientX, y: e.clientY }, startPos: { ...pt }, moved: false };
    boardNode.setPointerCapture(e.pointerId);
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
      setStatus("Seed prompt required.");
      return;
    }
    setBoundValue("storm-latest-run-id", "");
    setStatus("Generating storm...");
    reportClientEvent("storm_generate_clicked", {
      promptLength: prompt.length,
      draftMode: state.draftContext?.mode ?? null,
      sourceIds: state.draftContext?.sourceIds ?? [],
    });
  });
  $("storm-clear-context")?.addEventListener("click", () => clearDraftContext());
  $("storm-composer-backdrop")?.addEventListener("click", () => hideComposer());

  // Fullscreen overlay
  $("storm-focus")?.addEventListener("click", (e) => {
    if ((e.target as HTMLElement).closest("[data-action='close-focus']")) closeFullscreen();
  });
  $("storm-focus-back")?.addEventListener("click", () => closeFullscreen());

  // Inspector actions

  // Keyboard — escape priority: radial > composer > fullscreen (popovers handled by Datastar)
  window.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      if (state.radialMenu.open) { closeRadialMenu(); e.preventDefault(); return; }
      if (!$("palette-panel")?.hidden) { closePalette(); e.preventDefault(); return; }
      const form = $("storm-form");
      if (form && !form.hidden) { hideComposer(); e.preventDefault(); return; }
      if (state.focusedRunId) closeFullscreen();
    }
    // Delete/Backspace to remove selected board node
    if ((e.key === "Delete" || e.key === "Backspace") && !isEditableTarget(e.target) && state.activeNodeId) {
      e.preventDefault();
      deleteBoardNode(state.activeNodeId);
    }
  });

  // History
  window.addEventListener("popstate", () => { applyUrlState(); renderRuns(); renderInspector(); renderFocus(); });
}

// ─── Command Palette ───

let paletteFilter = "all";

function openPalette(): void {
  const panel = $("palette-panel");
  const input = $("palette-search") as HTMLInputElement | null;
  if (!panel) return;
  panel.hidden = false;
  input?.focus();
  renderPaletteList();
}

function closePalette(): void {
  const panel = $("palette-panel");
  if (!panel) return;
  panel.hidden = true;
  const input = $("palette-search") as HTMLInputElement | null;
  if (input) input.value = "";
  paletteFilter = "all";
  panel.querySelectorAll(".palette-chip").forEach(c => c.classList.toggle("is-active", (c as HTMLElement).dataset.filter === "all"));
}

function getPaletteItems(): Array<{ id: string; type: string; label: string; detail: string }> {
  const items: Array<{ id: string; type: string; label: string; detail: string }> = [];

  for (const run of state.runs) {
    items.push({ id: run.id, type: "design", label: run.title, detail: run.summary });
  }
  for (const node of state.boardNodes) {
    if (node.nodeType === "entropy") {
      items.push({ id: node.id, type: "entropy", label: (node.content.title as string) ?? "Entropy", detail: (node.content.url as string) ?? "" });
    } else if (node.nodeType === "user_input") {
      items.push({ id: node.id, type: "user_input", label: "Input", detail: ((node.content.text as string) ?? "").slice(0, 80) });
    } else if (node.nodeType === "generate") {
      const inputCount = state.edges.filter(e => e.targetId === node.id).length;
      items.push({ id: node.id, type: "generate", label: "Generate", detail: `${inputCount} input${inputCount !== 1 ? "s" : ""} wired` });
    }
  }
  return items;
}

function renderPaletteList(): void {
  const list = $("palette-list");
  const input = $("palette-search") as HTMLInputElement | null;
  if (!list) return;

  const query = (input?.value ?? "").toLowerCase().trim();
  let items = getPaletteItems();

  if (paletteFilter !== "all") items = items.filter(i => i.type === paletteFilter);
  if (query) items = items.filter(i => i.label.toLowerCase().includes(query) || i.detail.toLowerCase().includes(query));

  if (items.length === 0) {
    list.innerHTML = '<div class="palette-empty">No nodes found.</div>';
    return;
  }

  list.innerHTML = items.map(item => `
    <button class="palette-item" data-palette-id="${item.id}" type="button">
      <span class="palette-type-dot palette-dot-${item.type}"></span>
      <div class="palette-item-text">
        <span class="palette-item-label">${escapeHtml(item.label)}</span>
        <span class="palette-item-detail">${escapeHtml(item.detail)}</span>
      </div>
    </button>
  `).join("");
}

function bindPalette(): void {
  const trigger = $("palette-trigger");
  const panel = $("palette-panel");
  const searchInput = $("palette-search") as HTMLInputElement | null;
  if (!trigger || !panel) return;

  trigger.addEventListener("click", () => {
    if (panel.hidden) openPalette(); else closePalette();
  });

  window.addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key === "k") {
      e.preventDefault();
      if (panel.hidden) openPalette(); else closePalette();
    }
  });

  searchInput?.addEventListener("input", () => renderPaletteList());

  panel.addEventListener("click", (e) => {
    const chip = (e.target as HTMLElement).closest<HTMLElement>(".palette-chip");
    if (chip?.dataset.filter) {
      paletteFilter = chip.dataset.filter;
      panel.querySelectorAll(".palette-chip").forEach(c => c.classList.toggle("is-active", (c as HTMLElement).dataset.filter === paletteFilter));
      renderPaletteList();
      return;
    }

    const item = (e.target as HTMLElement).closest<HTMLElement>("[data-palette-id]");
    const nodeId = item?.dataset.paletteId;
    if (!nodeId) return;

    const nodeType = getNodeType(nodeId);
    if (nodeType === "design") {
      setActiveRun(nodeId, { sync: true });
    } else {
      state.activeNodeId = nodeId;
      state.activeRunId = null;
    }
    centerRunInView(nodeId);
    closePalette();
  });

  document.addEventListener("pointerdown", (e) => {
    if (!panel.hidden && !panel.contains(e.target as Node) && !trigger.contains(e.target as Node)) {
      closePalette();
    }
  });
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
  if (pendingWireSource) {
    const wire = pendingWireSource;
    const worldPos = wire.dropWorld;
    const makeAndConnect = (nodeType: string) => {
      createBoardNode(nodeType, worldPos, {
        sourceId: wire.id,
        sourceType: wire.type,
        sourceAnchor: wire.anchor,
      });
      pendingWireSource = null;
    };
    // Only show valid connection targets for this source type
    const validTargets = VALID_CONNECTIONS[wire.type] ?? [];
    return [
      { id: "generate", angle: 0, label: "Generate", icon: "✦", variant: "primary" as const, disabled: !validTargets.includes("generate"), action: () => { makeAndConnect("generate"); } },
      { id: "entropy", angle: 120, label: "Entropy", icon: "🎲", disabled: !validTargets.includes("entropy"), action: () => { makeAndConnect("entropy"); } },
      { id: "input", angle: 240, label: "Input", icon: "✎", disabled: !validTargets.includes("user_input"), action: () => { makeAndConnect("user_input"); } },
    ];
  }
  if (!state.activeRunId && !state.activeNodeId) {
    const worldPos = clientToWorld(state.radialMenu.position.x, state.radialMenu.position.y);
    return [
      { id: "generate", angle: 0, label: "Generate", icon: "✦", variant: "primary", action: () => { createBoardNode("generate", worldPos); } },
      { id: "entropy", angle: 120, label: "Entropy", icon: "🎲", action: () => { createBoardNode("entropy", worldPos); } },
      { id: "input", angle: 240, label: "Input", icon: "✎", action: () => { createBoardNode("user_input", worldPos); } },
    ];
  }
  if (state.activeRunId) {
    return [
      { id: "fork", angle: 0, label: "Fork", icon: "⑂", action: () => { if (state.activeRunId) { handleRunAction(state.activeRunId, "fork"); showComposer(); } } },
      { id: "fullscreen", angle: 90, label: "Fullscreen", icon: "⛶", action: () => { if (state.activeRunId) openFullscreen(state.activeRunId); } },
      { id: "combine", angle: 180, label: "Combine", icon: "⊕", action: () => { if (state.activeRunId) beginCombine(state.activeRunId); } },
      { id: "generate", angle: 270, label: "Generate", icon: "✦", variant: "primary", action: showComposer },
      { id: "delete", angle: 315, label: "Delete", icon: "✕", variant: "danger" as const, action: () => { if (state.activeRunId) void deleteRun(state.activeRunId); } },
    ];
  }
  // Board node selected
  const activeNode = state.activeNodeId;
  return [
    { id: "delete", angle: 0, label: "Delete", icon: "✕", variant: "danger" as const, action: () => { if (activeNode) deleteBoardNode(activeNode); } },
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
  pendingWireSource = null;
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
    const stormNode = (e.target as HTMLElement).closest<HTMLElement>(".storm-node");
    const boardNode = (e.target as HTMLElement).closest<HTMLElement>(".board-node");
    if (stormNode?.dataset.runId) {
      state.activeNodeId = null;
      setActiveRun(stormNode.dataset.runId);
    } else if (boardNode?.dataset.nodeId) {
      state.activeRunId = null;
      state.activeNodeId = boardNode.dataset.nodeId;
      renderBoardNodes();
      renderRuns();
    } else {
      state.activeRunId = null;
      state.activeNodeId = null;
    }
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
    const relevant = mutations.some((mutation) =>
      mutation.type === "childList"
      && [...mutation.addedNodes, ...mutation.removedNodes].some((node) =>
        node instanceof HTMLElement
        && (node.id === "storm-runs" || node.classList.contains("storm-node") || node.classList.contains("board-node"))
      )
    );
    if (relevant) {
      reportClientEvent(
        "board_mutation",
        {
          mutationCount: mutations.length,
          runCount: $("storm-runs")?.querySelectorAll(".storm-node[data-run-id]").length ?? 0,
        },
        { cooldownMs: 3000 },
      );
      hydrateBoardFromDom();
    }
  });
  observer.observe(board, { childList: true });
}

function bindStormApp(): void {
  if (getConfig().currentPath !== "/app") return;
  setAvatarInitials();

  renderDraftContext();
  initBoardBackground();
  updateBoardTransform();
  bindBoardObserver();
  bindCanvasInteractions();
  bindNodeInteractions();
  bindBoardNodeInteractions();
  bindEdgeHover();
  bindAppChrome();
  bindPalette();
  bindRadialMenu();
  hydrateBoardFromDom();

  const focus = $("storm-focus-preview") as HTMLIFrameElement | null;
  focus?.addEventListener("load", () => {
    reportClientEvent(
      "preview_loaded",
      { frame: "focus", src: focus.getAttribute("src") },
      { cooldownMs: 5000 },
    );
  });
  focus?.addEventListener("error", () => {
    reportClientEvent(
      "preview_failed",
      { frame: "focus", src: focus.getAttribute("src") },
      { message: "Fullscreen preview failed to load." },
    );
  });
}

function bindDiagnostics(): void {
  window.addEventListener("error", (event) => {
    reportClientEvent(
      "window_error",
      {
        message: event.message,
        source: event.filename,
        line: event.lineno,
        column: event.colno,
        stack: event.error instanceof Error ? event.error.stack ?? null : null,
      },
      { message: event.message, cooldownMs: 2000 },
    );
  });

  window.addEventListener("unhandledrejection", (event) => {
    let reason = "";
    try {
      reason = typeof event.reason === "string" ? event.reason : JSON.stringify(event.reason);
    } catch {
      reason = String(event.reason);
    }
    reportClientEvent(
      "unhandled_rejection",
      { reason },
      { cooldownMs: 2000 },
    );
  });

  if ("PerformanceObserver" in window) {
    try {
      const observer = new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) {
          reportClientEvent(
            "long_task",
            {
              name: entry.name,
              duration: Math.round(entry.duration),
              startTime: Math.round(entry.startTime),
            },
            { cooldownMs: 1000 },
          );
        }
      });
      observer.observe({ entryTypes: ["longtask"] });
    } catch {
      // Unsupported browser or entry type.
    }
  }
}

// ─── Bootstrap ───

async function bootstrap(): Promise<void> {
  bindDiagnostics();
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
