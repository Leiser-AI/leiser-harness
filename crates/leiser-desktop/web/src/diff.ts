// The diff panel: a chat's changes per turn, from the checkpoints the
// harness service captures before and after every turn, and the whole
// branch's changes against its base. The developer can put the files back
// to a turn's checkpoint. The panel only shows what the service reports.

import type { ChatId, CheckpointMoment, DiffLine, DiffRange, FileDiff, TurnCheckpoints } from "./protocol";
import * as service from "./service";

/** The chat shown, and which of its changes. */
let chat: ChatId | null = null;
let range: DiffRange | null = null;
/** The restore waiting for the developer's confirmation. */
let confirming: CheckpointMoment | null = null;
/** What the last restore did, shown until the developer picks other changes. */
let restored: string | null = null;
/** Drops answers to older reads that arrive after newer ones. */
let readCount = 0;

let root: HTMLElement;
let rangePicker: HTMLSelectElement;
let actions: HTMLElement;
let summary: HTMLElement;
let files: HTMLElement;
/** Reports a failed action the way the rest of the app does. */
let report: (error: unknown) => void = () => {};

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

function button(text: string, onClick: () => void, className = ""): HTMLButtonElement {
  const element = el("button", className, text);
  element.type = "button";
  element.addEventListener("click", onClick);
  return element;
}

/** Builds the panel inside `container` once; it stays hidden until opened. */
export function mount(container: HTMLElement, onError: (error: unknown) => void): void {
  root = container;
  report = onError;
  const toolbar = el("header", "diff-toolbar");
  const label = el("label", "diff-range");
  rangePicker = el("select");
  rangePicker.setAttribute("aria-label", "Changes to show");
  rangePicker.addEventListener("change", () => {
    range = parseRange(rangePicker.value);
    confirming = null;
    restored = null;
    void refresh().catch(report);
  });
  label.append(el("span", "top-bar-label", "Changes"), rangePicker);
  actions = el("div", "diff-actions");
  toolbar.append(label, actions);
  summary = el("p", "diff-summary muted");
  files = el("div", "diff-files");
  root.append(toolbar, summary, files);
}

export function isOpen(): boolean {
  return chat !== null;
}

/**
 * Opens the panel on a chat's changes, its latest turn first, or reads
 * them again. `running` is whether the chat's turn runs.
 */
export async function show(shown: ChatId, running: boolean): Promise<void> {
  if (chat !== shown) {
    range = null;
    confirming = null;
    restored = null;
  }
  chat = shown;
  root.hidden = false;
  await refresh(running);
}

export function hide(): void {
  chat = null;
  range = null;
  confirming = null;
  restored = null;
  root.hidden = true;
}

async function refresh(running = false): Promise<void> {
  if (chat === null) return;
  const shown = chat;
  const read = ++readCount;
  const checkpoints = await service.listCheckpoints(shown);
  const latest = checkpoints.turns.at(-1);
  // A new turn moves the panel along, unless the developer picked another.
  const chosen: DiffRange =
    range ?? (latest === undefined ? { type: "branch" } : { type: "turn", turn: latest.turn });
  const known =
    chosen.type === "branch" || checkpoints.turns.some((turn) => turn.turn === chosen.turn);
  const current: DiffRange = known ? chosen : { type: "branch" };
  const diff = await service.readDiff(shown, current);
  if (read !== readCount || chat !== shown) return;
  renderRanges(checkpoints, current, running);
  const turn =
    current.type === "turn"
      ? checkpoints.turns.find((candidate) => candidate.turn === current.turn) ?? null
      : null;
  renderActions(shown, turn, running);
  renderSummary(diff, current, checkpoints.branchBase, turn);
  files.replaceChildren(...(diff.length === 0 ? [el("p", "empty", "No changes.")] : diff.map(renderFile)));
}

function rangeValue(range: DiffRange): string {
  return range.type === "branch" ? "branch" : `turn:${range.turn}`;
}

function parseRange(value: string): DiffRange {
  const turn = /^turn:(\d+)$/.exec(value);
  return turn === null ? { type: "branch" } : { type: "turn", turn: Number(turn[1]) };
}

function renderRanges(
  checkpoints: service.Checkpoints,
  current: DiffRange,
  running: boolean,
): void {
  const branch = el("option", "", `Whole branch against ${checkpoints.branchBase}`);
  branch.value = "branch";
  const turns = [...checkpoints.turns].reverse().map((turn) => {
    const unfinished = turn.after === null ? (running ? " (running)" : " (not finished)") : "";
    const option = el("option", "", `Turn ${turn.turn}: ${turn.title || "(no text)"}${unfinished}`);
    option.value = rangeValue({ type: "turn", turn: turn.turn });
    return option;
  });
  rangePicker.replaceChildren(branch, ...turns);
  rangePicker.value = rangeValue(current);
}

