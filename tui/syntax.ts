import { basename } from "node:path";

// Stateful syntax highlighting. A hand-written scanner keeps the diff and chat
// renderers deterministic and dependency-free: OpenTUI's Tree-sitter path needs
// a worker plus downloaded grammars for most languages a session touches.

export type CodeToken =
  | "plain"
  | "keyword"
  | "string"
  | "comment"
  | "number"
  | "type"
  | "function"
  | "punctuation"
  | "operator"
  | "property"
  | "tag"
  | "attribute"
  | "constant"
  | "regex"
  | "heading";

export interface CodeSpan {
  text: string;
  token: CodeToken;
}

/**
 * Scanner state carried between consecutive lines of one file so block
 * comments and multi-line strings keep their color past the first line.
 */
export interface HighlightState {
  mode: "code" | "block-comment" | "string";
  /** Closing delimiter for a multi-line string (``` ` ```, `"""`, `'''`). */
  close?: string;
}

export const INITIAL_HIGHLIGHT_STATE: HighlightState = { mode: "code" };

interface LanguageSpec {
  keywords: Set<string>;
  types: Set<string>;
  constants: Set<string>;
  lineComment: string[];
  blockComment?: [string, string];
  /** Quote characters whose strings may span lines. */
  multilineQuotes: string[];
  /** Triple-quote openers (Python docstrings). */
  tripleQuotes: string[];
  regexLiterals: boolean;
  jsx: boolean;
  decorators: "@" | "#[" | undefined;
  caseInsensitiveKeywords: boolean;
  /** Treat `$name` as a variable (shell). */
  dollarVariables: boolean;
  /** `#` starts a comment only when not part of `$#` or `${#`. */
  hashComment: boolean;
}

const words = (list: string): Set<string> => new Set(list.split(/\s+/).filter(Boolean));

