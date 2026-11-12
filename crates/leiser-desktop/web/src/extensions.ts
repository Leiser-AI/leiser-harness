// The CLI extensions panel: the skills, MCP servers, hooks and instruction
// files each agent CLI loads from a repository. They belong to the agent
// CLI (ADR 0003): the panel opens the agent CLI's own file as text and
// saves it back. It never converts between agent CLIs.

import { icon, type IconName } from "./icons";
import type { AgentCli, CliExtension, CliExtensionKind, RepositoryId, UnreadableCliExtensionFile } from "./protocol";
import * as service from "./service";
import { AGENT_CLI_NAMES } from "./service";

const AGENT_CLIS: AgentCli[] = ["claude_code", "codex"];

const KINDS: { type: CliExtensionKind["type"]; title: string; icon: IconName }[] = [
  { type: "instruction_file", title: "Instruction files", icon: "file" },
  { type: "skill", title: "Skills", icon: "sparkle" },
  { type: "mcp_server", title: "MCP servers", icon: "plug" },
  { type: "hook", title: "Hooks", icon: "hook" },
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
/** The agent CLI whose CLI extensions the list shows. */
let shownAgentCli: AgentCli = "claude_code";
/** Whether the developer chose `shownAgentCli` for this repository. */
let agentCliChosen = false;

/** Runs before every action in the panel. */
let beforeAction: () => void = () => {};

let root: HTMLElement;
let tabs: HTMLElement;
let list: HTMLElement;
let editorEmpty: HTMLElement;
let editorHeader: HTMLElement;
let editorTitle: HTMLElement;
let editorUnsaved: HTMLElement;
let editorDetail: HTMLElement;
let editorText: HTMLTextAreaElement;
let editorFooter: HTMLElement;
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

function button(text: string, className: string, onClick: () => void): HTMLButtonElement {
  const element = el("button", className, text);
  element.type = "button";
  element.addEventListener("click", onClick);
  return element;
}

const SAVE_SHORTCUT = navigator.platform.startsWith("Mac") ? "⌘S" : "Ctrl+S";

/**
 * Builds the panel inside `container` once; it stays hidden until opened.
 * `onAction` runs before every action the developer starts in the panel,
 * for example to clear an error the app shows about the panel.
 */
export function mount(container: HTMLElement, onAction: () => void): void {
  root = container;
  beforeAction = onAction;
  const pane = el("div", "extensions-pane");
  tabs = el("div", "extensions-tabs");
  tabs.setAttribute("role", "tablist");
  tabs.setAttribute("aria-label", "Agent CLI");
  tabs.addEventListener("keydown", moveBetweenTabs);
  list = el("div", "extensions-list");
  list.id = "extensions-list";
  list.setAttribute("role", "tabpanel");
  pane.append(tabs, list);

  const editor = el("section", "extensions-editor");
  editorEmpty = el("div", "extensions-empty");
  const logo = el("div", "extensions-empty-logo");
  logo.append(icon("blocks"));
  editorEmpty.append(
    logo,
    el("h2", "extensions-empty-title", "Select a CLI extension"),
    el(
      "p",
      "muted",
      "Its file opens here as text. Saving writes the agent CLI's own file; nothing is converted between agent CLIs.",
    ),
  );
  editorHeader = el("header", "extensions-editor-header");
  const heading = el("div", "extensions-editor-heading");
  const titleRow = el("div", "extensions-editor-title-row");
  editorTitle = el("h2", "extensions-editor-title");
  editorUnsaved = el("span", "badge warning", "Unsaved");
  titleRow.append(editorTitle, editorUnsaved);
  editorDetail = el("div", "extensions-editor-detail");
  heading.append(titleRow, editorDetail);
  reloadButton = button("Reload", "ghost", () => void run(reload));
  reloadButton.prepend(icon("refresh"));
  reloadButton.title = "Read the file from disk again";
  discardButton = button("Discard changes", "", () => void run(reload));
  saveButton = button("Save", "primary", () => void run(save));
  saveButton.title = `Save the file (${SAVE_SHORTCUT})`;
  const actions = el("div", "extensions-editor-actions");
  actions.append(reloadButton, discardButton, saveButton);
  editorHeader.append(heading, actions);
  editorText = el("textarea", "extensions-editor-text");
  editorText.spellcheck = false;
  editorText.setAttribute("aria-label", "File contents");
  editorText.addEventListener("input", renderEditorState);
  editorText.addEventListener("keydown", (event) => {
    if (event.key === "s" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      void run(save);
    }
  });
  editorFooter = el("footer", "extensions-editor-footer");
  editorStatus = el("div", "extensions-editor-status");
  editorStatus.setAttribute("role", "status");
  editorFooter.append(editorStatus, el("span", "extensions-editor-hint", `${SAVE_SHORTCUT} saves`));
  editor.append(editorEmpty, editorHeader, editorText, editorFooter);

  root.replaceChildren(pane, editor);
  renderEditor();
}

/** Shows the panel for `id`, reading its CLI extensions again. */
export async function show(id: RepositoryId): Promise<void> {
  if (repository !== id) {
    repository = id;
    open = null;
    listing = { extensions: [], unreadable: [] };
    agentCliChosen = false;
    status("");
    renderList();
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
  beforeAction();
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
  // Until the developer chooses, the list shows the first agent CLI that loads something.
  if (!agentCliChosen) shownAgentCli = AGENT_CLIS.find((agentCli) => count(agentCli) > 0) ?? "claude_code";
  renderList();
  renderEditor();
}

/** How many entries the list shows for `agentCli`. */
function count(agentCli: AgentCli): number {
  return [...listing.extensions, ...listing.unreadable].filter((entry) => entry.agent_cli === agentCli).length;
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
  tabs.replaceChildren(...AGENT_CLIS.map(renderTab));
  const agentCli = shownAgentCli;
  const name = AGENT_CLI_NAMES[agentCli];
  list.setAttribute("aria-label", `CLI extensions of ${name}`);
  const body: HTMLElement[] = [];
  const note = NOTES[agentCli];
  if (note) {
    const item = el("p", "extensions-note");
    item.append(icon("info"), el("span", "", note));
    body.push(item);
  }
  const extensions = listing.extensions.filter((extension) => extension.agent_cli === agentCli);
  const unreadable = listing.unreadable.filter((file) => file.agent_cli === agentCli);
  body.push(...unreadable.map(renderUnreadable));
  for (const kind of KINDS) {
    const ofKind = extensions.filter((extension) => extension.kind.type === kind.type);
    if (ofKind.length === 0) continue;
    const group = el("section", "extensions-group");
    const heading = el("h3", "extensions-group-heading");
    heading.append(icon(kind.icon), el("span", "", kind.title), el("span", "extensions-count", String(ofKind.length)));
    const items = el("ul", "extensions-items");
    for (const extension of ofKind) items.append(renderExtension(extension));
    group.append(heading, items);
    body.push(group);
  }
  if (extensions.length === 0 && unreadable.length === 0) {
    body.push(el("p", "empty", `${name} loads no CLI extensions from this repository.`));
  }
  list.replaceChildren(...body);
}

function renderTab(agentCli: AgentCli): HTMLElement {
  const tab = el("button", "extensions-tab");
  tab.type = "button";
  tab.setAttribute("role", "tab");
  tab.setAttribute("aria-controls", list.id);
  const shown = agentCli === shownAgentCli;
  tab.setAttribute("aria-selected", String(shown));
  // Only the shown tab is a tab stop; the arrow keys move between tabs.
  tab.tabIndex = shown ? 0 : -1;
  tab.dataset.agentCli = agentCli;
  tab.append(el("span", "", AGENT_CLI_NAMES[agentCli]), el("span", "extensions-count", String(count(agentCli))));
  tab.addEventListener("click", () => showAgentCli(agentCli));
  return tab;
}

function showAgentCli(agentCli: AgentCli): void {
  shownAgentCli = agentCli;
  agentCliChosen = true;
  renderList();
}

function moveBetweenTabs(event: KeyboardEvent): void {
  const step = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
  if (step === 0) return;
  const next = AGENT_CLIS[(AGENT_CLIS.indexOf(shownAgentCli) + step + AGENT_CLIS.length) % AGENT_CLIS.length];
  if (next === undefined) return;
  event.preventDefault();
  showAgentCli(next);
  tabs.querySelector<HTMLElement>('[aria-selected="true"]')?.focus();
}

function renderUnreadable(file: UnreadableCliExtensionFile): HTMLElement {
  const item = el("button", "extension unreadable");
  item.type = "button";
  item.title = `${file.file}\n${file.reason}`;
  const text = el("span", "extension-text");
  text.append(
    el("span", "extension-name", `${file.path} could not be read`),
    el("span", "extension-summary", file.reason),
  );
  item.append(icon("alert"), text);
  markOpen(item, file.file);
  item.addEventListener("click", () => void run(() => openFile(file.file)));
  return item;
}

function renderExtension(extension: CliExtension): HTMLElement {
  const listItem = el("li");
  const item = el("button", "extension");
  item.type = "button";
  const where = extension.path === extension.file ? extension.path : `${extension.path} → ${extension.file}`;
  const detail = describe(extension.kind);
  item.title = detail === null ? where : `${where}\n${detail}`;
  const name = el("span", "extension-name");
  name.append(el("span", "extension-name-text", extension.name));
  const shared = otherAgentClis(extension);
  if (shared.length > 0) {
    name.append(el("span", "badge", `also ${shared.map((cli) => AGENT_CLI_NAMES[cli]).join(", ")}`));
  }
  const text = el("span", "extension-text");
  text.append(name);
  const summary = summarize(extension, detail);
  if (summary !== null) {
    const line = el("span", "extension-summary", summary);
    // Everything but a skill's description is a path or a command.
    line.classList.toggle("mono", extension.kind.type !== "skill" || detail === null);
    text.append(line);
  }
  item.append(text);
  markOpen(item, extension.file);
  item.addEventListener("click", () => void run(() => openFile(extension.file)));
  listItem.append(item);
  return listItem;
}

/**
 * The line below an extension's name: what it does, or else where its
 * file lies. An instruction file is named after its file, so its path
 * shows only where it says more than the name.
 */
function summarize(extension: CliExtension, detail: string | null): string | null {
  if (extension.kind.type !== "instruction_file") return detail ?? extension.path;
  const parts = [extension.path === extension.name ? null : extension.path, detail];
  const said = parts.filter((part) => part !== null);
  return said.length === 0 ? null : said.join(" · ");
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
  editorEmpty.hidden = hasFile;
  editorHeader.hidden = !hasFile;
  editorText.hidden = !hasFile;
  editorFooter.hidden = !hasFile;
  if (open === null) return;
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
  editorUnsaved.hidden = !dirty;
  saveButton.disabled = !dirty;
  discardButton.hidden = !dirty;
}
