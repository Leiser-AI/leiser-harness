// What the UI shows differently on the developer's platform.

/** Whether the app runs on macOS, where shortcuts take the Command key. */
export const ON_MAC = navigator.platform.startsWith("Mac");

/** How a shortcut of the platform's main modifier and `key` shows, such as ⌘S. */
export function shortcutLabel(key: string): string {
  return ON_MAC ? `⌘${key}` : `Ctrl+${key}`;
}
