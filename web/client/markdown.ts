// A small markdown renderer for agent messages. It escapes all text first and
// emits only a fixed set of tags, so model output can never inject markup
// into a page that is able to steer agents.

export function escapeHTML(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function safeHref(url: string): string | undefined {
  const trimmed = url.trim();
  return /^(https?:|mailto:)/i.test(trimmed) ? trimmed : undefined;
}

export function renderInline(text: string): string {
  let html = "";
  let index = 0;
  while (index < text.length) {
    const rest = text.slice(index);
    const code = /^(`+)([\s\S]*?[^`])\1(?!`)/.exec(rest);
    if (code) {
      html += `<code>${escapeHTML(code[2].replace(/^ (.*) $/, "$1"))}</code>`;
      index += code[0].length;
      continue;
    }
    const link = /^\[([^\]]+)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/.exec(rest);
    if (link) {
      const href = safeHref(link[2]);
      html += href
        ? `<a href="${escapeHTML(href)}" target="_blank" rel="noopener noreferrer">${renderInline(link[1])}</a>`
        : `<span class="md-link">${renderInline(link[1])}</span>`;
      index += link[0].length;
      continue;
    }
    const auto = /^https?:\/\/[^\s<>()]+[^\s<>().,;:!?'"]/.exec(rest);
    if (auto && (index === 0 || /[\s(]/.test(text[index - 1]))) {
      html += `<a href="${escapeHTML(auto[0])}" target="_blank" rel="noopener noreferrer">${escapeHTML(auto[0])}</a>`;
      index += auto[0].length;
      continue;
    }
    const bold = /^\*\*(?=\S)([\s\S]*?\S)\*\*|^__(?=\S)([\s\S]*?\S)__(?!\w)/.exec(rest);
    if (bold) {
      html += `<strong>${renderInline(bold[1] ?? bold[2])}</strong>`;
      index += bold[0].length;
      continue;
    }
    const italic = /^\*(?=[^\s*])([^*]*?[^\s*])\*(?!\*)|^_(?=[^\s_])([^_]*?[^\s_])_(?!\w)/.exec(rest);
    if (italic && (index === 0 || !/\w/.test(text[index - 1]))) {
      html += `<em>${renderInline(italic[1] ?? italic[2])}</em>`;
      index += italic[0].length;
      continue;
    }
    const strike = /^~~(?=\S)([\s\S]*?\S)~~/.exec(rest);
    if (strike) {
      html += `<del>${renderInline(strike[1])}</del>`;
      index += strike[0].length;
      continue;
    }
    const plain = /^[^`*_~\[h]+|^[\s\S]/.exec(rest)!;
    html += escapeHTML(plain[0]);
    index += plain[0].length;
  }
  return html;
}

interface ListFrame {
  tag: "ul" | "ol";
  indent: number;
}

function splitRow(line: string): string[] {
  let body = line.trim();
  if (body.startsWith("|")) body = body.slice(1);
  if (body.endsWith("|") && !body.endsWith("\\|")) body = body.slice(0, -1);
  return body.split(/(?<!\\)\|/).map((cell) => cell.trim().replace(/\\\|/g, "|"));
}

export interface MarkdownOptions {
  /** Called with each fenced block so the caller can highlight it later. */
  codeBlock?: (language: string, code: string) => string;
}

export function renderMarkdown(text: string, options: MarkdownOptions = {}): string {
  const lines = text.replace(/\r\n/g, "\n").split("\n");
  const out: string[] = [];
  let paragraph: string[] = [];
  const lists: ListFrame[] = [];
  let quote: string[] | undefined;

  const flushParagraph = () => {
    if (!paragraph.length) return;
    out.push(`<p>${paragraph.map(renderInline).join("<br>")}</p>`);
    paragraph = [];
  };
  const closeLists = (toIndent = -1) => {
    while (lists.length && lists[lists.length - 1].indent > toIndent) {
      out.push(`</li></${lists.pop()!.tag}>`);
    }
  };
  const flushQuote = () => {
    if (!quote) return;
    out.push(`<blockquote>${renderMarkdown(quote.join("\n"), options)}</blockquote>`);
    quote = undefined;
  };
  const flushAll = () => {
    flushParagraph();
    closeLists();
    flushQuote();
  };

  for (let index = 0; index < lines.length; index++) {
    const raw = lines[index];
    const fence = /^(\s*)(`{3,}|~{3,})\s*([\w+#.-]*)/.exec(raw);
    if (fence) {
      flushAll();
      const marker = fence[2];
      const body: string[] = [];
      index++;
      while (index < lines.length && !lines[index].trim().startsWith(marker)) {
        body.push(lines[index]);
        index++;
      }
      const code = body.join("\n");
      const language = fence[3] ?? "";
      out.push(
        options.codeBlock
          ? options.codeBlock(language, code)
          : `<pre class="md-code"><code>${escapeHTML(code)}</code></pre>`,
      );
      continue;
    }
    const quoted = /^\s*>\s?(.*)$/.exec(raw);
    if (quoted) {
      flushParagraph();
      closeLists();
      (quote ??= []).push(quoted[1]);
      continue;
    }
    flushQuote();
    if (!raw.trim()) {
      flushParagraph();
      // A blank line inside a list keeps the list open for a following item.
      const next = lines[index + 1] ?? "";
      if (!/^\s*([-*+]|\d+[.)])\s+/.test(next)) closeLists();
      continue;
    }
    const heading = /^(#{1,6})\s+(.*?)\s*#*\s*$/.exec(raw);
    if (heading) {
      flushAll();
      const level = heading[1].length;
      out.push(`<h${level}>${renderInline(heading[2])}</h${level}>`);
      continue;
    }
    if (/^\s*([-*_])(?:\s*\1){2,}\s*$/.test(raw)) {
      flushAll();
      out.push("<hr>");
      continue;
    }
    if (raw.includes("|") && /^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$/.test(lines[index + 1] ?? "")) {
      flushAll();
      const header = splitRow(raw);
      const aligns = splitRow(lines[index + 1]).map((cell) =>
        cell.startsWith(":") && cell.endsWith(":") ? "center" : cell.endsWith(":") ? "right" : "",
      );
      index += 2;
      const rows: string[][] = [];
      while (index < lines.length && lines[index].includes("|") && lines[index].trim()) {
        rows.push(splitRow(lines[index]));
        index++;
      }
      index--;
      const cell = (tag: string, value: string, column: number) =>
        `<${tag}${aligns[column] ? ` style="text-align:${aligns[column]}"` : ""}>${renderInline(value)}</${tag}>`;
      out.push(
        `<div class="md-table"><table><thead><tr>${header.map((value, column) => cell("th", value, column)).join("")}</tr></thead><tbody>${rows
          .map((row) => `<tr>${header.map((_, column) => cell("td", row[column] ?? "", column)).join("")}</tr>`)
          .join("")}</tbody></table></div>`,
      );
      continue;
    }
    const item = /^(\s*)([-*+]|\d+[.)])\s+(?:\[( |x|X)\]\s+)?(.*)$/.exec(raw);
    if (item) {
      flushParagraph();
      const indent = item[1].replace(/\t/g, "  ").length;
      const tag = /\d/.test(item[2]) ? "ol" : "ul";
      const top = lists[lists.length - 1];
      if (!top || indent > top.indent) {
        const start = tag === "ol" && Number.parseInt(item[2], 10) !== 1 ? ` start="${Number.parseInt(item[2], 10)}"` : "";
        out.push(`<${tag}${start}><li>`);
        lists.push({ tag, indent });
      } else {
        closeLists(indent);
        const current = lists[lists.length - 1];
        if (current && current.tag !== tag && current.indent === indent) {
          out.push(`</li></${lists.pop()!.tag}><${tag}><li>`);
          lists.push({ tag, indent });
        } else if (current) out.push("</li><li>");
        else {
          out.push(`<${tag}><li>`);
          lists.push({ tag, indent });
        }
      }
      const check = item[3] ? `<input type="checkbox" disabled${item[3].trim() ? " checked" : ""}> ` : "";
      out.push(check + renderInline(item[4]));
      continue;
    }
    if (lists.length && /^\s+\S/.test(raw)) {
      out.push(`<br>${renderInline(raw.trim())}`);
      continue;
    }
    closeLists();
    paragraph.push(raw);
  }
  flushAll();
  return out.join("");
}
