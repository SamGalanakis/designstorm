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
  title: string;
  summary: string;
  createdAt: string;
  sessionId?: string | null;
};

type StudioSnapshotResponse = {
  sessionListHtml: string;
  messagesHtml: string;
  galleryHtml: string;
  referenceListHtml: string;
  activeSessionTitle: string;
  activeSessionUpdatedLabel: string;
  status: string;
};

type CreateDesignSessionResponse = {
  sessionId: string;
  location: string;
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

type ClaudeStartResponse = {
  authUrl: string;
};

type ClaudeExchangeResponse = {
  status: "error" | "connected";
  message?: string | null;
  authUrl?: string | null;
};

type MentionItem = {
  handle: string;
  label: string;
  kind: "design" | "text" | "link" | "image";
  search: string;
  sessionId?: string | null;
};

type MentionState = {
  start: number;
  end: number;
  query: string;
  items: MentionItem[];
} | null;

declare global {
  interface Window {
    Clerk?: new (publishableKey: string) => ClerkLike;
    designstormAuth: {
      signIn(): Promise<void>;
      signUp(): Promise<void>;
      signOut(): Promise<void>;
    };
    designstormSettings: {
      connectProvider(kind: "codex" | "claude"): Promise<void>;
      submitClaudeCode(): Promise<void>;
    };
  }
}

let clerk: ClerkLike | null = null;
let isSyncing = false;
let authPollTimer: number | null = null;
let mentionState: MentionState = null;
let designMentionItems: MentionItem[] = [];
let pollTimer: number | null = null;
let lastSavedTitle = "";

const selectedReferences = new Map<string, MentionItem>();
let draftIteratesOnId: string | null = null;
let draftIteratesOnLabel: string | null = null;

function $(id: string): HTMLElement | null {
  return document.getElementById(id);
}

function getConfig(): AppConfig {
  const el = $("app-config");
  if (!el?.textContent) throw new Error("Missing app config.");
  return JSON.parse(el.textContent) as AppConfig;
}

function escapeHtml(input: string): string {
  return input
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll("\"", "&quot;")
    .replaceAll("'", "&#39;");
}

function getActiveSessionId(): string {
  const root = $("studio-app");
  const sessionId = root?.getAttribute("data-active-session-id") ?? "";
  if (!sessionId) throw new Error("Missing active session id.");
  return sessionId;
}

function setActiveSessionId(sessionId: string): void {
  $("studio-app")?.setAttribute("data-active-session-id", sessionId);
}

function setStatus(message: string): void {
  const status = $("studio-status");
  if (status) status.textContent = message;
}

function setAvatarInitials(): void {
  const fallback = $("avatar-fallback");
  const name = fallback?.getAttribute("data-name") ?? "";
  const parts = name.trim().split(/\s+/).filter(Boolean);
  const initials = parts.slice(0, 2).map((part) => part[0]?.toUpperCase() ?? "").join("") || "DS";
  if (fallback) fallback.textContent = initials;
}

function redirectToApp(): void {
  if (window.location.pathname !== "/app") window.location.href = "/app";
}

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
    const response = await fetch("/auth/session", {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ token }),
    });
    return response.ok;
  } finally {
    isSyncing = false;
  }
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
  await fetch("/auth/logout", { method: "POST", credentials: "include" });
  if (instance) await instance.signOut();
  window.location.href = "/";
}

function setProviderStatus(message: string, tone: "muted" | "error" | "success" = "muted"): void {
  const element = $("provider-connect-status");
  if (!element) return;
  element.textContent = message;
  element.classList.remove("is-error", "is-success");
  if (tone === "error") element.classList.add("is-error");
  if (tone === "success") element.classList.add("is-success");
}

async function connectCodex(): Promise<void> {
  setProviderStatus("Starting Codex device flow...");
  const response = await fetch("/settings/provider/codex/start", { method: "POST", credentials: "include" });
  if (!response.ok) {
    setProviderStatus("Failed to start Codex OAuth.", "error");
    return;
  }
  const payload = (await response.json()) as CodexStartResponse;
  setProviderStatus(`Enter code ${payload.userCode} in the OpenAI window.`);
  window.open(payload.verifyUrl, "_blank", "noopener,noreferrer");
  ($("provider-refresh") as HTMLButtonElement | null)?.click();
  startCodexPolling(payload.intervalSeconds);
}

