// The wayfinder map view (#86): one map of a repository as a graph, from
// Start to Destination. Every ticket is a node with its type as icon and
// color and its state (closed, claimed, blocked, frontier); arrows go from
// a blocker to the ticket it blocks. Start points at the tickets nothing in
// the map blocks, and the tickets that block nothing point at Destination.
//
// The view keeps no state: the harness service polls the map on the git
// host and reports changes, and the app renders it again.

import type { Ticket, TicketKind, TicketState, WayfinderMap } from "./protocol";
import { icon, type IconName } from "./icons";

/** What clicking in the graph does; the app decides how. */
export interface MapActions {
  /** Opens the ticket on the git host. */
  openTicket(ticket: Ticket): void;
  /** Starts a chat on a frontier ticket. */
  startChat(ticket: Ticket): void;
}

export const KIND_NAMES: Record<TicketKind, string> = {
  research: "Research",
  prototype: "Prototype",
  grilling: "Grilling",
  task: "Task",
};

const KIND_ICONS: Record<TicketKind, IconName> = {
  research: "search",
  prototype: "wrench",
  grilling: "flame",
  task: "list",
};

/** A ticket's state in a few words. */
export function stateText(state: TicketState): string {
  switch (state.type) {
    case "closed":
      return "closed";
    case "claimed":
      return `claimed by ${state.assignees.join(", ")}`;
    case "blocked":
      return `blocked by ${state.open_blockers.join(", ")}`;
    case "frontier":
      return "on the frontier";
  }
}

/** How many of the map's tickets can be taken now. */
export function frontierCount(map: WayfinderMap): number {
  return map.tickets.filter((ticket) => ticket.state.type === "frontier").length;
}

const NODE_WIDTH = 232;
const NODE_HEIGHT = 52;
const END_WIDTH = 132;
const END_HEIGHT = 32;
const GAP_X = 24;
const GAP_Y = 56;
const PADDING = 16;

interface Box {
  x: number;
  y: number;
  width: number;
  height: number;
}

interface Layout {
  width: number;
  height: number;
  start: Box;
  destination: Box;
  tickets: Map<number, Box>;
}

/**
 * Puts each ticket one row below its lowest blocker, so every arrow points
 * down. Within a row, tickets sit near the middle of their blockers, else
 * in map order.
 */
function layout(map: WayfinderMap): Layout {
  const byNumber = new Map(map.tickets.map((ticket) => [ticket.number, ticket]));
  const depth = new Map<number, number>();
  const visiting = new Set<number>();
  const depthOf = (ticket: Ticket): number => {
    const known = depth.get(ticket.number);
    if (known !== undefined) return known;
    // A cycle in the git host's dependencies: cut it here.
    if (visiting.has(ticket.number)) return 0;
    visiting.add(ticket.number);
    let found = 0;
    for (const blocker of ticket.blocked_by) {
      const blocking = byNumber.get(blocker);
      if (blocking !== undefined) found = Math.max(found, depthOf(blocking) + 1);
    }
    visiting.delete(ticket.number);
    depth.set(ticket.number, found);
    return found;
  };
  const rows: Ticket[][] = [];
  for (const ticket of map.tickets) {
    const row = depthOf(ticket);
    (rows[row] ??= []).push(ticket);
  }
  const order = new Map<number, number>();
  rows.forEach((row, index) => {
    if (index > 0) {
      const middle = (ticket: Ticket): number => {
        const places = ticket.blocked_by.flatMap((blocker) => order.get(blocker) ?? []);
        return places.length === 0
          ? Number.POSITIVE_INFINITY
          : places.reduce((sum, place) => sum + place, 0) / places.length;
      };
      // `sort` is stable: ties keep map order.
      row.sort((a, b) => middle(a) - middle(b));
    }
    row.forEach((ticket, place) => order.set(ticket.number, place - (row.length - 1) / 2));
  });

  const widest = Math.max(1, ...rows.map((row) => row.length));
  const width = Math.max(END_WIDTH, widest * NODE_WIDTH + (widest - 1) * GAP_X) + 2 * PADDING;
  const rowTop = (row: number) => PADDING + END_HEIGHT + GAP_Y + row * (NODE_HEIGHT + GAP_Y);
  const tickets = new Map<number, Box>();
  rows.forEach((row, index) => {
    const rowWidth = row.length * NODE_WIDTH + (row.length - 1) * GAP_X;
    const left = (width - rowWidth) / 2;
    row.forEach((ticket, place) =>
      tickets.set(ticket.number, {
        x: left + place * (NODE_WIDTH + GAP_X),
        y: rowTop(index),
        width: NODE_WIDTH,
        height: NODE_HEIGHT,
      }),
    );
  });
  const end = (y: number): Box => ({ x: (width - END_WIDTH) / 2, y, width: END_WIDTH, height: END_HEIGHT });
  const destinationTop = rowTop(rows.length);
  return {
    width,
    height: destinationTop + END_HEIGHT + PADDING,
    start: end(PADDING),
    destination: end(destinationTop),
    tickets,
  };
}

const SVG = "http://www.w3.org/2000/svg";

function svg<K extends keyof SVGElementTagNameMap>(tag: K, attributes: Record<string, string | number>) {
  const element = document.createElementNS(SVG, tag);
  for (const [name, value] of Object.entries(attributes)) element.setAttribute(name, String(value));
  return element;
}