const LANGUAGES: Record<string, LanguageSpec> = {
  typescript: {
    keywords: words(
      "abstract as async await break case catch class const continue debugger declare default delete do else enum export extends finally for from function get if implements import in instanceof interface is keyof let module namespace new of override package private protected public readonly return satisfies set static super switch this throw try type typeof var void while with yield",
    ),
    types: words("any bigint boolean never number object string symbol unknown Array Promise Record Partial Readonly Map Set Date Error RegExp"),
    constants: words("true false null undefined NaN Infinity"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: ["`"],
    tripleQuotes: [],
    regexLiterals: true,
    jsx: true,
    decorators: "@",
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  go: {
    keywords: words("break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var"),
    types: words("bool byte complex64 complex128 error float32 float64 int int8 int16 int32 int64 rune string uint uint8 uint16 uint32 uint64 uintptr any comparable"),
    constants: words("true false nil iota"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: ["`"],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  python: {
    keywords: words("and as assert async await break class continue def del elif else except finally for from global if import in is lambda nonlocal not or pass raise return try while with yield match case"),
    types: words("int float str bytes bool list dict set tuple object type Optional Any Union List Dict"),
    constants: words("True False None self cls"),
    lineComment: ["#"],
    multilineQuotes: [],
    tripleQuotes: ['"""', "'''"],
    regexLiterals: false,
    jsx: false,
    decorators: "@",
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: true,
  },
  rust: {
    keywords: words("as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod move mut pub ref return static struct super trait type unsafe use where while"),
    types: words("bool char f32 f64 i8 i16 i32 i64 i128 isize str u8 u16 u32 u64 u128 usize String Vec Option Result Box Rc Arc Self"),
    constants: words("true false self None Some Ok Err"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: ['"'],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: "#[",
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  shell: {
    keywords: words("if then else elif fi for while until do done case esac function in return exit export local readonly set unset source alias break continue select time"),
    types: words(""),
    constants: words("true false"),
    lineComment: ["#"],
    multilineQuotes: ["'", '"'],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: true,
    hashComment: true,
  },
  json: {
    keywords: words(""),
    types: words(""),
    constants: words("true false null"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  yaml: {
    keywords: words(""),
    types: words(""),
    constants: words("true false null yes no on off ~"),
    lineComment: ["#"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: true,
  },
  c: {
    keywords: words("auto break case catch class const constexpr continue default delete do else enum explicit export extern for friend goto if inline namespace new noexcept operator override private protected public register return sizeof static struct switch template this throw try typedef typename union using virtual volatile while final"),
    types: words("bool char double float int long short signed unsigned void wchar_t size_t int8_t int16_t int32_t int64_t uint8_t uint16_t uint32_t uint64_t string vector map auto var"),
    constants: words("true false nullptr NULL"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  java: {
    keywords: words("abstract assert break case catch class const continue default do else enum extends final finally for goto if implements import instanceof interface native new package private protected public return static strictfp super switch synchronized this throw throws transient try volatile while var record sealed permits yield fun val when object data is in"),
    types: words("boolean byte char double float int long short void String Integer Long Boolean List Map Set Unit"),
    constants: words("true false null"),
    lineComment: ["//"],
    blockComment: ["/*", "*/"],
    multilineQuotes: ['"""'],
    tripleQuotes: ['"""'],
    regexLiterals: false,
    jsx: false,
    decorators: "@",
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  ruby: {
    keywords: words("alias and begin break case class def defined? do else elsif end ensure for if in module next not or redo rescue retry return super then undef unless until when while yield require require_relative attr_reader attr_writer attr_accessor include extend private public protected raise"),
    types: words("String Integer Float Array Hash Symbol Proc"),
    constants: words("true false nil self"),
    lineComment: ["#"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: true,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: true,
  },
  sql: {
    keywords: words("select from where and or not in is null as join left right inner outer full on group by order having limit offset insert into values update set delete create table alter drop index view primary key foreign references default unique check constraint begin commit rollback with recursive union all distinct case when then else end exists between like ilike asc desc returning if cascade transaction grant revoke"),
    types: words("int integer bigint smallint serial bigserial text varchar char boolean bool date timestamp timestamptz time interval numeric decimal real float double json jsonb uuid bytea"),
    constants: words("true false null current_timestamp now"),
    lineComment: ["--"],
    blockComment: ["/*", "*/"],
    multilineQuotes: ["'"],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: true,
    dollarVariables: false,
    hashComment: false,
  },
  css: {
    keywords: words("important media supports import font-face keyframes from to and not only screen print"),
    types: words(""),
    constants: words("inherit initial unset none auto"),
    lineComment: [],
    blockComment: ["/*", "*/"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: true,
    dollarVariables: false,
    hashComment: false,
  },
  html: {
    keywords: words(""),
    types: words(""),
    constants: words(""),
    lineComment: [],
    blockComment: ["<!--", "-->"],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: true,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
  markdown: {
    keywords: words(""),
    types: words(""),
    constants: words(""),
    lineComment: [],
    multilineQuotes: [],
    tripleQuotes: [],
    regexLiterals: false,
    jsx: false,
    decorators: undefined,
    caseInsensitiveKeywords: false,
    dollarVariables: false,
    hashComment: false,
  },
};

const EXTENSION_FILETYPES: Record<string, string> = {
  ts: "typescript", tsx: "typescript", mts: "typescript", cts: "typescript",
  js: "typescript", jsx: "typescript", mjs: "typescript", cjs: "typescript",
  go: "go",
  py: "python", pyi: "python",
  rs: "rust",
  sh: "shell", bash: "shell", zsh: "shell", fish: "shell",
  json: "json", jsonc: "json",
  yaml: "yaml", yml: "yaml", toml: "yaml",
  c: "c", h: "c", cc: "c", cpp: "c", hpp: "c", cs: "c", swift: "c", m: "c",
  kt: "java", kts: "java", java: "java", scala: "java",
  rb: "ruby",
  md: "markdown", markdown: "markdown", mdx: "markdown",
  css: "css", scss: "css", less: "css",
  html: "html", htm: "html", svelte: "html", vue: "html", xml: "html", svg: "html",
  sql: "sql",
};

const FENCE_ALIASES: Record<string, string> = {
  js: "typescript", jsx: "typescript", ts: "typescript", tsx: "typescript",
  javascript: "typescript", typescript: "typescript", typescriptreact: "typescript",
  py: "python", python: "python",
  rs: "rust", rust: "rust",
  sh: "shell", bash: "shell", zsh: "shell", shell: "shell", console: "shell",
  c: "c", cpp: "c", "c++": "c", csharp: "c", cs: "c", swift: "c", objc: "c",
  java: "java", kotlin: "java", kt: "java", scala: "java",
  rb: "ruby", ruby: "ruby",
  json: "json", jsonc: "json",
  yaml: "yaml", yml: "yaml", toml: "yaml",
  css: "css", scss: "css",
  html: "html", xml: "html", svg: "html", vue: "html", svelte: "html",
  sql: "sql", psql: "sql",
  md: "markdown", markdown: "markdown",
  go: "go", golang: "go",
};

export function filetypeForPath(path: string | undefined): string {
  if (!path) return "plain";
  const name = basename(path);
  if (name === "Dockerfile" || name === "Makefile" || name.startsWith(".")) return "shell";
  const extension = name.includes(".") ? name.split(".").pop()! : "";
  return EXTENSION_FILETYPES[extension.toLowerCase()] ?? "plain";
}

export function filetypeForFence(info: string | undefined): string {
  const language = (info ?? "").trim().split(/\s+/)[0]?.toLowerCase() ?? "";
  if (!language) return "plain";
  return FENCE_ALIASES[language] ?? EXTENSION_FILETYPES[language] ?? "plain";
}

export function highlightCode(line: string, filetype: string): CodeSpan[] {
  return highlightLine(line, filetype, INITIAL_HIGHLIGHT_STATE).spans;
}

/** Highlights consecutive lines of one file, threading scanner state. */
export function highlightLines(lines: readonly string[], filetype: string): CodeSpan[][] {
  let state = INITIAL_HIGHLIGHT_STATE;
  return lines.map((line) => {
    const result = highlightLine(line, filetype, state);
    state = result.state;
    return result.spans;
  });
}

const WORD = /^[A-Za-z_$][\w$]*/;
const NUMBER = /^(?:0[xX][0-9a-fA-F_]+|0[bB][01_]+|0[oO][0-7_]+|\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?[a-zA-Z]{0,3})\b/;
const OPERATOR = /^(?:=>|->|::|\?\?=?|\?\.|\.\.\.?|&&=?|\|\|=?|<<=?|>>>?=?|[-+*/%&|^!=<>]=?|~|\?|:)/;
const PUNCTUATION = /^[{}()[\],;.]+/;

export function highlightLine(
  line: string,
  filetype: string,
  state: HighlightState = INITIAL_HIGHLIGHT_STATE,
): { spans: CodeSpan[]; state: HighlightState } {
  if (line.length === 0) return { spans: [], state };
  const spec = LANGUAGES[filetype];
  if (!spec) return { spans: [{ text: line, token: "plain" }], state };
  if (filetype === "markdown") return { spans: highlightMarkdownLine(line), state };

  const spans: CodeSpan[] = [];
  const push = (text: string, token: CodeToken) => {
    if (!text) return;
    const last = spans[spans.length - 1];
    if (last && last.token === token) last.text += text;
    else spans.push({ text, token });
  };
  let index = 0;
  let mode = state;
  let lastSignificant: CodeSpan | undefined;
  const significant = () => {
    for (let cursor = spans.length - 1; cursor >= 0; cursor--) {
      if (spans[cursor].text.trim()) return spans[cursor];
    }
    return undefined;
  };

  while (index < line.length) {
    const rest = line.slice(index);

    if (mode.mode === "block-comment") {
      const close = spec.blockComment?.[1] ?? "*/";
      const end = rest.indexOf(close);
      if (end < 0) {
        push(rest, "comment");
        index = line.length;
        break;
      }
      push(rest.slice(0, end + close.length), "comment");
      index += end + close.length;
      mode = INITIAL_HIGHLIGHT_STATE;
      continue;
    }

    if (mode.mode === "string") {
      const close = mode.close ?? '"';
      const end = findUnescaped(rest, close);
      if (end < 0) {
        push(rest, "string");
        index = line.length;
        break;
      }
      push(rest.slice(0, end + close.length), "string");
      index += end + close.length;
      mode = INITIAL_HIGHLIGHT_STATE;
      continue;
    }

    // Comments.
    const lineComment = spec.lineComment.find((marker) => rest.startsWith(marker));
    if (
      lineComment &&
      !(spec.hashComment && lineComment === "#" && index > 0 && /[$\w{]/.test(line[index - 1]))
    ) {
      push(rest, "comment");
      index = line.length;
      break;
    }
    if (spec.blockComment && rest.startsWith(spec.blockComment[0])) {
      const [open, close] = spec.blockComment;
      const end = rest.indexOf(close, open.length);
      if (end < 0) {
        push(rest, "comment");
        mode = { mode: "block-comment" };
        index = line.length;
        break;
      }
      push(rest.slice(0, end + close.length), "comment");
      index += end + close.length;
      continue;
    }

    // Strings.
    const triple = spec.tripleQuotes.find((quote) => rest.startsWith(quote));
    if (triple) {
      const end = rest.indexOf(triple, triple.length);
      if (end < 0) {
        push(rest, "string");
        mode = { mode: "string", close: triple };
        index = line.length;
        break;
      }
      push(rest.slice(0, end + triple.length), "string");
      index += end + triple.length;
      continue;
    }
    const quote = rest[0];
    if (quote === '"' || quote === "'" || quote === "`") {
      const end = findUnescaped(rest.slice(1), quote);
      if (end < 0) {
        push(rest, "string");
        if (spec.multilineQuotes.includes(quote)) mode = { mode: "string", close: quote };
        index = line.length;
        break;
      }
      const text = rest.slice(0, end + 2);
      if (quote === "`" && spec.regexLiterals) {
        // Template literal: color interpolations as code.
        pushTemplate(text, push, spec);
      } else push(text, "string");
      index += text.length;
      continue;
    }

    // JSX / HTML tags.
    if (spec.jsx && rest[0] === "<") {
      const tag = /^<\/?[A-Za-z][\w.:-]*/.exec(rest);
      if (tag && (filetype === "html" || looksLikeJsx(line, index))) {
        push(tag[0].slice(0, tag[0].startsWith("</") ? 2 : 1), "punctuation");
        push(tag[0].slice(tag[0].startsWith("</") ? 2 : 1), "tag");
        index += tag[0].length;
        // Attributes until the closing bracket.
        while (index < line.length) {
          const inner = line.slice(index);
          const closeTag = /^\s*\/?>/.exec(inner);
          if (closeTag) {
            push(closeTag[0], "punctuation");
            index += closeTag[0].length;
            break;
          }
          const space = /^\s+/.exec(inner);
          if (space) {
            push(space[0], "plain");
            index += space[0].length;
            continue;
          }
          const attribute = /^[A-Za-z_:@][\w.:-]*/.exec(inner);
          if (attribute) {
            push(attribute[0], "attribute");
            index += attribute[0].length;
            continue;
          }
          if (inner[0] === "=") {
            push("=", "operator");
            index++;
            continue;
          }
          if (inner[0] === '"' || inner[0] === "'") {
            const end = findUnescaped(inner.slice(1), inner[0]);
            const text = end < 0 ? inner : inner.slice(0, end + 2);
            push(text, "string");
            index += text.length;
            continue;
          }
          if (inner[0] === "{") {
            const end = matchBrace(inner);
            const text = inner.slice(0, end);
            push("{", "punctuation");
            const innerSpans = highlightLine(text.slice(1, -1), filetype).spans;
            for (const span of innerSpans) push(span.text, span.token);
            if (text.endsWith("}")) push("}", "punctuation");
            index += text.length;
            continue;
          }
          break;
        }
        continue;
      }
      if (filetype === "html" && rest.startsWith("<!")) {
        const end = rest.indexOf(">");
        const text = end < 0 ? rest : rest.slice(0, end + 1);
        push(text, "comment");
        index += text.length;
        continue;
      }
    }

    // Decorators and attributes.
    if (spec.decorators === "@" && rest[0] === "@") {
      const decorator = /^@[\w.]+/.exec(rest);
      if (decorator) {
        push(decorator[0], "attribute");
        index += decorator[0].length;
        continue;
      }
    }
    if (spec.decorators === "#[" && rest.startsWith("#[")) {
      const end = rest.indexOf("]");
      const text = end < 0 ? rest : rest.slice(0, end + 1);
      push(text, "attribute");
      index += text.length;
      continue;
    }

    // Shell variables.
    if (spec.dollarVariables && rest[0] === "$") {
      const variable = /^\$(?:\{[^}]*\}|[\w@#?*!$-]+)/.exec(rest);
      if (variable) {
        push(variable[0], "property");
        index += variable[0].length;
        continue;
      }
    }

    // Regex literals after an operator or open bracket.
    if (spec.regexLiterals && rest[0] === "/" && !rest.startsWith("//") && !rest.startsWith("/*")) {
      const previous = significant();
      const allowed =
        !previous ||
        previous.token === "operator" ||
        previous.token === "keyword" ||
        (previous.token === "punctuation" && /[([{,;]$/.test(previous.text));
      if (allowed) {
        const regex = /^\/(?:\\.|\[(?:\\.|[^\]\\])*\]|[^/\\\n[])+\/[a-z]*/.exec(rest);
        if (regex) {
          push(regex[0], "regex");
          index += regex[0].length;
          continue;
        }
      }
    }

    // CSS: selectors and properties.
    if (filetype === "css") {
      const atRule = /^@[a-z-]+/.exec(rest);
      if (atRule) {
        push(atRule[0], "keyword");
        index += atRule[0].length;
        continue;
      }
      const color = /^#[0-9a-fA-F]{3,8}\b/.exec(rest);
      if (color) {
        push(color[0], "number");
        index += color[0].length;
        continue;
      }
      // `name:` at the start of a declaration is a property; elsewhere a
      // colon means a pseudo-class, which falls through to the word scanner.
      const property = /^([a-zA-Z-]+)(\s*:)/.exec(rest);
      const before = line.slice(0, index);
      if (property && (/^\s*$/.test(before) || /[{;]\s*$/.test(before))) {
        push(property[1], "property");
        push(property[2], "operator");
        index += property[0].length;
        continue;
      }
      const selector = /^[.#][A-Za-z_][\w-]*/.exec(rest);
      if (selector) {
        push(selector[0], "tag");
        index += selector[0].length;
        continue;
      }
    }

    // Numbers.
    const number = NUMBER.exec(rest);
    if (number && (index === 0 || !/[\w$]/.test(line[index - 1]))) {
      push(number[0], "number");
      index += number[0].length;
      continue;
    }

    // Words.
    const word = WORD.exec(rest);
    if (word) {
      const text = word[0];
      const lookup = spec.caseInsensitiveKeywords ? text.toLowerCase() : text;
      const after = rest.slice(text.length);
      const previous = significant();
      let token: CodeToken = "plain";
      if (spec.constants.has(lookup)) token = "constant";
      else if (spec.keywords.has(lookup)) token = "keyword";
      else if (spec.types.has(lookup)) token = "type";
      else if (
        previous?.token === "keyword" &&
        /^(?:func|fn|def|function|class|struct|enum|interface|type|trait|impl|module|namespace)$/.test(previous.text)
      )
        token = /^(?:class|struct|enum|interface|type|trait|impl|module|namespace)$/.test(previous.text) ? "type" : "function";
      else if (/^\s*\(/.test(after) || /^\s*[<]\s*[A-Za-z].*>\s*\(/.test(after)) token = "function";
      else if (previous?.token === "keyword" && previous.text === "new") token = "type";
      else if (/^[A-Z][A-Z0-9_]{2,}$/.test(text)) token = "constant";
      else if (/^[A-Z][A-Za-z0-9_]*$/.test(text) && text.length > 1) token = "type";
      else if (previous?.text.endsWith(".") && !/^\s*\(/.test(after)) token = "property";
      else if (previous?.token === "operator" && /:$/.test(previous.text) && filetype !== "yaml")
        token = "type";
      else if ((filetype === "yaml" || filetype === "json") && /^\s*:/.test(after)) token = "property";
      else if (filetype === "python" && previous?.token === "keyword" && /^(?:import|from)$/.test(previous.text))
        token = "type";
      push(text, token);
      lastSignificant = spans[spans.length - 1];
      index += text.length;
      continue;
    }

    // Operators and punctuation.
    const operator = OPERATOR.exec(rest);
    if (operator) {
      push(operator[0], "operator");
      index += operator[0].length;
      continue;
    }
    const punctuation = PUNCTUATION.exec(rest);
    if (punctuation) {
      push(punctuation[0], "punctuation");
      index += punctuation[0].length;
      continue;
    }
    const whitespace = /^\s+/.exec(rest);
    if (whitespace) {
      push(whitespace[0], "plain");
      index += whitespace[0].length;
      continue;
    }
    push(rest[0], "plain");
    index++;
  }
  void lastSignificant;

  if (filetype === "json") {
    for (const [position, span] of spans.entries()) {
      const next = spans[position + 1];
      if (span.token === "string" && next?.text.trimStart().startsWith(":")) span.token = "property";
    }
  }
  return { spans, state: mode };
}

function findUnescaped(text: string, close: string): number {
  let cursor = 0;
  while (cursor < text.length) {
    if (text[cursor] === "\\") {
      cursor += 2;
      continue;
    }
    if (text.startsWith(close, cursor)) return cursor;
    cursor++;
  }
  return -1;
}

function matchBrace(text: string): number {
  let depth = 0;
  for (let cursor = 0; cursor < text.length; cursor++) {
    if (text[cursor] === "{") depth++;
    else if (text[cursor] === "}") {
      depth--;
      if (depth === 0) return cursor + 1;
    }
  }
  return text.length;
}

function looksLikeJsx(line: string, index: number): boolean {
  const before = line.slice(0, index).trimEnd();
  const next = line[index + 1] ?? "";
  if (!/[A-Za-z/]/.test(next)) return false;
  // `a < b` is a comparison; `return <div>` or `(<div>` is markup.
  return before === "" || /(?:return|=>|=|\(|,|\?|:|&&|\|\||\{)$/.test(before);
}

function pushTemplate(
  text: string,
  push: (text: string, token: CodeToken) => void,
  spec: LanguageSpec,
): void {
  void spec;
  let cursor = 0;
  while (cursor < text.length) {
    const open = text.indexOf("${", cursor);
    if (open < 0) {
      push(text.slice(cursor), "string");
      return;
    }
    push(text.slice(cursor, open), "string");
    const end = matchBrace(text.slice(open + 1));
    const inner = text.slice(open + 2, open + end);
    push("${", "punctuation");
    for (const span of highlightLine(inner, "typescript").spans) push(span.text, span.token);
    push("}", "punctuation");
    cursor = open + 1 + end;
  }
}

function highlightMarkdownLine(line: string): CodeSpan[] {
  if (/^#{1,6}\s/.test(line)) return [{ text: line, token: "heading" }];
  if (/^\s*(?:[-*+]|\d+[.)])\s/.test(line)) {
    const marker = /^\s*(?:[-*+]|\d+[.)])\s/.exec(line)![0];
    return [{ text: marker, token: "keyword" }, ...highlightMarkdownInline(line.slice(marker.length))];
  }
  if (/^\s*>/.test(line)) return [{ text: line, token: "comment" }];
  if (/^\s*(`{3,}|~{3,})/.test(line)) return [{ text: line, token: "punctuation" }];
  return highlightMarkdownInline(line);
}

function highlightMarkdownInline(text: string): CodeSpan[] {
  const spans: CodeSpan[] = [];
  const pattern = /`[^`]+`|\*\*[^*]+\*\*|\[[^\]]+\]\([^)]+\)/g;
  let cursor = 0;
  for (const match of text.matchAll(pattern)) {
    if (match.index > cursor) spans.push({ text: text.slice(cursor, match.index), token: "plain" });
    const token: CodeToken = match[0].startsWith("`")
      ? "string"
      : match[0].startsWith("**")
        ? "constant"
        : "attribute";
    spans.push({ text: match[0], token });
    cursor = match.index + match[0].length;
  }
  if (cursor < text.length) spans.push({ text: text.slice(cursor), token: "plain" });
  return spans;
}

// ---------------------------------------------------------------------------
// Markdown for agent messages. Block structure plus a small inline grammar.

export type InlineStyle = "plain" | "code" | "bold" | "italic" | "link";

export interface InlineSpan {
  text: string;
  style: InlineStyle;
}

export type MarkdownLineKind =
  | "heading"
  | "bullet"
  | "numbered"
  | "quote"
  | "code"
  | "fence"
  | "rule"
  | "blank"
  | "paragraph";

export interface MarkdownLine {
  kind: MarkdownLineKind;
  spans: InlineSpan[];
  level?: number;
  language?: string;
  marker?: string;
  indent?: number;
}

export function parseInline(text: string): InlineSpan[] {
  const spans: InlineSpan[] = [];
  const push = (value: string, style: InlineStyle) => {
    if (!value) return;
    const last = spans[spans.length - 1];
    if (last && last.style === style) last.text += value;
    else spans.push({ text: value, style });
  };
  let index = 0;
  while (index < text.length) {
    const rest = text.slice(index);
    const code = /^`([^`]+)`/.exec(rest);
    if (code) {
      push(code[1], "code");
      index += code[0].length;
      continue;
    }
    const bold = /^\*\*([^*]+)\*\*|^__([^_]+)__/.exec(rest);
    if (bold) {
      push(bold[1] ?? bold[2], "bold");
      index += bold[0].length;
      continue;
    }
    const italic = /^\*([^*\s][^*]*)\*|^_([^_\s][^_]*)_(?![\w])/.exec(rest);
    if (italic && (index === 0 || !/\w/.test(text[index - 1]))) {
      push(italic[1] ?? italic[2], "italic");
      index += italic[0].length;
      continue;
    }
    const link = /^\[([^\]]+)\]\(([^)]+)\)/.exec(rest);
    if (link) {
      push(link[1], "link");
      index += link[0].length;
      continue;
    }
    const plain = /^[^`*_\[]+|^./.exec(rest)!;
    push(plain[0], "plain");
    index += plain[0].length;
  }
  return spans;
}

export function parseMarkdown(text: string): MarkdownLine[] {
  const lines: MarkdownLine[] = [];
  let fence: { language: string; marker: string } | undefined;
  for (const raw of text.replace(/\r\n/g, "\n").split("\n")) {
    if (fence) {
      if (raw.trim().startsWith(fence.marker)) {
        fence = undefined;
        continue;
      }
      lines.push({
        kind: "code",
        spans: [{ text: raw, style: "plain" }],
        language: fence.language,
      });
      continue;
    }
    const open = /^\s*(`{3,}|~{3,})\s*(\S*)/.exec(raw);
    if (open) {
      fence = { language: filetypeForFence(open[2]), marker: open[1] };
      lines.push({ kind: "fence", spans: [], language: open[2] || undefined });
      continue;
    }
    if (raw.trim() === "") {
      lines.push({ kind: "blank", spans: [] });
      continue;
    }
    const heading = /^(#{1,6})\s+(.*)$/.exec(raw);
    if (heading) {
      lines.push({
        kind: "heading",
        level: heading[1].length,
        spans: parseInline(heading[2].replace(/\s#+$/, "")),
      });
      continue;
    }
    if (/^\s*([-*_])(?:\s*\1){2,}\s*$/.test(raw)) {
      lines.push({ kind: "rule", spans: [] });
      continue;
    }
    const bullet = /^(\s*)[-*+]\s+(.*)$/.exec(raw);
    if (bullet) {
      lines.push({
        kind: "bullet",
        indent: Math.floor(bullet[1].length / 2),
        spans: parseInline(bullet[2]),
      });
      continue;
    }
    const numbered = /^(\s*)(\d+)[.)]\s+(.*)$/.exec(raw);
    if (numbered) {
      lines.push({
        kind: "numbered",
        indent: Math.floor(numbered[1].length / 2),
        marker: `${numbered[2]}.`,
        spans: parseInline(numbered[3]),
      });
      continue;
    }
    const quote = /^\s*>\s?(.*)$/.exec(raw);
    if (quote) {
      lines.push({ kind: "quote", spans: parseInline(quote[1]) });
      continue;
    }
    lines.push({ kind: "paragraph", spans: parseInline(raw) });
  }
  return lines;
}
