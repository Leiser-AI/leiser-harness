// The wayfinder map view (#86): one map of a repository as a graph, from
// Start to Destination. Every ticket is a box with its type as icon and
// color and its state (closed, claimed, blocked, frontier); arrows go from
// a blocker to the ticket it blocks. Start points at the tickets nothing
// blocks, and the tickets that block nothing point at Destination. An open
// issue outside the map that blocks a ticket shows as a small box of its
// own.
//
// Every arrow points down a row: an arrow that spans rows passes through a
// gap of its own in each row between, so it never runs under a ticket.
//
// The view keeps no state: the harness service polls the map on the git
// host and reports changes, and the app renders it again.

import type { IssueRef, Ticket, TicketState, TicketType, WayfinderMap } from "./protocol";
import { icon, type IconName } from "./icons";

/** What clicking in the graph does; the app decides how. */
export interface MapActions {
  /** Opens the ticket on the git host. */
  openTicket(ticket: Ticket): void;
  /** Starts a chat on a frontier ticket. */
  startChat(ticket: Ticket): void;
}

export const TYPE_NAMES: Record<TicketType, string> = {
  research: "Research",
  prototype: "Prototype",
  grilling: "Grilling",
  task: "Task",
};

const TYPE_ICONS: Record<TicketType, IconName> = {
  research: "search",
  prototype: "wrench",
  grilling: "flame",
  task: "list",
};

/** An issue as the git host names it, such as `#12` or `owner/repo#12`. */
export function issueName(issue: IssueRef): string {
  return issue.type === "here" ? `#${issue.number}` : `${issue.repository}#${issue.number}`;
}

/** A ticket's state in a few words. */
export function stateText(state: TicketState): string {
  switch (state.type) {
    case "closed":
      return "closed";
    case "claimed":
      return `claimed by ${state.assignees.join(", ")}`;
    case "blocked":
      return `blocked by ${state.open_blockers.map(issueName).join(", ")}`;
    case "frontier":
      return "on the frontier";
  }
}

/** How many of the map's tickets can be taken now. */
export function frontierCount(map: WayfinderMap): number {
  return map.tickets.filter((ticket) => ticket.state.type === "frontier").length;
}

const ROW_HEIGHT = 52;
const GAP_Y = 56;
const GAP_X = 24;
const PADDING = 16;

/** Something that takes a place in a row of the graph. */
type Place =
  | { kind: "start" }
  | { kind: "destination" }
  | { kind: "ticket"; ticket: Ticket }
  | { kind: "outside"; issue: IssueRef }
  /** Where an arrow that spans rows crosses this one. */
  | { kind: "gap" };

const SIZES: Record<Place["kind"], { width: number; height: number }> = {
  start: { width: 132, height: 32 },
  destination: { width: 132, height: 32 },
  ticket: { width: 232, height: ROW_HEIGHT },
  outside: { width: 132, height: 28 },
  gap: { width: 8, height: ROW_HEIGHT },
};

