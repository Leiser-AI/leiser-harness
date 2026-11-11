// The CLI extensions panel: the skills, MCP servers, hooks and instruction
// files each agent CLI loads from a repository. They belong to the agent
// CLI (ADR 0003): the panel opens the agent CLI's own file as text and
// saves it back. It never converts between agent CLIs.

import type { AgentCli, CliExtension, CliExtensionKind, RepositoryId, UnreadableCliExtensionFile } from "./protocol";
import * as service from "./service";
import { AGENT_CLI_NAMES } from "./service";

const AGENT_CLIS: AgentCli[] = ["claude_code", "codex"];

const KINDS: { type: CliExtensionKind["type"]; title: string }[] = [
  { type: "instruction_file", title: "Instruction files" },
  { type: "skill", title: "Skills" },
  { type: "mcp_server", title: "MCP servers" },
  { type: "hook", title: "Hooks" },
];

/** What the agent CLI needs before it loads a kind, where it is not obvious. */
const NOTES: Partial<Record<AgentCli, string>> = {
  codex: "Codex loads .codex/config.toml and .codex/hooks.json only in trusted projects.",
};

/** The file open in the editor. */
interface OpenFile {
  file: string;
  /** The text as read from the file; saving checks the file still holds it. */
  previous: string;
}

let repository: RepositoryId | null = null;
let listing: service.CliExtensions = { extensions: [], unreadable: [] };
let open: OpenFile | null = null;

let root: HTMLElement;
let list: HTMLElement;
let editorTitle: HTMLElement;
let editorDetail: HTMLElement;
let editorText: HTMLTextAreaElement;
let editorStatus: HTMLElement;
let saveButton: HTMLButtonElement;
let discardButton: HTMLButtonElement;
let reloadButton: HTMLButtonElement;

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className?: string,
  text?: string,
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  if (className) element.className = className;
  if (text !== undefined) element.textContent = text;
  return element;
}

function button(text: string, onClick: () => void): HTMLButtonElement {
  const element = el("button", "", text);
  element.type = "button";
  element.addEventListener("click", onClick);
  return element;
}

/** Builds the panel inside `container` once; it stays hidden until opened. */
export function mount(container: HTMLElement): void {
  root = container;
  list = el("nav", "extensions-list");
  list.setAttribute("aria-label", "CLI extensions");

  const editor = el("section", "extensions-editor");
  const header = el("header", "extensions-editor-header");
  const heading = el("div", "extensions-editor-heading");
  editorTitle = el("div", "extensions-editor-title");
  editorDetail = el("div", "extensions-editor-detail");
  heading.append(editorTitle, editorDetail);
  reloadButton = button("Reload", () => void run(reload));
  discardButton = button("Discard changes", () => void run(reload));
  saveButton = button("Save", () => void run(save));
  saveButton.classList.add("primary");
  const actions = el("div", "extensions-editor-actions");
  actions.append(reloadButton, discardButton, saveButton);
  header.append(heading, actions);
  editorText = el("textarea", "extensions-editor-text");
  editorText.spellcheck = false;
  editorText.addEventListener("input", renderEditorState);
  editorText.addEventListener("keydown", (event) => {
    if (event.key === "s" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      void run(save);
    }
  });
  editorStatus = el("div", "extensions-editor-status");
  editorStatus.setAttribute("role", "status");
  editor.append(header, editorText, editorStatus);

  root.replaceChildren(list, editor);
  renderEditor();
}

/** Shows the panel for `id`, reading its CLI extensions again. */
export async function show(id: RepositoryId): Promise<void> {
  if (repository !== id) {
    repository = id;
    open = null;
    listing = { extensions: [], unreadable: [] };
    renderEditor();
  }
  root.hidden = false;
  await loadList();
}

export function hide(): void {
  root.hidden = true;
}

/** Whether the open file has changes that are not saved. */
export function hasUnsavedChanges(): boolean {
  return open !== null && editorText.value !== open.previous;
}

async function run(action: () => Promise<void>): Promise<void> {
  try {
    await action();
  } catch (error) {
    status(error instanceof Error ? error.message : String(error), true);
  }
}

function status(text: string, failed = false): void {
  editorStatus.textContent = text;
  editorStatus.classList.toggle("failed", failed);
}

async function loadList(): Promise<void> {
  if (repository === null) return;
  const id = repository;
  const loaded = await service.listCliExtensions(id);
  if (repository !== id) return;
  listing = loaded;
  renderList();
  renderEditor();
}

async function openFile(file: string): Promise<void> {
  if (repository === null) return;
  if (open?.file === file) return;
  if (hasUnsavedChanges()) {
    status(`Save or discard your changes to ${open?.file} first.`, true);
    return;
  }
  const text = await service.readCliExtensionFile(repository, file);
  open = { file, previous: text };
  editorText.value = text;
  status("");
  renderList();
  renderEditor();
  editorText.focus();
}

