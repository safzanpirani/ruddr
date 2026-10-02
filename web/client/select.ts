// A themed replacement for <select>. The native control opens an OS menu that
// ignores the page theme, so this draws its own popover from the palette.
import { h } from "./dom";

export interface SelectOption {
  value: string;
  label: string;
  hint?: string;
}

export class ThemedSelect {
  readonly element: HTMLButtonElement;
  private options: SelectOption[] = [];
  private current = "";
  private listeners: Array<(value: string) => void> = [];
  private popover?: HTMLElement;
  private active = 0;
  private teardown?: () => void;

  constructor(className = "", title = "") {
    this.element = h("button", { type: "button", class: `tselect ${className}`, title, "aria-haspopup": "listbox" });
    this.element.addEventListener("click", () => (this.popover ? this.close() : this.open()));
    this.element.addEventListener("keydown", (event) => {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        this.open();
      }
    });
  }

  get value(): string {
    return this.current;
  }

  set value(value: string) {
    this.current = this.options.some((option) => option.value === value) ? value : this.options[0]?.value ?? "";
    this.paint();
  }

  get disabled(): boolean {
    return this.element.disabled;
  }

  set disabled(disabled: boolean) {
    this.element.disabled = disabled;
  }

  setOptions(options: SelectOption[]): void {
    this.options = options;
    this.value = this.current;
  }

  onChange(listener: (value: string) => void): void {
    this.listeners.push(listener);
  }

  private paint(): void {
    const option = this.options.find((candidate) => candidate.value === this.current);
    this.element.replaceChildren(h("span", { class: "tselect-label" }, option?.label ?? ""), h("span", { class: "tselect-caret" }, "▾"));
  }

  open(): void {
    if (this.popover || this.element.disabled || !this.options.length) return;
    this.active = Math.max(0, this.options.findIndex((option) => option.value === this.current));
    const items = this.options.map((option, index) =>
      h(
        "button",
        {
          type: "button",
          role: "option",
          class: `tselect-item${option.value === this.current ? " chosen" : ""}`,
          onmouseenter: () => this.highlight(index, false),
          onclick: () => this.choose(index),
        },
        h("span", { class: "tselect-check" }, option.value === this.current ? "✓" : ""),
        h("span", { class: "tselect-text" }, option.label),
        option.hint ? h("span", { class: "tselect-hint" }, option.hint) : null,
      ),
    );
    const popover = h("div", { class: "tselect-pop", role: "listbox" }, ...items);
    document.body.append(popover);
    this.popover = popover;
    this.place();
    this.highlight(this.active, true);
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        this.close();
        this.element.focus();
      } else if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        event.stopPropagation();
        this.highlight(Math.max(0, Math.min(this.options.length - 1, this.active + (event.key === "ArrowDown" ? 1 : -1))), true);
      } else if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        event.stopPropagation();
        this.choose(this.active);
      }
    };
    const onPointer = (event: PointerEvent) => {
      if (!popover.contains(event.target as Node) && !this.element.contains(event.target as Node)) this.close();
    };
    const onMove = () => this.place();
    document.addEventListener("keydown", onKey, true);
    document.addEventListener("pointerdown", onPointer, true);
    window.addEventListener("resize", onMove);
    window.addEventListener("scroll", onMove, true);
    this.teardown = () => {
      document.removeEventListener("keydown", onKey, true);
      document.removeEventListener("pointerdown", onPointer, true);
      window.removeEventListener("resize", onMove);
      window.removeEventListener("scroll", onMove, true);
    };
    this.element.classList.add("open");
  }

  /** Opens below the button, or above it when the space below is short. */
  private place(): void {
    const popover = this.popover;
    if (!popover) return;
    const anchor = this.element.getBoundingClientRect();
    const height = popover.offsetHeight;
    const width = Math.max(anchor.width, popover.offsetWidth);
    const below = window.innerHeight - anchor.bottom;
    const top = below >= height + 8 || below > anchor.top ? anchor.bottom + 4 : anchor.top - height - 4;
    const left = Math.min(Math.max(8, anchor.right - width), window.innerWidth - width - 8);
    popover.style.top = `${Math.max(8, top)}px`;
    popover.style.left = `${left}px`;
    popover.style.minWidth = `${anchor.width}px`;
    popover.classList.toggle("up", top < anchor.top);
  }

  private highlight(index: number, scroll: boolean): void {
    this.active = index;
    const items = this.popover?.children;
    if (!items) return;
    for (let position = 0; position < items.length; position++) items[position].classList.toggle("active", position === index);
    if (scroll) items[index]?.scrollIntoView({ block: "nearest" });
  }

  private choose(index: number): void {
    const option = this.options[index];
    this.close();
    this.element.focus();
    if (!option || option.value === this.current) return;
    this.current = option.value;
    this.paint();
    for (const listener of this.listeners) listener(option.value);
  }

  close(): void {
    this.teardown?.();
    this.teardown = undefined;
    const popover = this.popover;
    this.popover = undefined;
    this.element.classList.remove("open");
    if (!popover) return;
    popover.classList.add("closing");
    setTimeout(() => popover.remove(), 120);
  }
}