interface Box {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** An arrow from one place to another, through the gaps between. */
interface Arrow {
  through: string[];
  className: string;
}

interface Layout {
  width: number;
  height: number;
  places: Map<string, Place>;
  boxes: Map<string, Box>;
  arrows: Arrow[];
}

/** Each item with the one after it. */
function pairs<T>(items: T[]): [T, T][] {
  return items.slice(1).map((item, index) => [items[index] as T, item]);
}

const START = "start";
const DESTINATION = "destination";
const ticketId = (number: number) => `ticket-${number}`;
const outsideId = (issue: IssueRef) => `outside-${issueName(issue)}`;

/** The map's places and the arrows between them, before they are laid out in rows. */
function graph(map: WayfinderMap) {
  const places = new Map<string, Place>([
    [START, { kind: "start" }],
    [DESTINATION, { kind: "destination" }],
  ]);
  /** Arrows into each place, by the place they come from, with their style. */
  const into = new Map<string, { from: string; className: string }[]>();
  const arrow = (from: string, to: string, className: string) =>
    (into.get(to) ?? into.set(to, []).get(to)!).push({ from, className });
  const inMap = new Set(map.tickets.map((ticket) => ticket.number));
  const closed = new Set(
    map.tickets.filter((ticket) => ticket.state.type === "closed").map((ticket) => ticket.number),
  );
  const blocking = new Set(map.tickets.flatMap((ticket) => ticket.blocked_by));
  for (const ticket of map.tickets) {
    places.set(ticketId(ticket.number), { kind: "ticket", ticket });
  }
  for (const ticket of map.tickets) {
    const id = ticketId(ticket.number);
    for (const blocker of ticket.blocked_by) {
      arrow(ticketId(blocker), id, closed.has(blocker) ? "cleared" : "blocking");
    }
    const outside =
      ticket.state.type === "blocked"
        ? ticket.state.open_blockers.filter((issue) => issue.type !== "here" || !inMap.has(issue.number))
        : [];
    for (const issue of outside) {
      const from = outsideId(issue);
      if (!places.has(from)) {
        places.set(from, { kind: "outside", issue });
        arrow(START, from, "from-start");
      }
      arrow(from, id, "blocking outside");
    }
    if (ticket.blocked_by.length === 0 && outside.length === 0) arrow(START, id, "from-start");
    if (!blocking.has(ticket.number)) {
      arrow(id, DESTINATION, closed.has(ticket.number) ? "cleared" : "to-destination");
    }
  }
  if (map.tickets.length === 0) arrow(START, DESTINATION, "from-start");
  return { places, into };
}

/**
 * Puts each place one row below the lowest place with an arrow into it, so
 * every arrow points down, and Destination in a row of its own at the
 * bottom. Within a row, places sit near the middle of where their arrows
 * come from, else in map order.
 */
function layout(map: WayfinderMap): Layout {
  const { places, into } = graph(map);
  const depth = new Map<string, number>([[START, 0]]);
  const visiting = new Set<string>();
  /** Arrows that would point up: the git host's dependencies have a cycle. */
  const cycles = new Set<string>();
  const depthOf = (id: string): number => {
    const known = depth.get(id);
    if (known !== undefined) return known;
    visiting.add(id);
    let found = 1;
    for (const { from } of into.get(id) ?? []) {
      if (visiting.has(from)) {
        cycles.add(`${from}>${id}`);
        continue;
      }
      found = Math.max(found, depthOf(from) + 1);
    }
    visiting.delete(id);
    depth.set(id, found);
    return found;
  };
  for (const id of places.keys()) if (id !== DESTINATION) depthOf(id);
  depth.set(DESTINATION, Math.max(...depth.values()) + 1);

  const rows: string[][] = [];
  const addToRow = (id: string, row: number) => (rows[row] ??= []).push(id);
  for (const id of places.keys()) addToRow(id, depth.get(id)!);
  /** The place one row up that each place's arrows come from, gaps included. */
  const above = new Map<string, string[]>();
  const arrows: Arrow[] = [];
  let gaps = 0;
  for (const [to, sources] of into) {
    for (const { from, className } of sources) {
      if (cycles.has(`${from}>${to}`)) {
        arrows.push({ through: [from, to], className: `${className} cycle` });
        continue;
      }
      const through = [from];
      for (let row = depth.get(from)! + 1; row < depth.get(to)!; row++) {
        const gap = `gap-${gaps++}`;
        places.set(gap, { kind: "gap" });
        addToRow(gap, row);
        through.push(gap);
      }
      through.push(to);
      for (const [upper, lower] of pairs(through)) {
        (above.get(lower) ?? above.set(lower, []).get(lower)!).push(upper);
      }
      arrows.push({ through, className });
    }
  }

  const order = new Map<string, number>();
  rows.forEach((row, index) => {
    if (index > 0) {
      const middle = (id: string): number => {
        const places = (above.get(id) ?? []).flatMap((from) => order.get(from) ?? []);
        return places.length === 0
          ? Number.POSITIVE_INFINITY
          : places.reduce((sum, place) => sum + place, 0) / places.length;
      };
      // `sort` is stable: ties keep map order.
      row.sort((a, b) => middle(a) - middle(b));
    }
    const width = (id: string) => SIZES[places.get(id)!.kind].width;
    let left = -(row.reduce((sum, id) => sum + width(id), 0) + (row.length - 1) * GAP_X) / 2;
    for (const id of row) {
      order.set(id, left + width(id) / 2);
      left += width(id) + GAP_X;
    }
  });

  const halfWidth = Math.max(
    ...rows.flatMap((row) =>
      row.map((id) => Math.abs(order.get(id)!) + SIZES[places.get(id)!.kind].width / 2),
    ),
  );
  const width = 2 * (halfWidth + PADDING);
  const boxes = new Map<string, Box>();
  rows.forEach((row, index) => {
    const top = PADDING + index * (ROW_HEIGHT + GAP_Y);
    for (const id of row) {
      const size = SIZES[places.get(id)!.kind];
      boxes.set(id, {
        x: width / 2 + order.get(id)! - size.width / 2,
        y: top + (ROW_HEIGHT - size.height) / 2,
        width: size.width,
        height: size.height,
      });
    }
  });
  return {
    width,
    height: 2 * PADDING + rows.length * ROW_HEIGHT + (rows.length - 1) * GAP_Y,
    places,
    boxes,
    arrows,
  };
}

const SVG = "http://www.w3.org/2000/svg";

function svg<K extends keyof SVGElementTagNameMap>(tag: K, attributes: Record<string, string | number>) {
  const element = document.createElementNS(SVG, tag);
  for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, String(value));
  return element;
}

