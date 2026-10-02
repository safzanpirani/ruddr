// Turns ANSI SGR color codes in command output into themed spans. Every other
// escape sequence is dropped. Text goes through text nodes, never HTML.

const SGR = /\x1b\[([0-9;]*)m/g;
const OTHER_ESCAPES = /\x1b(?:\[[0-9;?]*[A-Za-z]|\][^\x07\x1b]*(?:\x07|\x1b\\)|[()][A-Za-z0-9]|[=>78])/g;
const NAMES = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];

interface Style {
  fg?: string;
  bold?: boolean;
  dim?: boolean;
}

function className(style: Style): string {
  return [style.fg ? `ansi-${style.fg}` : "", style.bold ? "ansi-bold" : "", style.dim ? "ansi-dim" : ""].filter(Boolean).join(" ");
}

function apply(style: Style, codes: number[]): Style {
  const next = { ...style };
  if (!codes.length) codes = [0];
  for (let index = 0; index < codes.length; index++) {
    const code = codes[index];
    if (code === 0) {
      delete next.fg;
      delete next.bold;
      delete next.dim;
    } else if (code === 1) next.bold = true;
    else if (code === 2) next.dim = true;
    else if (code === 22) {
      delete next.bold;
      delete next.dim;
    } else if (code >= 30 && code <= 37) next.fg = NAMES[code - 30];
    else if (code >= 90 && code <= 97) next.fg = NAMES[code - 90];
    else if (code === 39) delete next.fg;
    else if (code === 38 || code === 48) index += codes[index + 1] === 5 ? 2 : codes[index + 1] === 2 ? 4 : 0;
  }
  return next;
}

/** Splits text into styled segments. Exported for tests. */
export function ansiSegments(text: string): Array<{ text: string; className: string }> {
  const segments: Array<{ text: string; className: string }> = [];
  let style: Style = {};
  let last = 0;
  const push = (chunk: string) => {
    const clean = chunk.replace(OTHER_ESCAPES, "").replace(/\x1b/g, "");
    if (!clean) return;
    const name = className(style);
    const previous = segments[segments.length - 1];
    if (previous && previous.className === name) previous.text += clean;
    else segments.push({ text: clean, className: name });
  };
  for (const match of text.matchAll(SGR)) {
    push(text.slice(last, match.index));
    style = apply(style, match[1] ? match[1].split(";").map(Number) : []);
    last = match.index! + match[0].length;
  }
  push(text.slice(last));
  return segments;
}

export function ansiNodes(text: string): Node[] {
  return ansiSegments(text).map((segment) => {
    if (!segment.className) return document.createTextNode(segment.text);
    const span = document.createElement("span");
    span.className = segment.className;
    span.textContent = segment.text;
    return span;
  });
}
