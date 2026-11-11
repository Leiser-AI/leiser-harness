// Menus that open from a button, such as the composer's pickers. The UI
// renders its buttons again on every service event, so an open menu does
// not belong to the button's element: it lives in a layer of its own and
// is tied to the button by a key that stays the same across renders.

import { icon, type IconName } from "./icons";

/** One choice in a menu. */
export interface MenuItem {
  label: string;
  /** A second, quieter line that says what the choice does. */
  description?: string;
  icon?: IconName;
  /** Marks the current choice of a group of alternatives. */
  selected?: boolean;
  disabled?: boolean;
  onSelect: () => void;
}

/** Items that belong together, under an optional heading. */
export interface MenuSection {
  heading?: string;
  items: MenuItem[];
}

export interface MenuButtonOptions {
  /** Names the menu across renders; unique among the buttons shown together. */
  key: string;
  /** What the button shows: the current choice. */
  label: string;
  /** A quieter addition to the label, such as the subscription next to the agent CLI. */
  detail?: string;
  /** What the button chooses, for screen readers and the tooltip. */
  name: string;
  /** More about the choice, added to the tooltip. */
  hint?: string;
  icon?: IconName;
  sections: MenuSection[];
  /** A line below the items, such as what holds whatever is chosen. */
  note?: string;
  disabled?: boolean;
  className?: string;
  /** Shows the label alone, as part of a sentence, without the chevron. */
  inline?: boolean;
}

interface OpenMenu {
  key: string;
  element: HTMLElement;
}

let open: OpenMenu | null = null;

function button(key: string): HTMLButtonElement | null {
  return document.querySelector<HTMLButtonElement>(`button[data-menu="${CSS.escape(key)}"]`);
}

/** Closes the open menu, if any; `focus` puts the cursor back on its button. */
export function close(focus = false): void {
  if (open === null) return;
  const { key, element } = open;
  open = null;
  element.remove();
  const opener = button(key);
  opener?.setAttribute("aria-expanded", "false");
  if (focus) opener?.focus();
}

/** After a render: closes the open menu when its button is gone or disabled. */
export function sync(): void {
  if (open === null) return;
  const opener = button(open.key);
  if (opener === null || opener.disabled) {
    close();
    return;
  }
  opener.setAttribute("aria-expanded", "true");
}

function items(menu: HTMLElement): HTMLButtonElement[] {
  return Array.from(menu.querySelectorAll<HTMLButtonElement>("button:not(:disabled)"));
}

function show(opener: HTMLButtonElement, options: MenuButtonOptions): void {
  close();
  const menu = document.createElement("div");
  menu.className = "menu";
  menu.setAttribute("role", "menu");
  menu.setAttribute("aria-label", options.name);
  for (const section of options.sections) {
    if (section.items.length === 0) continue;
    const group = document.createElement("div");
    group.className = "menu-section";
    group.setAttribute("role", "group");
    if (section.heading !== undefined) {
      const heading = document.createElement("div");
      heading.className = "menu-heading";
      heading.textContent = section.heading;
      group.setAttribute("aria-label", section.heading);
      group.append(heading);
    }
    for (const item of section.items) group.append(menuItem(item));
    menu.append(group);
  }
  if (options.note !== undefined) {
    const note = document.createElement("p");
    note.className = "menu-note";
    note.textContent = options.note;
    menu.append(note);
  }
  menu.addEventListener("keydown", (event) => {
    const all = items(menu);
    const at = all.indexOf(document.activeElement as HTMLButtonElement);
    const move = (to: number): void => {
      event.preventDefault();
      all[(to + all.length) % all.length]?.focus();
    };
    if (event.key === "ArrowDown") move(at + 1);
    else if (event.key === "ArrowUp") move(at - 1);
    else if (event.key === "Home") move(0);
    else if (event.key === "End") move(all.length - 1);
    else if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      close(true);
    } else if (event.key === "Tab") close(true);
  });
  document.body.append(menu);
  place(menu, opener);
  open = { key: options.key, element: menu };
  opener.setAttribute("aria-expanded", "true");
  const all = items(menu);
  (all.find((item) => item.getAttribute("aria-checked") === "true") ?? all[0])?.focus();
}

function menuItem(item: MenuItem): HTMLButtonElement {
  const element = document.createElement("button");
  element.type = "button";
  element.className = "menu-item";
  element.disabled = item.disabled ?? false;
  if (item.selected === undefined) {
    element.setAttribute("role", "menuitem");
  } else {
    element.setAttribute("role", "menuitemradio");
    element.setAttribute("aria-checked", String(item.selected));
  }
  if (item.icon !== undefined) element.append(icon(item.icon));
  const text = document.createElement("span");
  text.className = "menu-item-text";
  const label = document.createElement("span");
  label.className = "menu-item-label";
  label.textContent = item.label;
  text.append(label);
  if (item.description !== undefined) {
    const description = document.createElement("span");
    description.className = "menu-item-description";
    description.textContent = item.description;
    text.append(description);
  }
  element.append(text);
  if (item.selected === true) element.append(icon("check"));
  element.addEventListener("click", () => {
    close(true);
    item.onSelect();
  });
  return element;
}

/** The gap between a button and its menu, and between the menu and the window's edge. */
const GAP = 6;

/** Puts the menu where there is more room: above or below its button, inside the window. */
function place(menu: HTMLElement, opener: HTMLElement): void {
  const anchor = opener.getBoundingClientRect();
  const above = anchor.top - GAP;
  const below = window.innerHeight - anchor.bottom - GAP;
  const upwards = above > below;
  menu.style.maxHeight = `${Math.max(120, (upwards ? above : below) - GAP)}px`;
  const { width, height } = menu.getBoundingClientRect();
  const left = Math.min(anchor.left, window.innerWidth - width - GAP);
  menu.style.left = `${Math.max(GAP, left)}px`;
  menu.style.top = `${upwards ? Math.max(GAP, anchor.top - GAP - height) : anchor.bottom + GAP}px`;
}

/** A button that shows the current choice and opens a menu of the others. */
export function menuButton(options: MenuButtonOptions): HTMLButtonElement {
  const element = document.createElement("button");
  element.type = "button";
  element.className = `menu-button ${options.className ?? ""}`.trim();
  element.dataset.menu = options.key;
  element.disabled = options.disabled ?? false;
  element.setAttribute("aria-haspopup", "menu");
  element.setAttribute("aria-expanded", String(open?.key === options.key));
  const choice = options.detail === undefined ? options.label : `${options.label}, ${options.detail}`;
  element.setAttribute("aria-label", `${options.name}: ${choice}`);
  element.title = options.hint === undefined ? options.name : `${options.name}: ${options.hint}`;
  if (options.icon !== undefined) element.append(icon(options.icon));
  const label = document.createElement("span");
  label.className = "menu-button-label";
  label.textContent = options.label;
  element.append(label);
  if (options.detail !== undefined) {
    const detail = document.createElement("span");
    detail.className = "menu-button-detail";
    detail.textContent = options.detail;
    element.append(detail);
  }
  if (options.inline !== true) element.append(icon("chevronDown"));
  element.addEventListener("click", () => {
    if (open?.key === options.key) close();
    else show(element, options);
  });
  return element;
}

document.addEventListener("pointerdown", (event) => {
  if (open === null || !(event.target instanceof Node)) return;
  if (open.element.contains(event.target)) return;
  // The menu's own button closes it in its click handler.
  if (button(open.key)?.contains(event.target)) return;
  close();
});
window.addEventListener("resize", () => close());