/** An arrow from the bottom of `from` to the top of `to`. */
function arrow(from: Box, to: Box, className: string): SVGPathElement {
  const x1 = from.x + from.width / 2;
  const y1 = from.y + from.height;
  const x2 = to.x + to.width / 2;
  const y2 = to.y;
  const bend = Math.max(GAP_Y / 2, (y2 - y1) / 2);
  return svg("path", {
    d: `M${x1} ${y1} C${x1} ${y1 + bend} ${x2} ${y2 - bend} ${x2} ${y2}`,
    class: `wayfinder-edge ${className}`,
    "marker-end": "url(#wayfinder-arrow)",
  });
}

function place(element: HTMLElement, box: Box): void {
  element.style.left = `${box.x}px`;
  element.style.top = `${box.y}px`;
  element.style.width = `${box.width}px`;
  element.style.height = `${box.height}px`;
}

function endNode(label: string, box: Box): HTMLElement {
  const node = document.createElement("div");
  node.className = "wayfinder-end";
  node.append(icon("flag"), document.createTextNode(label));
  place(node, box);
  return node;
}

function ticketNode(ticket: Ticket, box: Box, actions: MapActions): HTMLElement {
  const kind = ticket.kind ?? "untyped";
  const node = document.createElement("div");
  node.className = `wayfinder-ticket kind-${kind} state-${ticket.state.type}`;
  place(node, box);

  const open = document.createElement("button");
  open.type = "button";
  open.className = "wayfinder-ticket-open";
  const typeName = ticket.kind === null ? "No type" : KIND_NAMES[ticket.kind];
  open.title = `${ticket.title}\n#${ticket.number} · ${typeName} · ${stateText(ticket.state)}\nOpen it on the git host`;
  open.setAttribute(
    "aria-label",
    `${ticket.title}, #${ticket.number}, ${typeName}, ${stateText(ticket.state)}. Open it on the git host`,
  );
  const badge = document.createElement("span");
  badge.className = "wayfinder-kind";
  badge.append(icon(ticket.kind === null ? "circle" : KIND_ICONS[ticket.kind]));
  const text = document.createElement("span");
  text.className = "wayfinder-ticket-text";
  const title = document.createElement("span");
  title.className = "wayfinder-ticket-title";
  title.textContent = ticket.title;
  const meta = document.createElement("span");
  meta.className = "wayfinder-ticket-meta";
  meta.textContent = `#${ticket.number} · ${stateText(ticket.state)}`;
  text.append(title, meta);
  open.append(badge, text);
  if (ticket.state.type === "closed") open.append(icon("check"));
  open.addEventListener("click", () => actions.openTicket(ticket));
  node.append(open);

  if (ticket.state.type === "frontier") {
    const start = document.createElement("button");
    start.type = "button";
    start.className = "wayfinder-start-chat";
    start.title = "Start a chat on this ticket";
    start.setAttribute("aria-label", `Start a chat on ${ticket.title}`);
    start.append(icon("chat"));
    start.addEventListener("click", () => actions.startChat(ticket));
    node.append(start);
  }
  return node;
}

/** The map as a graph, from Start to Destination. */
export function renderGraph(map: WayfinderMap, actions: MapActions): HTMLElement {
  const placed = layout(map);
  const graph = document.createElement("div");
  graph.className = "wayfinder-graph";
  graph.setAttribute("role", "group");
  graph.setAttribute("aria-label", `${map.title}, from start to destination`);
  graph.style.width = `${placed.width}px`;
  graph.style.height = `${placed.height}px`;

  const edges = svg("svg", { class: "wayfinder-edges", width: placed.width, height: placed.height });
  edges.setAttribute("aria-hidden", "true");
  const marker = svg("marker", {
    id: "wayfinder-arrow",
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
  edges.append(defs);

  const inMap = new Set(map.tickets.map((ticket) => ticket.number));
  const blocking = new Set(map.tickets.flatMap((ticket) => ticket.blocked_by));
  const closed = new Set(
    map.tickets.filter((ticket) => ticket.state.type === "closed").map((ticket) => ticket.number),
  );
  for (const ticket of map.tickets) {
    const box = placed.tickets.get(ticket.number);
    if (box === undefined) continue;
    const blockers = ticket.blocked_by.filter((blocker) => inMap.has(blocker));
    if (blockers.length === 0) edges.append(arrow(placed.start, box, "from-start"));
    for (const blocker of blockers) {
      const from = placed.tickets.get(blocker);
      if (from !== undefined) edges.append(arrow(from, box, closed.has(blocker) ? "cleared" : "blocking"));
    }
    if (!blocking.has(ticket.number)) {
      edges.append(arrow(box, placed.destination, closed.has(ticket.number) ? "cleared" : "to-destination"));
    }
  }
  if (map.tickets.length === 0) edges.append(arrow(placed.start, placed.destination, "from-start"));

  graph.append(edges, endNode("Start", placed.start), endNode("Destination", placed.destination));
  for (const ticket of map.tickets) {
    const box = placed.tickets.get(ticket.number);
    if (box !== undefined) graph.append(ticketNode(ticket, box, actions));
  }
  return graph;
}