async function reload(): Promise<void> {
  if (repository === null || open === null) return;
  const text = await service.readCliExtensionFile(repository, open.file);
  open = { file: open.file, previous: text };
  editorText.value = text;
  status("Reloaded from disk.");
  await loadList();
}

async function save(): Promise<void> {
  if (repository === null || open === null || !hasUnsavedChanges()) return;
  const text = editorText.value;
  await service.saveCliExtensionFile(repository, open.file, text, open.previous);
  open = { file: open.file, previous: text };
  status(`Saved ${open.file}.`);
  await loadList();
}

function renderList(): void {
  const columns = AGENT_CLIS.map((agentCli) => {
    const column = el("section", "extensions-agent-cli");
    column.append(el("h2", "", AGENT_CLI_NAMES[agentCli]));
    const note = NOTES[agentCli];
    if (note) column.append(el("p", "extensions-note", note));
    const extensions = listing.extensions.filter((extension) => extension.agent_cli === agentCli);
    const unreadable = listing.unreadable.filter((file) => file.agent_cli === agentCli);
    if (extensions.length === 0 && unreadable.length === 0) {
      column.append(el("p", "empty", "Loads no CLI extensions from this repository."));
    }
    for (const file of unreadable) column.append(renderUnreadable(file));
    for (const kind of KINDS) {
      const ofKind = extensions.filter((extension) => extension.kind.type === kind.type);
      if (ofKind.length === 0) continue;
      const items = el("ul", "extensions-items");
      for (const extension of ofKind) items.append(renderExtension(extension));
      column.append(el("h3", "", `${kind.title} (${ofKind.length})`), items);
    }
    return column;
  });
  list.replaceChildren(...columns);
}

function renderUnreadable(file: UnreadableCliExtensionFile): HTMLElement {
  const item = el("button", "extension unreadable");
  item.type = "button";
  item.append(
    el("span", "extension-name", `${file.path} could not be read`),
    el("span", "extension-detail", file.reason),
  );
  markOpen(item, file.file);
  item.addEventListener("click", () => void run(() => openFile(file.file)));
  return item;
}

function renderExtension(extension: CliExtension): HTMLElement {
  const listItem = el("li");
  const item = el("button", "extension");
  item.type = "button";
  item.title = extension.file;
  const name = el("span", "extension-name", extension.name);
  const shared = otherAgentClis(extension);
  if (shared.length > 0) {
    name.append(el("span", "extension-shared", `also ${shared.map((cli) => AGENT_CLI_NAMES[cli]).join(", ")}`));
  }
  item.append(name);
  const where = extension.path === extension.file ? extension.path : `${extension.path} → ${extension.file}`;
  item.append(el("span", "extension-path", where));
  const detail = describe(extension.kind);
  if (detail) item.append(el("span", "extension-detail", detail));
  markOpen(item, extension.file);
  item.addEventListener("click", () => void run(() => openFile(extension.file)));
  listItem.append(item);
  return listItem;
}

function markOpen(item: HTMLElement, file: string): void {
  if (open?.file === file) {
    item.classList.add("selected");
    item.setAttribute("aria-current", "true");
  }
}

function describe(kind: CliExtensionKind): string | null {
  switch (kind.type) {
    case "instruction_file":
      return kind.imported_by === null ? null : `imported by ${kind.imported_by}`;
    case "skill":
      return kind.description;
    case "mcp_server":
      return kind.launch;
    case "hook": {
      const matcher = kind.matcher === null ? "" : `${kind.matcher}: `;
      return `${matcher}${kind.commands.join("; ")}`;
    }
  }
}

/** The other agent CLIs that load the same file. */
function otherAgentClis(extension: CliExtension): AgentCli[] {
  return AGENT_CLIS.filter(
    (agentCli) =>
      agentCli !== extension.agent_cli &&
      listing.extensions.some((other) => other.agent_cli === agentCli && other.file === extension.file),
  );
}

function renderEditor(): void {
  const hasFile = open !== null;
  editorText.hidden = !hasFile;
  reloadButton.hidden = !hasFile;
  editorTitle.classList.toggle("placeholder", open === null);
  if (open === null) {
    editorTitle.textContent = "Select a CLI extension to open its file.";
    editorDetail.textContent = "";
    discardButton.hidden = true;
    saveButton.hidden = true;
    return;
  }
  const file = open.file;
  editorTitle.textContent = file;
  const loadedAs = listing.extensions
    .filter((extension) => extension.file === file)
    .map((extension) => `${AGENT_CLI_NAMES[extension.agent_cli]}: ${extension.name}`);
  const unique = [...new Set(loadedAs)];
  editorDetail.textContent =
    unique.length === 0 ? "" : `Holds ${unique.join(" · ")}. Saved as is, in this agent CLI's own format.`;
  renderEditorState();
}

function renderEditorState(): void {
  const dirty = hasUnsavedChanges();
  saveButton.hidden = open === null;
  saveButton.disabled = !dirty;
  discardButton.hidden = !dirty;
}