function renderActions(shown: ChatId, turn: TurnCheckpoints | null, running: boolean): void {
  if (turn === null) {
    actions.replaceChildren();
    return;
  }
  if (confirming !== null) {
    const moment = confirming;
    const question = el(
      "span",
      "diff-confirm",
      `Put every file back to how it was ${moment} turn ${turn.turn}? Changes made since the last checkpoint are lost unless they are committed.`,
    );
    const yes = button(
      "Restore",
      () =>
        void (async () => {
          confirming = null;
          await service.restoreCheckpoint(shown, turn.turn, moment);
          restored = `Restored the files to how they were ${moment} turn ${turn.turn}. The agent learns this with your next message.`;
          await refresh();
        })().catch(report),
      "primary",
    );
    const no = button("Cancel", () => {
      confirming = null;
      renderActions(shown, turn, running);
    });
    actions.replaceChildren(question, yes, no);
    return;
  }
  const ask = (moment: CheckpointMoment) => () => {
    confirming = moment;
    renderActions(shown, turn, running);
  };
  const before = button(`Restore to before turn ${turn.turn}`, ask("before"));
  const after = button(`Restore to after turn ${turn.turn}`, ask("after"));
  after.disabled = turn.after === null;
  for (const restore of [before, after]) {
    if (running) {
      restore.disabled = true;
      restore.title = "Wait for the turn to end, or interrupt it.";
    }
  }
  actions.replaceChildren(before, after);
}

function renderSummary(
  diff: FileDiff[],
  current: DiffRange,
  branchBase: string,
  turn: TurnCheckpoints | null,
): void {
  const { added, removed } = diff.reduce(
    (total, file) => {
      const counts = lineCounts(file);
      return { added: total.added + counts.added, removed: total.removed + counts.removed };
    },
    { added: 0, removed: 0 },
  );
  const scope =
    current.type === "branch"
      ? `Since the branch left ${branchBase}, also changes not committed yet.`
      : turn?.after === null
        ? "Since the turn started, up to the files now."
        : "What the turn changed.";
  const files = diff.length === 1 ? "1 file" : `${diff.length} files`;
  summary.textContent = `${scope} ${files} changed, +${added} −${removed}.`;
  if (restored !== null) summary.prepend(el("strong", "diff-restored", restored), " ");
}

function lineCounts(file: FileDiff): { added: number; removed: number } {
  if (file.patch.type !== "text") return { added: 0, removed: 0 };
  let added = 0;
  let removed = 0;
  for (const hunk of file.patch.hunks) {
    for (const line of hunk.lines) {
      if (line.type === "added") added += 1;
      if (line.type === "removed") removed += 1;
    }
  }
  return { added, removed };
}

function changeLabel(file: FileDiff): string {
  switch (file.change.type) {
    case "added":
      return "added";
    case "modified":
      return "modified";
    case "deleted":
      return "deleted";
    case "renamed":
      return `renamed from ${file.change.from}`;
  }
}

function renderFile(file: FileDiff): HTMLElement {
  const details = el("details", "diff-file");
  details.open = true;
  const header = el("summary", "diff-file-header");
  const counts = lineCounts(file);
  header.append(
    el("span", "diff-file-path", file.path),
    el("span", `diff-file-change ${file.change.type}`, changeLabel(file)),
    el("span", "diff-file-counts", `+${counts.added} −${counts.removed}`),
  );
  details.append(header);
  switch (file.patch.type) {
    case "binary":
      details.append(el("p", "diff-note muted", "Binary file; no lines to show."));
      break;
    case "too_large":
      details.append(el("p", "diff-note muted", `Too large to show: ${file.patch.lines} lines of diff.`));
      break;
    case "text": {
      if (file.patch.hunks.length === 0) {
        details.append(el("p", "diff-note muted", "Only the file mode changed."));
        break;
      }
      const table = el("table", "diff-lines");
      const body = el("tbody");
      for (const hunk of file.patch.hunks) {
        const row = el("tr", "diff-hunk");
        const cell = el("td", "", hunkHeading(hunk.lines, hunk.section));
        cell.colSpan = 3;
        row.append(cell);
        body.append(row, ...hunk.lines.map(renderLine));
      }
      table.append(body);
      details.append(table);
      break;
    }
  }
  return details;
}

function hunkHeading(lines: DiffLine[], section: string): string {
  const first = lines[0];
  const at = first === undefined ? "" : `line ${first.type === "removed" ? first.old : first.new}`;
  return [at, section].filter((part) => part !== "").join(" · ");
}

function renderLine(line: DiffLine): HTMLElement {
  const row = el("tr", `diff-line ${line.type}`);
  const old = line.type === "added" ? "" : String(line.old);
  const updated = line.type === "removed" ? "" : String(line.new);
  const sign = line.type === "added" ? "+" : line.type === "removed" ? "−" : " ";
  row.append(el("td", "diff-number", old), el("td", "diff-number", updated));
  const text = el("td", "diff-text");
  text.append(el("span", "diff-sign", sign), document.createTextNode(line.text));
  row.append(text);
  return row;
}