async function connectClaude(): Promise<void> {
  setProviderStatus("Starting Claude OAuth...");
  const response = await fetch("/settings/provider/claude/start", { method: "POST", credentials: "include" });
  if (!response.ok) {
    setProviderStatus("Failed to start Claude OAuth.", "error");
    return;
  }
  const payload = (await response.json()) as ClaudeStartResponse;
  setProviderStatus("Claude login started. Complete the browser step, then paste the code below.");
  window.open(payload.authUrl, "_blank", "noopener,noreferrer");
  ($("provider-refresh") as HTMLButtonElement | null)?.click();
}

async function connectProvider(kind: "codex" | "claude"): Promise<void> {
  if (kind === "claude") {
    await connectClaude();
    return;
  }
  await connectCodex();
}

async function submitClaudeCode(): Promise<void> {
  const input = $("provider-claude-code") as HTMLInputElement | null;
  const code = input?.value.trim() ?? "";
  if (!code) {
    setProviderStatus("Paste the Claude authorization code first.", "error");
    input?.focus();
    return;
  }

  setProviderStatus("Finishing Claude OAuth...");
  const response = await fetch("/settings/provider/claude/exchange", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ code }),
  });
  if (!response.ok) {
    setProviderStatus("Failed to finish Claude OAuth.", "error");
    return;
  }
  const payload = (await response.json()) as ClaudeExchangeResponse;
  if (payload.status === "connected") {
    setProviderStatus(payload.message ?? "Claude is connected.", "success");
    if (input) input.value = "";
    ($("provider-refresh") as HTMLButtonElement | null)?.click();
    return;
  }
  if (payload.authUrl) window.open(payload.authUrl, "_blank", "noopener,noreferrer");
  setProviderStatus(payload.message ?? "Claude OAuth failed.", "error");
  ($("provider-refresh") as HTMLButtonElement | null)?.click();
}

function startCodexPolling(intervalSeconds: number): void {
  if (authPollTimer !== null) window.clearTimeout(authPollTimer);
  const tick = async (): Promise<void> => {
    const response = await fetch("/settings/provider/codex/poll", { method: "POST", credentials: "include" });
    if (!response.ok) {
      setProviderStatus("Failed to verify.", "error");
      return;
    }
    const payload = (await response.json()) as CodexPollResponse;
    if (payload.status === "connected") {
      setProviderStatus(payload.message ?? "Connected.", "success");
      ($("provider-refresh") as HTMLButtonElement | null)?.click();
      return;
    }
    if (payload.status === "pending") {
      setProviderStatus(payload.message ?? "Waiting...");
      authPollTimer = window.setTimeout(() => void tick(), intervalSeconds * 1000);
      return;
    }
    setProviderStatus("No pending session.");
  };
  authPollTimer = window.setTimeout(() => void tick(), intervalSeconds * 1000);
}

function getReferenceMentionItemsFromDom(): MentionItem[] {
  return Array.from(document.querySelectorAll<HTMLElement>("#reference-list .reference-item")).map((item) => ({
    handle: item.dataset.referenceHandle ?? "",
    label: item.dataset.referenceLabel ?? "",
    kind: (item.dataset.referenceKind ?? "text") as MentionItem["kind"],
    search: `${item.dataset.referenceLabel ?? ""} ${item.dataset.referenceKind ?? ""}`.toLowerCase(),
    sessionId: getActiveSessionId(),
  })).filter((item) => Boolean(item.handle) && Boolean(item.label));
}

async function loadDesignMentionItems(): Promise<void> {
  const response = await fetch("/api/storms", { credentials: "include" });
  if (!response.ok) return;
  const runs = (await response.json()) as StormRun[];
  designMentionItems = runs.map((run) => ({
    handle: `design:${run.id}`,
    label: run.title,
    kind: "design",
    search: `${run.title} ${run.summary}`.toLowerCase(),
    sessionId: run.sessionId ?? null,
  }));
}

function getAllMentionItems(): MentionItem[] {
  const activeSessionId = getActiveSessionId();
  const merged = [...designMentionItems, ...getReferenceMentionItemsFromDom()];
  merged.sort((left, right) => {
    const leftActive = left.sessionId === activeSessionId ? 0 : 1;
    const rightActive = right.sessionId === activeSessionId ? 0 : 1;
    if (leftActive !== rightActive) return leftActive - rightActive;
    return left.label.localeCompare(right.label);
  });
  return merged;
}

function renderSelectedReferences(): void {
  // References are tracked in the selectedReferences map
  // and sent as referenceIds on submit. No visual chips needed —
  // the @mention text in the textarea is the visual indicator.
}