/**
 * Themed suggestions under a text input, replacing <datalist>. Tab or Enter
 * accepts the highlighted entry; accepting a directory keeps the list open
 * for its children.
 */
export class Suggestions {
  private popover?: HTMLElement;
  private items: string[] = [];
  private active = -1;
  private timer?: ReturnType<typeof setTimeout>;
  private request = 0;

  constructor(
    private readonly input: HTMLInputElement,
    private readonly source: (value: string) => Promise<string[]>,
  ) {
    input.setAttribute("autocomplete", "off");
    input.addEventListener("input", () => this.refresh());
    input.addEventListener("focus", () => this.refresh());
    input.addEventListener("blur", () => setTimeout(() => this.close(), 120));
    input.addEventListener("keydown", (event) => {
      if (!this.popover || !this.items.length) return;
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        this.highlight((this.active + (event.key === "ArrowDown" ? 1 : -1) + this.items.length) % this.items.length);
      } else if ((event.key === "Tab" || event.key === "Enter") && this.active >= 0) {
        event.preventDefault();
        event.stopPropagation();
        this.accept(this.items[this.active]);
      } else if (event.key === "Escape") {
        event.preventDefault();
        event.stopPropagation();
        this.close();
      }
    });
  }

  refresh(): void {
    clearTimeout(this.timer);
    this.timer = setTimeout(async () => {
      const request = ++this.request;
      let items: string[] = [];
      try {
        items = await this.source(this.input.value);
      } catch {
        // Suggestions are optional.
      }
      if (request !== this.request || document.activeElement !== this.input) return;
      this.show(items.filter((item) => item !== this.input.value).slice(0, 12));
    }, 90);
  }

  private show(items: string[]): void {
    this.items = items;
    this.active = -1;
    if (!items.length) return this.close();
    if (!this.popover) {
      this.popover = h("div", { class: "tselect-pop suggest", role: "listbox" });
      document.body.append(this.popover);
    }
    this.popover.replaceChildren(
      ...items.map((item, index) =>
        h(
          "button",
          {
            type: "button",
            class: "tselect-item",
            tabindex: "-1",
            onmouseenter: () => this.highlight(index),
            onmousedown: (event: Event) => event.preventDefault(),
            onclick: () => this.accept(item),
          },
          h("span", { class: "tselect-text" }, item),
        ),
      ),
    );
    const box = this.input.getBoundingClientRect();
    this.popover.style.top = `${box.bottom + 4}px`;
    this.popover.style.left = `${box.left}px`;
    this.popover.style.minWidth = `${box.width}px`;
    this.popover.style.maxWidth = `${box.width}px`;
  }

  private highlight(index: number): void {
    this.active = index;
    const children = this.popover?.children;
    if (!children) return;
    for (let position = 0; position < children.length; position++) children[position].classList.toggle("active", position === index);
    children[index]?.scrollIntoView({ block: "nearest" });
  }

  private accept(item: string): void {
    this.input.value = item.endsWith("/") ? item : `${item}/`;
    this.input.dispatchEvent(new Event("change"));
    this.refresh();
  }

  close(): void {
    this.popover?.remove();
    this.popover = undefined;
    this.items = [];
    this.active = -1;
  }
}
