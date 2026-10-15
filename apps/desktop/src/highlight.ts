// Syntax highlighting for a file's text and its diff (D-111): highlight.js
// with the languages a workspace usually holds, line by line.

import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import c from "highlight.js/lib/languages/c";
import cpp from "highlight.js/lib/languages/cpp";
import css from "highlight.js/lib/languages/css";
import dockerfile from "highlight.js/lib/languages/dockerfile";
import go from "highlight.js/lib/languages/go";
import ini from "highlight.js/lib/languages/ini";
import java from "highlight.js/lib/languages/java";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import kotlin from "highlight.js/lib/languages/kotlin";
import lua from "highlight.js/lib/languages/lua";
import makefile from "highlight.js/lib/languages/makefile";
import markdown from "highlight.js/lib/languages/markdown";
import nix from "highlight.js/lib/languages/nix";
import php from "highlight.js/lib/languages/php";
import python from "highlight.js/lib/languages/python";
import ruby from "highlight.js/lib/languages/ruby";
import rust from "highlight.js/lib/languages/rust";
import scss from "highlight.js/lib/languages/scss";
import sql from "highlight.js/lib/languages/sql";
import swift from "highlight.js/lib/languages/swift";
import typescript from "highlight.js/lib/languages/typescript";
import xml from "highlight.js/lib/languages/xml";
import yaml from "highlight.js/lib/languages/yaml";

const LANGUAGES = {
  bash, c, cpp, css, dockerfile, go, ini, java, javascript, json, kotlin, lua, makefile, markdown, nix, php,
  python, ruby, rust, scss, sql, swift, typescript, xml, yaml,
};
for (const [name, lang] of Object.entries(LANGUAGES)) hljs.registerLanguage(name, lang);

const BY_EXTENSION: Record<string, string> = {
  rs: "rust", ts: "typescript", tsx: "typescript", mts: "typescript", cts: "typescript",
  js: "javascript", jsx: "javascript", mjs: "javascript", cjs: "javascript",
  py: "python", go: "go", json: "json", jsonl: "json", yml: "yaml", yaml: "yaml",
  toml: "ini", ini: "ini", cfg: "ini", conf: "ini", lock: "ini",
  sh: "bash", bash: "bash", zsh: "bash", envrc: "bash",
  css: "css", scss: "scss", html: "xml", htm: "xml", xml: "xml", svg: "xml", plist: "xml",
  md: "markdown", markdown: "markdown", sql: "sql", java: "java", kt: "kotlin", kts: "kotlin",
  c: "c", h: "c", cc: "cpp", cpp: "cpp", cxx: "cpp", hpp: "cpp", hh: "cpp",
  rb: "ruby", php: "php", swift: "swift", lua: "lua", nix: "nix",
};
const BY_NAME: Record<string, string> = {
  Makefile: "makefile", makefile: "makefile", GNUmakefile: "makefile", Dockerfile: "dockerfile",
  Containerfile: "dockerfile", Gemfile: "ruby", Rakefile: "ruby", ".envrc": "bash", ".bashrc": "bash", ".zshrc": "bash",
};

/** Bigger than this, a text is shown plain: highlighting it would stall the view. */
const MAX_HIGHLIGHT = 300_000;

/** The language a path's name says, if one is known here. */
export function languageOf(path: string): string | undefined {
  const name = path.split("/").pop() ?? path;
  if (BY_NAME[name]) return BY_NAME[name];
  const dot = name.lastIndexOf(".");
  return dot >= 0 ? BY_EXTENSION[name.slice(dot + 1).toLowerCase()] : undefined;
}

function escape(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/**
 * HTML split at its newlines into lines that each stand alone: a span still
 * open at a line's end is closed there and opened again on the next line.
 */
export function splitLines(html: string): string[] {
  const out: string[] = [];
  const open: string[] = [];
  let line = "";
  for (const m of html.matchAll(/(<span[^>]*>)|(<\/span>)|(\n)|([^<\n]+)/g)) {
    if (m[1]) {
      open.push(m[1]);
      line += m[1];
    } else if (m[2]) {
      open.pop();
      line += m[2];
    } else if (m[3]) {
      out.push(line + "</span>".repeat(open.length));
      line = open.join("");
    } else {
      line += m[4];
    }
  }
  out.push(line);
  return out;
}

/** `text` as HTML lines, highlighted as `lang` when known (escaped plain text otherwise). */
export function highlightLines(text: string, lang: string | undefined): string[] {
  if (lang && text.length <= MAX_HIGHLIGHT) {
    try {
      return splitLines(hljs.highlight(text, { language: lang, ignoreIllegals: true }).value);
    } catch {
      // Plain, below.
    }
  }
  return text.split("\n").map(escape);
}