function renderDraftIteration(): void {
  // Draft iteration state is tracked in draftIteratesOnId/Label
  // and sent as iteratesOnId on submit. No visual element needed.
}

function clearDraftContext(): void {
  selectedReferences.clear();
  draftIteratesOnId = null;
  draftIteratesOnLabel = null;
  renderSelectedReferences();
  renderDraftIteration();
}

function addSelectedReference(item: MentionItem): void {
  selectedReferences.set(item.handle, item);
  renderSelectedReferences();
  setStatus(`Added ${item.label} to the draft context.`);
}

function setDraftIteration(id: string, label: string): void {
  draftIteratesOnId = id;
  draftIteratesOnLabel = label;
  renderDraftIteration();
}

function hideMentionMenu(): void {
  mentionState = null;
  const menu = $("mention-menu");
  if (menu) {
    menu.innerHTML = "";
    menu.hidden = true;
  }
}

function renderMentionMenu(items: MentionItem[]): void {
  const menu = $("mention-menu");
  if (!menu) return;
  if (items.length === 0 || !mentionState) {
    hideMentionMenu();
    return;
  }
  const activeSessionId = getActiveSessionId();
  menu.innerHTML = items.slice(0, 8).map((item) => {
    const isOther = item.sessionId && item.sessionId !== activeSessionId;
    return `<button class="mention-item${isOther ? " is-other-session" : ""}" type="button" data-mention-handle="${escapeHtml(item.handle)}" data-mention-label="${escapeHtml(item.label)}">
      <span class="mention-item-title">${escapeHtml(shortLabel(item.label))}</span>
      <span class="mention-item-kind">${escapeHtml(item.kind)}${isOther ? " · other" : ""}</span>
    </button>`;
  }).join("");
  menu.hidden = false;
}

function updateMentionState(): void {
  const textarea = $("session-composer") as HTMLTextAreaElement | null;
  if (!textarea) return;
  const cursor = textarea.selectionStart ?? textarea.value.length;
  const beforeCursor = textarea.value.slice(0, cursor);
  const match = beforeCursor.match(/(?:^|\s)@([^\s@]*)$/);
  if (!match) {
    hideMentionMenu();
    return;
  }
  const query = match[1] ?? "";
  const start = cursor - query.length - 1;
  const items = getAllMentionItems().filter((item) => item.search.includes(query.toLowerCase()));
  mentionState = { start, end: cursor, query, items };
  renderMentionMenu(items);
}

function shortLabel(label: string): string {
  // Take text before " — " or " - ", or first 30 chars
  const dash = label.indexOf(" — ");
  const hyphen = dash === -1 ? label.indexOf(" - ") : dash;
  const short = hyphen > 0 ? label.slice(0, hyphen) : label;
  return short.length > 30 ? short.slice(0, 30) + "…" : short;
}

function insertMention(handle: string, label: string): void {
  const textarea = $("session-composer") as HTMLTextAreaElement | null;
  if (!textarea || !mentionState) return;
  const short = shortLabel(label);
  const nextValue = `${textarea.value.slice(0, mentionState.start)}@${short} ${textarea.value.slice(mentionState.end)}`;
  const nextCursor = mentionState.start + short.length + 2;
  textarea.value = nextValue;
  textarea.focus();
  textarea.setSelectionRange(nextCursor, nextCursor);
  const item = getAllMentionItems().find((candidate) => candidate.handle === handle);
  if (item) addSelectedReference(item);
  hideMentionMenu();
}

async function applySnapshot(
  payload: StudioSnapshotResponse,
  options: { preserveDraft: boolean },
): Promise<void> {
  const sessionList = $("session-list");
  const messages = $("session-messages");
  const gallery = $("design-gallery");
  const references = $("reference-list");
  const title = $("active-session-title") as HTMLInputElement | null;
  const updated = $("active-session-updated");

  if (sessionList) sessionList.innerHTML = payload.sessionListHtml;
  if (messages) messages.innerHTML = payload.messagesHtml;
  if (gallery) gallery.innerHTML = payload.galleryHtml;
  if (references) references.innerHTML = payload.referenceListHtml;
  if (title) {
    title.value = payload.activeSessionTitle;
    lastSavedTitle = payload.activeSessionTitle;
  }
  if (updated) updated.textContent = payload.activeSessionUpdatedLabel;
  if (payload.status) setStatus(payload.status);

  if (!options.preserveDraft) {
    const composer = $("session-composer") as HTMLTextAreaElement | null;
    if (composer) composer.value = "";
    clearDraftContext();
  } else {
    renderSelectedReferences();
    renderDraftIteration();
  }

  const thread = $("session-messages");
  if (thread) thread.scrollTop = thread.scrollHeight;
  await loadDesignMentionItems();
  startPollingIfNeeded();
}