/** An arrow from the bottom of its first box to the top of its last, straight down through its gaps. */
function arrowPath(boxes: Box[], className: string): SVGPathElement {
  const segments = pairs(boxes).map(([from, to], index) => {
    const x1 = from.x + from.width / 2;
    const y1 = from.y + from.height;
    const x2 = to.x + to.width / 2;
    const y2 = to.y;
    const bend = Math.max(GAP_Y / 2, Math.abs(y2 - y1) / 2);
    const start = index === 0 ? `M${x1} ${y1} ` : "";
    // Through a gap, the arrow goes on straight to the gap's bottom.
    const through = index < boxes.length - 2 ? ` L${x2} ${to.y + to.height}` : "";
    return `${start}C${x1} ${y1 + bend} ${x2} ${y2 - bend} ${x2} ${y2}${through}`;
  });
  const d = segments.join(" ");
  return svg("path", { d, class: `wayfinder-arrow ${className}`, "marker-end": "url(#wayfinder-arrowhead)" });
}

function place(element: HTMLElement, box: Box): void {
  element.style.left = `${box.x}px`;
  element.style.top = `${box.y}px`;
  element.style.width = `${box.width}px`;
  element.style.height = `${box.height}px`;
}

function endBox(label: string, box: Box): HTMLElement {
  const end = document.createElement("div");
  end.className = "wayfinder-end";
  end.append(icon("flag"), document.createTextNode(label));
  place(end, box);
  return end;
}

function outsideBox(issue: IssueRef, box: Box): HTMLElement {
  const outside = document.createElement("div");
  outside.className = "wayfinder-outside";
  outside.textContent = issueName(issue);
  outside.title = `${issueName(issue)} is open and not on this map`;
  place(outside, box);
  return outside;
}

function ticketBox(ticket: Ticket, box: Box, actions: MapActions): HTMLElement {
  const type = ticket.ticket_type ?? "untyped";
  const element = document.createElement("div");
  element.className = `wayfinder-ticket type-${type} state-${ticket.state.type}`;
  place(element, box);

  const open = document.createElement("button");
  open.type = "button";
  open.className = "wayfinder-ticket-open";
  const typeName = ticket.ticket_type === null ? "No type" : TYPE_NAMES[ticket.ticket_type];
  open.title = `${ticket.title}\n#${ticket.number} · ${typeName} · ${stateText(ticket.state)}\nOpen it on the git host`;
  open.setAttribute(
    "aria-label",
    `${ticket.title}, #${ticket.number}, ${typeName}, ${stateText(ticket.state)}. Open it on the git host`,
  );
  const badge = document.createElement("span");
  badge.className = "wayfinder-type";
  badge.append(icon(ticket.ticket_type === null ? "circle" : TYPE_ICONS[ticket.ticket_type]));
  const text = document.createElement("span");
  text.className = "wayfinder-ticket-text";
  const title = document.createElement("span");
  title.className = "wayfinder-ticket-title";
  title.textContent = ticket.title;
  const facts = document.createElement("span");
  facts.className = "wayfinder-ticket-facts";
  facts.textContent = `#${ticket.number} · ${stateText(ticket.state)}`;
  text.append(title, facts);
  open.append(badge, text);
  if (ticket.state.type === "closed") open.append(icon("check"));
  open.addEventListener("click", () => actions.openTicket(ticket));
  element.append(open);

  if (ticket.state.type === "frontier") {
    const start = document.createElement("button");
    start.type = "button";
    start.className = "wayfinder-start-chat";
    start.title = "Start a chat on this ticket";
    start.setAttribute("aria-label", `Start a chat on ${ticket.title}`);
    start.append(icon("chat"));
    start.addEventListener("click", () => actions.startChat(ticket));
    element.append(start);
  }
  return element;
}

/** The map as a graph, from Start to Destination. */
export function renderGraph(map: WayfinderMap, actions: MapActions): HTMLElement {
  const laid = layout(map);
  const container = document.createElement("div");
  container.className = "wayfinder-graph";
  container.setAttribute("role", "group");
  container.setAttribute("aria-label", `${map.title}, from start to destination`);
  container.style.width = `${laid.width}px`;
  container.style.height = `${laid.height}px`;

  const arrows = svg("svg", { class: "wayfinder-arrows", width: laid.width, height: laid.height });
  arrows.setAttribute("aria-hidden", "true");
  const marker = svg("marker", {
    id: "wayfinder-arrowhead",
    viewBox: "0 0 8 8",
    refX: 7,
    refY: 4,
    markerWidth: 8,
    markerHeight: 8,
    orient: "auto-start-reverse",
  });
  marker.append(svg("path", { d: "M0 0 8 4 0 8z", class: "wayfinder-arrowhead" }));
  const defs = svg("defs", {});
  defs.append(marker);
  arrows.append(defs);
  for (const arrow of laid.arrows) {
    arrows.append(arrowPath(arrow.through.map((id) => laid.boxes.get(id)!), arrow.className));
  }
  container.append(arrows);

  for (const [id, placed] of laid.places) {
    const box = laid.boxes.get(id)!;
    switch (placed.kind) {
      case "start":
        container.append(endBox("Start", box));
        break;
      case "destination":
        container.append(endBox("Destination", box));
        break;
      case "ticket":
        container.append(ticketBox(placed.ticket, box, actions));
        break;
      case "outside":
        container.append(outsideBox(placed.issue, box));
        break;
      case "gap":
        break;
    }
  }
  return container;
}
