// Tiny DOM helpers. Text always goes through textContent, never innerHTML,
// except for the markdown renderer's escaped output.

type Child = Node | string | number | false | null | undefined;
type Attributes = Record<string, string | number | boolean | EventListener | undefined | null>;

export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attributes: Attributes | null = null,
  ...children: Array<Child | Child[]>
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  if (attributes) {
    for (const [key, value] of Object.entries(attributes)) {
      if (value === undefined || value === null || value === false) continue;
      if (key.startsWith("on") && typeof value === "function") {
        element.addEventListener(key.slice(2).toLowerCase(), value as EventListener);
      } else if (key === "class") element.className = String(value);
      else if (key === "html") element.innerHTML = String(value);
      else if (value === true) element.setAttribute(key, "");
      else element.setAttribute(key, String(value));
    }
  }
  append(element, children);
  return element;
}

export function append(parent: Node, children: Array<Child | Child[]>): void {
  for (const child of children.flat()) {
    if (child === undefined || child === null || child === false) continue;
    parent.appendChild(typeof child === "object" ? child : document.createTextNode(String(child)));
  }
}

export function clear(element: Element): void {
  while (element.firstChild) element.firstChild.remove();
}

/** Runs a DOM update inside a view transition when the browser supports it. */
export function transition(update: () => void, kind?: string): void {
  const doc = document as Document & {
    startViewTransition?: (callback: () => void) => { finished: Promise<void> };
  };
  if (!doc.startViewTransition || matchMedia("(prefers-reduced-motion: reduce)").matches) {
    update();
    return;
  }
  if (kind) document.documentElement.dataset.transition = kind;
  const running = doc.startViewTransition(update);
  void running.finished.finally(() => {
    if (document.documentElement.dataset.transition === kind) delete document.documentElement.dataset.transition;
  });
}

export async function copyText(text: string): Promise<boolean> {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    const area = h("textarea", { style: "position:fixed;opacity:0" });
    area.value = text;
    document.body.append(area);
    area.select();
    const ok = document.execCommand("copy");
    area.remove();
    return ok;
  }
}