async function fetchSnapshot(options: { preserveDraft: boolean }): Promise<void> {
  const response = await fetch(`/sessions/${getActiveSessionId()}/snapshot`, { credentials: "include" });
  if (!response.ok) return;
  const payload = (await response.json()) as StudioSnapshotResponse;
  await applySnapshot(payload, options);
}

function openDesignFullscreen(title: string, previewUrl: string): void {
  const existing = document.querySelector(".design-fullscreen");
  if (existing) existing.remove();

  const overlay = document.createElement("div");
  overlay.className = "design-fullscreen";
  overlay.innerHTML = `
    <div class="design-fullscreen-bar">
      <span class="design-fullscreen-title">${escapeHtml(title)}</span>
      <button class="design-fullscreen-close" type="button" aria-label="Close fullscreen">&times;</button>
    </div>
    <iframe src="${escapeHtml(previewUrl)}" title="${escapeHtml(title)}" sandbox="allow-scripts allow-forms allow-modals" referrerpolicy="no-referrer"></iframe>
  `;

  const close = () => overlay.remove();
  overlay.querySelector(".design-fullscreen-close")!.addEventListener("click", close);
  const onKey = (e: KeyboardEvent) => {
    if (e.key === "Escape") { close(); document.removeEventListener("keydown", onKey); }
  };
  document.addEventListener("keydown", onKey);

  document.body.appendChild(overlay);
}

function startPollingIfNeeded(): void {
  const hasPending = document.querySelector("[data-job-status='pending'], [data-job-status='running']") !== null;
  if (!hasPending) {
    if (pollTimer !== null) {
      window.clearInterval(pollTimer);
      pollTimer = null;
    }
    return;
  }
  if (pollTimer !== null) return;
  pollTimer = window.setInterval(() => {
    void fetchSnapshot({ preserveDraft: true });
  }, 4000);
}

async function createNewSession(): Promise<void> {
  const response = await fetch("/sessions", {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({}),
  });
  if (!response.ok) {
    setStatus("Failed to create a session.");
    return;
  }
  const payload = (await response.json()) as CreateDesignSessionResponse;
  window.location.href = payload.location;
}

async function saveSessionTitle(): Promise<void> {
  const input = $("active-session-title") as HTMLInputElement | null;
  if (!input) return;
  const nextTitle = input.value.trim();
  if (!nextTitle || nextTitle === lastSavedTitle) {
    input.value = lastSavedTitle;
    return;
  }
  const response = await fetch(`/sessions/${getActiveSessionId()}/rename`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ title: nextTitle }),
  });
  if (!response.ok) {
    setStatus("Failed to rename the session.");
    input.value = lastSavedTitle;
    return;
  }
  const payload = (await response.json()) as StudioSnapshotResponse;
  await applySnapshot(payload, { preserveDraft: true });
}

async function submitSessionMessage(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  const composer = $("session-composer") as HTMLTextAreaElement | null;
  const submit = $("session-send") as HTMLButtonElement | null;
  if (!composer || !submit) return;
  const body = composer.value.trim();
  if (!body) {
    composer.focus();
    return;
  }

  // Optimistically render the user message + thinking indicator
  const thread = $("session-messages");
  if (thread) {
    const now = new Date();
    const time = now.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" }).toLowerCase();
    const msgHtml = `<article class="chat-message is-user">
  <header class="chat-message-head">
    <span class="chat-message-role">You</span>
    <span class="chat-message-meta">${time}</span>
  </header>
  <div class="chat-message-body">${escapeHtml(body)}</div>
</article>
<article class="chat-message is-assistant is-thinking" id="thinking-indicator">
  <header class="chat-message-head">
    <span class="chat-message-role">Agent</span>
  </header>
  <div class="chat-message-body">
    <span class="thinking-dots"><span></span><span></span><span></span></span>
  </div>
</article>`;
    thread.insertAdjacentHTML("beforeend", msgHtml);
    thread.scrollTop = thread.scrollHeight;
  }

  const referenceIds = Array.from(selectedReferences.keys());
  const iteratesOnId = draftIteratesOnId;

  // Clear input and context right away
  composer.value = "";
  composer.style.height = "auto";
  clearDraftContext();
  composer.focus();

  submit.disabled = true;
  try {
    const response = await fetch(`/sessions/${getActiveSessionId()}/messages`, {
      method: "POST",
      credentials: "include",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ body, referenceIds, iteratesOnId }),
    });
    if (!response.ok) {
      // Remove thinking indicator on error
      $("thinking-indicator")?.remove();
      setStatus(await response.text());
      return;
    }
    const payload = (await response.json()) as StudioSnapshotResponse;
    await applySnapshot(payload, { preserveDraft: false });
    composer.focus();
  } finally {
    submit.disabled = false;
    $("thinking-indicator")?.remove();
  }
}

