// Small line icons for buttons and labels, drawn on a 16×16 grid. They are
// decoration: the button or label that holds one carries the text for
// screen readers (`aria-label` or visible text).

const PATHS = {
  plus: "M8 3v10M3 8h10",
  folderPlus: "M2 4.5A1.5 1.5 0 0 1 3.5 3h3l1.5 1.5h4.5A1.5 1.5 0 0 1 14 6v5.5a1.5 1.5 0 0 1-1.5 1.5h-9A1.5 1.5 0 0 1 2 11.5zM8 6.5v4M6 8.5h4",
  home: "M2.5 7.5 8 3l5.5 4.5M4 6.5V13h3v-3h2v3h3V6.5",
  blocks: "M3 3h4v4H3zM9 3h4v4H9zM3 9h4v4H3zM9 9h4v4H9z",
  key: "M10 2.5a3.5 3.5 0 1 1-2.9 5.46L3 12v1.5h2V12h1.5v-1.5H8l.04-.04A3.5 3.5 0 0 1 10 2.5zM10.75 5.25h.01",
  check: "M3.5 8.5 6.5 11.5 12.5 4.5",
  close: "M4 4l8 8M12 4l-8 8",
  stop: "M5 5h6v6H5z",
  arrowUp: "M8 13V3M4 7l4-4 4 4",
  terminal: "M3 4.5 6 8l-3 3.5M8 12h5",
  shield: "M8 2 3 4v4c0 3 2.2 5 5 6 2.8-1 5-3 5-6V4z",
  alert: "M8 2.5 14 13H2zM8 6.5v3M8 11.25h.01",
  branch: "M5 3v10M5 13a1.5 1.5 0 1 0 0-.01M11 3a1.5 1.5 0 1 0 0 .01M11 4.5c0 3-6 2.5-6 5.5",
  chevron: "M6 4l4 4-4 4",
  diff: "M5 2.5v5M2.5 5h5M8.5 12h5M3 14 13 2",
  wrench: "M10.5 2.5a3 3 0 0 0-2.8 4.1L2.5 11.8l1.7 1.7 5.2-5.2a3 3 0 0 0 4.1-2.8l-1.8 1.8-1.9-.5-.5-1.9z",
  clipboard: "M5.5 3H4.5A1.5 1.5 0 0 0 3 4.5v8A1.5 1.5 0 0 0 4.5 14h7a1.5 1.5 0 0 0 1.5-1.5v-8A1.5 1.5 0 0 0 11.5 3h-1M6 2h4v2H6z",
  map: "M2 4l4-1.5 4 1.5 4-1.5v9.5l-4 1.5-4-1.5-4 1.5zM6 2.5v9.5M10 4v9.5",
  flag: "M4 14V2.5M4 3h7.5L10 5.5 11.5 8H4",
  search: "M7 3a4 4 0 1 0 0 8 4 4 0 0 0 0-8zM10 10l3.5 3.5",
  flame: "M8 2c.5 2.5 3.5 3.8 3.5 7a3.5 3.5 0 0 1-7 0c0-1.6.8-2.6 1.6-3.4.2 1.2.8 1.9 1.6 2.2C7.3 6 7.3 4 8 2z",
  list: "M6 4h7M6 8h7M6 12h7M3 4h.01M3 8h.01M3 12h.01",
  circle: "M8 4.5a3.5 3.5 0 1 0 0 7 3.5 3.5 0 0 0 0-7z",
  external: "M9 3h4v4M13 3 7.5 8.5M11 9.5V13H3V5h3.5",
  chat: "M3 3.5h10v7H7l-3 2.5v-2.5H3z",
} as const;

export type IconName = keyof typeof PATHS;

const SVG = "http://www.w3.org/2000/svg";

/** An inline icon in the current text color. */
export function icon(name: IconName): SVGSVGElement {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("class", `icon icon-${name}`);
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  const path = document.createElementNS(SVG, "path");
  path.setAttribute("d", PATHS[name]);
  svg.append(path);
  return svg;
}
