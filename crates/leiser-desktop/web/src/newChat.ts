// The chat the developer is about to start. The app always shows a chat:
// when none is selected, this one, which starts on the harness service
// only with its first message, so an open app creates no chats or
// worktrees by itself.

import type { Checkout, PermissionMode, RepositoryId, RepositoryOverview, StartOn } from "./protocol";

/** Where the new chat starts and with what settings. */
export interface NewChat {
  /** `null`: the most recently used repository. */
  repository: RepositoryId | null;
  checkout: Checkout;
  /** `null`: the repository's default agent CLI and subscription. */
  startOn: StartOn | null;
  /** `null`: the repository's permission mode. */
  permissionMode: PermissionMode | null;
}

const LAST_REPOSITORY = "leiser-harness.last-repository";
const IN_MAIN_CHECKOUT = "leiser-harness.new-chat-in-main-checkout";

function stored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function store(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Without storage the next start of the app begins with the defaults.
  }
}

/** Where the developer last chose to start chats: a new worktree unless they chose the main checkout. */
function preferredCheckout(): Checkout {
  return stored(IN_MAIN_CHECKOUT) === "true" ? { type: "main" } : { type: "new_worktree" };
}

const state: NewChat = {
  repository: null,
  checkout: preferredCheckout(),
  startOn: null,
  permissionMode: null,
};

/** The new chat as the developer has set it so far. */
export function current(): Readonly<NewChat> {
  return state;
}

/**
 * Starts over on `repository` (`null`: the most recently used one), in
 * `checkout` or else where the developer last chose to start chats. The
 * settings go back to the repository's defaults, which differ per
 * repository.
 */
export function open(repository: RepositoryId | null, checkout?: Checkout): void {
  state.repository = repository;
  state.checkout = checkout ?? preferredCheckout();
  resetSettings();
}

/** Puts the settings back to the repository's defaults. */
export function resetSettings(): void {
  state.startOn = null;
  state.permissionMode = null;
}

/** Changes where the new chat starts; a new worktree or the main checkout also holds for later new chats. */
export function setCheckout(checkout: Checkout): void {
  state.checkout = checkout;
  if (checkout.type !== "worktree") store(IN_MAIN_CHECKOUT, String(checkout.type === "main"));
}

export function setStartOn(startOn: StartOn): void {
  state.startOn = startOn;
}

export function setPermissionMode(permissionMode: PermissionMode): void {
  state.permissionMode = permissionMode;
}

/** Notes the repository the developer last worked in, for the next new chat. */
export function remember(repository: RepositoryId): void {
  store(LAST_REPOSITORY, String(repository));
}

function remembered(): RepositoryId | null {
  const last = stored(LAST_REPOSITORY);
  return last === null ? null : Number(last);
}

/**
 * The repository the new chat starts in: the chosen one, else the most
 * recently used, else the first; `null` while there is none.
 */
export function repository(repositories: RepositoryOverview[]): RepositoryOverview | null {
  const wanted = state.repository ?? remembered();
  return repositories.find((overview) => overview.repository === wanted) ?? repositories[0] ?? null;
}