async function submitReferenceForm(
  kind: "text" | "link",
  payload: Record<string, string>,
): Promise<void> {
  const response = await fetch(`/sessions/${getActiveSessionId()}/references`, {
    method: "POST",
    credentials: "include",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ kind, ...payload }),
  });
  if (!response.ok) {
    setStatus(await response.text());
    return;
  }
  const snapshot = (await response.json()) as StudioSnapshotResponse;
  await applySnapshot(snapshot, { preserveDraft: true });
}

async function submitImageReference(file: File): Promise<void> {
  const formData = new FormData();
  formData.append("file", file);
  const response = await fetch(`/sessions/${getActiveSessionId()}/references/image`, {
    method: "POST",
    credentials: "include",
    body: formData,
  });
  if (!response.ok) {
    setStatus(await response.text());
    return;
  }
  const snapshot = (await response.json()) as StudioSnapshotResponse;
  await applySnapshot(snapshot, { preserveDraft: true });
}

function bindStudioEvents(): void {
  ($("new-session-button") as HTMLButtonElement | null)?.addEventListener("click", () => {
    void createNewSession();
  });

  const title = $("active-session-title") as HTMLInputElement | null;
  if (title) {
    lastSavedTitle = title.value;
    title.addEventListener("blur", () => {
      void saveSessionTitle();
    });
    title.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        event.preventDefault();
        title.blur();
      }
    });
  }

  $("session-message-form")?.addEventListener("submit", (event) => {
    void submitSessionMessage(event as SubmitEvent);
  });

  $("session-composer")?.addEventListener("input", () => {
    updateMentionState();
    // Auto-grow textarea
    const ta = $("session-composer") as HTMLTextAreaElement | null;
    if (ta) {
      ta.style.height = "auto";
      ta.style.height = Math.min(ta.scrollHeight, 180) + "px";
    }
  });
  $("session-composer")?.addEventListener("keydown", (event) => {
    if (event.key === "Escape") hideMentionMenu();
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      $("session-message-form")?.dispatchEvent(new Event("submit", { cancelable: true }));
    }
  });

  $("clear-selected-references")?.addEventListener("click", () => {
    clearDraftContext();
  });

  $("reference-note-form")?.addEventListener("submit", (event) => {
    event.preventDefault();
    const titleInput = $("reference-note-title") as HTMLInputElement | null;
    const bodyInput = $("reference-note-body") as HTMLTextAreaElement | null;
    const body = bodyInput?.value.trim() ?? "";
    if (!body) return;
    void submitReferenceForm("text", {
      title: titleInput?.value.trim() ?? "",
      body,
    }).then(() => {
      if (titleInput) titleInput.value = "";
      if (bodyInput) bodyInput.value = "";
    });
  });

  $("reference-link-form")?.addEventListener("submit", (event) => {
    event.preventDefault();
    const titleInput = $("reference-link-title") as HTMLInputElement | null;
    const urlInput = $("reference-link-url") as HTMLInputElement | null;
    const bodyInput = $("reference-link-body") as HTMLTextAreaElement | null;
    const url = urlInput?.value.trim() ?? "";
    if (!url) return;
    void submitReferenceForm("link", {
      title: titleInput?.value.trim() ?? "",
      url,
      body: bodyInput?.value.trim() ?? "",
    }).then(() => {
      if (titleInput) titleInput.value = "";
      if (urlInput) urlInput.value = "";
      if (bodyInput) bodyInput.value = "";
    });
  });

  $("reference-image-input")?.addEventListener("change", (event) => {
    const input = event.currentTarget as HTMLInputElement;
    const file = input.files?.[0];
    if (!file) return;
    void submitImageReference(file).then(() => {
      input.value = "";
    });
  });

  document.addEventListener("click", (event) => {
    const target = event.target as HTMLElement | null;
    if (!target) return;

    const mentionButton = target.closest<HTMLElement>("[data-mention-handle]");
    if (mentionButton) {
      event.preventDefault();
      insertMention(
        mentionButton.dataset.mentionHandle ?? "",
        mentionButton.dataset.mentionLabel ?? "",
      );
      return;
    }

    const removeChip = target.closest<HTMLElement>("[data-remove-reference]");
    if (removeChip) {
      const handle = removeChip.dataset.removeReference ?? "";
      selectedReferences.delete(handle);
      renderSelectedReferences();
      return;
    }

    if (target.closest<HTMLElement>("[data-clear-iteration]")) {
      draftIteratesOnId = null;
      draftIteratesOnLabel = null;
      renderDraftIteration();
      return;
    }

    const expandButton = target.closest<HTMLElement>("[data-action='expand-design']");
    if (expandButton) {
      const label = expandButton.dataset.designLabel ?? "Design";
      const previewUrl = expandButton.dataset.previewUrl ?? "";
      if (previewUrl) {
        openDesignFullscreen(label, previewUrl);
      }
      return;
    }

    const iterateButton = target.closest<HTMLElement>("[data-action='iterate-design']");
    if (iterateButton) {
      const designId = iterateButton.dataset.designId ?? "";
      const label = iterateButton.dataset.designLabel ?? "";
      if (designId && label) {
        setDraftIteration(designId, label);
        addSelectedReference({
          handle: `design:${designId}`,
          label,
          kind: "design",
          search: label.toLowerCase(),
          sessionId: getActiveSessionId(),
        });
      }
      return;
    }

    const referenceButton = target.closest<HTMLElement>("[data-action='use-design-reference']");
    if (referenceButton) {
      const handle = referenceButton.dataset.referenceHandle ?? "";
      const label = referenceButton.dataset.referenceLabel ?? "";
      if (handle && label) {
        addSelectedReference({
          handle,
          label,
          kind: "design",
          search: label.toLowerCase(),
          sessionId: getActiveSessionId(),
        });
      }
      return;
    }

    const shiftDesign = target.closest<HTMLElement>(".design-card");
    if (shiftDesign && event instanceof MouseEvent && event.shiftKey) {
      event.preventDefault();
      const handle = shiftDesign.dataset.designHandle ?? "";
      const label = shiftDesign.dataset.designLabel ?? "";
      if (handle && label) {
        addSelectedReference({
          handle,
          label,
          kind: "design",
          search: label.toLowerCase(),
          sessionId: getActiveSessionId(),
        });
      }
      return;
    }

    const shiftReference = target.closest<HTMLElement>(".reference-item");
    if (shiftReference && event instanceof MouseEvent && event.shiftKey) {
      event.preventDefault();
      const handle = shiftReference.dataset.referenceHandle ?? "";
      const label = shiftReference.dataset.referenceLabel ?? "";
      const kind = (shiftReference.dataset.referenceKind ?? "text") as MentionItem["kind"];
      if (handle && label) {
        addSelectedReference({
          handle,
          label,
          kind,
          search: `${label} ${kind}`.toLowerCase(),
          sessionId: getActiveSessionId(),
        });
      }
      return;
    }

    if (!target.closest("#mention-menu") && !target.closest("#session-composer")) {
      hideMentionMenu();
    }
  });
}

async function bindStudioApp(): Promise<void> {
  if (getConfig().currentPath !== "/app") return;
  setAvatarInitials();
  renderSelectedReferences();
  renderDraftIteration();
  await loadDesignMentionItems();
  bindStudioEvents();
  startPollingIfNeeded();
  const thread = $("session-messages");
  if (thread) thread.scrollTop = thread.scrollHeight;
}

async function bootstrap(): Promise<void> {
  const instance = await ensureClerk();
  if (instance) {
    const response = await fetch("/auth/me", { credentials: "include" });
    const auth = response.ok ? ((await response.json()) as { authenticated: boolean }) : { authenticated: false };
    if (!auth.authenticated && instance.session) {
      const synced = await syncServerSession();
      if (synced && getConfig().currentPath === "/") redirectToApp();
    }
  }
  await bindStudioApp();
}

window.designstormAuth = { signIn, signUp, signOut };
window.designstormSettings = { connectProvider, submitClaudeCode };

void bootstrap();
