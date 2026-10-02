import { describe, expect, test } from "bun:test";
import { renderInline, renderMarkdown } from "./markdown";

describe("markdown", () => {
  test("escapes raw HTML from model output", () => {
    const html = renderMarkdown('<img src=x onerror="alert(1)"> **<b>x</b>**');
    expect(html).not.toContain("<img");
    expect(html).not.toContain("<b>");
    expect(html).toContain("&lt;img");
  });

  test("allows only http, https, and mailto links", () => {
    expect(renderInline("[a](javascript:alert(1))")).not.toContain("href");
    expect(renderInline("[a](https://x.dev)")).toContain('href="https://x.dev"');
    expect(renderInline('[a](https://x.dev/"onmouseover=1)')).toContain("&quot;");
  });

  test("renders fences, lists, tables, and inline styles", () => {
    const html = renderMarkdown("# T\n\n- one\n- **two**\n\n```ts\nconst a = 1 < 2;\n```\n\n| a | b |\n|---|--:|\n| 1 | 2 |");
    expect(html).toContain("<h1>T</h1>");
    expect(html).toContain("<ul><li>one</li><li><strong>two</strong></li></ul>");
    expect(html).toContain("const a = 1 &lt; 2;");
    expect(html).toContain('<td style="text-align:right">2</td>');
  });

  test("nests indented list items", () => {
    expect(renderMarkdown("1. a\n   - b\n2. c")).toBe("<ol><li>a<ul><li>b</li></ul></li><li>c</li></ol>");
  });

  test("keeps code spans literal", () => {
    expect(renderInline("`**x**`")).toBe("<code>**x**</code>");
  });
});
