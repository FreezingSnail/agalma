#!/usr/bin/env node
// Convert the documentation linked from docs/progress-roadmap.html into rich,
// self-contained HTML (no network, no CDN, no JS required to read).
//
// Usage:
//   node scripts/md-to-html.mjs [file...]
//
// With no arguments it derives the set from the roadmap: every linked
// *.html/*.md target under docs/ is converted from its .md source, so the HTML
// and the markdown stay in sync. Links between converted docs are rewritten
// from .md to .html.
//
// Node stdlib only. No dependencies, no network.

import { readFileSync, writeFileSync, existsSync, readdirSync } from "node:fs";
import { dirname, join, basename, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, "..");
const docsDir = join(repoRoot, "docs");
const ROADMAP = "progress-roadmap.html";

// Recommended ingestion order — "best path through these components".
// Contracts first, then the transport, then the Tier-1 service seams, the
// execution engine, the runtime/loop docs, then the supporting design and
// evidence documents. Prev/next links form a ring (last wraps to first).
const ORDER = [
  "component-contracts.md",   // the contracts every component implements
  "ipc-connect-design.md",    // transport that carries every seam
  "b1-ledger-evidence.md",    // Tier-1 seam: canonical durable state
  "b1-taskqueue-evidence.md", // Tier-1 seam: backlog
  "b1-harness-evidence.md",   // Tier-1 seam: agent runtime
  "b1-sandbox-evidence.md",   // Tier-1 seam: confinement (frozen historical)
  "sandbox-removal-design.md",// amendment: remove sandbox → process launcher
  "b1-workspace-evidence.md", // Tier-1 seam: git
  "b1-execution-evidence.md", // Tier-1 seam: executor
  "b0-kernel.md",             // durable execution kernel design
  "m1-workloop.md",           // reference harness + decision baselines
  "self-hosting-roadmap.md",  // target architecture / plan
  "self-mutation-roadmap.md", // remaining roadmap
  "m0-exit-evidence.md",      // milestone evidence
  "m1-exit-evidence.md",
  "hardening-evidence.md",
  "reliability-hardening.md",
  "paid-route-evidence.md",
  "b1-extraction.md",
  "b1-exit-evidence.md",
  "b1-f3-evidence.md",
  "b0-exit-evidence.md",
  "ipc-exit-evidence.md",
  "planning-pipeline.md",
  "tui-design.md",
  "tui.md",
  "self-mutation-contracts.md",
  "self-mutation-standup.md",
  "smart-routing-design.md",
  "web-dashboard-design.md",
  "testing-strategy.md",
];

// Full ordered ring; any doc not in ORDER is appended (sorted) after it.
function ring(extra = []) {
  const seen = new Set(ORDER);
  const tail = extra.filter((f) => !seen.has(f)).sort();
  return [...ORDER, ...tail];
}

function ringFor(name, ordered) {
  const i = ordered.indexOf(name);
  const n = ordered.length;
  if (i === -1) return { prev: ordered[n - 1], next: ordered[0], step: 0, n };
  return {
    prev: ordered[(i - 1 + n) % n],
    next: ordered[(i + 1) % n],
    step: i + 1,
    n,
  };
}

function mapHtml(ordered, current) {
  const items = ordered
    .map((f) => {
      const label = f.replace(/\.md$/, "");
      const cls = f === current ? ' class="here"' : "";
      return `<li${cls}><a href="${esc(rewriteUrl(f))}">${esc(label)}</a></li>`;
    })
    .join("\n      ");
  return `<details class="map"${current === ordered[0] ? " open" : ""}>
    <summary>Component ingestion map — recommended order (${ordered.length} docs)</summary>
    <ol>
      ${items}
    </ol>
  </details>`;
}


const CSS = `
  :root{
    --bg:#0b1120; --card:#111c31; --card2:#152238; --border:#233350;
    --text:#e6edf7; --muted:#8fa3c0; --green:#34d399; --blue:#60a5fa;
    --amber:#fbbf24; --red:#f87171; --purple:#a78bfa;
  }
  *{box-sizing:border-box}
  body{margin:0;background:var(--bg);color:var(--text);
       font:15px/1.6 -apple-system,BlinkMacSystemFont,"Segoe UI",Roboto,Helvetica,Arial,sans-serif}
  .wrap{max-width:900px;margin:0 auto;padding:32px 24px 96px}
  .nav{display:flex;gap:16px;font-size:13px;color:var(--muted);margin-bottom:22px}
  h1{font-size:26px;margin:0 0 4px;line-height:1.25}
  h2{font-size:20px;margin:38px 0 12px;border-bottom:1px solid var(--border);padding-bottom:8px}
  h3{font-size:16px;margin:26px 0 8px}
  h4{font-size:14px;margin:20px 0 6px;color:var(--muted);text-transform:uppercase;letter-spacing:.04em}
  a{color:var(--blue);text-decoration:none} a:hover{text-decoration:underline}
  p{margin:12px 0}
  code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:12.8px;
       background:var(--card2);border:1px solid var(--border);border-radius:5px;padding:1px 5px}
  pre.code{background:#0d1526;border:1px solid var(--border);border-radius:10px;
           padding:14px 16px;overflow:auto;margin:16px 0}
  pre.code code{background:none;border:none;padding:0;font-size:12.6px;line-height:1.5}
  ul,ol{margin:10px 0;padding-left:24px} li{margin:4px 0}
  hr{border:none;border-top:1px solid var(--border);margin:28px 0}
  table{border-collapse:collapse;width:100%;font-size:13px;margin:16px 0}
  th,td{text-align:left;padding:7px 10px;border-bottom:1px solid var(--border);vertical-align:top}
  th{color:var(--muted);font-weight:600;font-size:12px;text-transform:uppercase;letter-spacing:.04em}
  blockquote{margin:14px 0;padding:10px 14px;background:var(--card2);
             border-left:3px solid var(--blue);border-radius:6px;color:#c6d3e8}
  strong{color:#f2f6fd}
  footer{margin-top:56px;color:var(--muted);font-size:12.5px;
         border-top:1px solid var(--border);padding-top:14px}
  .ring{display:flex;flex-wrap:wrap;gap:10px;align-items:center;margin:14px 0 26px;
        font-size:13px}
  .ring a{background:var(--card);border:1px solid var(--border);border-radius:8px;
          padding:6px 12px}
  .ring a:hover{border-color:var(--blue);text-decoration:none}
  .ring .step{color:var(--muted);font-size:12px;margin-left:auto}
  details.map{margin:26px 0;background:var(--card);border:1px solid var(--border);
              border-radius:10px;padding:12px 16px}
  details.map>summary{cursor:pointer;color:var(--muted);font-size:13px;font-weight:600}
  details.map ol{margin:12px 0 4px;padding-left:26px}
  details.map li{margin:3px 0;font-size:13.5px}
  details.map li.here{color:var(--text);font-weight:700}
  details.map li.here::after{content:" ← you are here";color:var(--amber);font-weight:600;font-size:11.5px}
`;

function esc(s) {
  return String(s)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

function slug(s) {
  return s
    .toLowerCase()
    .replace(/[^\w\s-]/g, "")
    .trim()
    .replace(/\s+/g, "-");
}

// Rewrite a link target so converted docs point at sibling HTML.
function rewriteUrl(u) {
  if (/^(https?:|mailto:|#)/.test(u)) return u;
  return u.replace(/\.md(?=($|#))/, ".html");
}

function inline(input) {
  let s = String(input);
  // hard line breaks (two+ trailing spaces) before joining soft wraps
  s = s.replace(/ {2,}\n/g, "\u0002");
  s = s.replace(/\n/g, " ");
  // code spans first, kept verbatim
  const codes = [];
  s = s.replace(/`([^`]+)`/g, (_, c) => {
    codes.push(c);
    return `\u0000${codes.length - 1}\u0000`;
  });
  s = esc(s);
  // inline links
  s = s.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g,
    (_, t, u) => `<a href="${esc(rewriteUrl(u))}">${t}</a>`);
  // bold, then italic (underscore italic needs word boundaries)
  s = s.replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>");
  s = s.replace(/__([^_]+)__/g, "<strong>$1</strong>");
  s = s.replace(/(^|[^\w])_([^_\n]+)_(?=[^\w]|$)/g, "$1<em>$2</em>");
  s = s.replace(/\*([^*\n]+)\*/g, "<em>$1</em>");
  // restore code spans
  s = s.replace(/\u0000(\d+)\u0000/g, (_, n) => `<code>${esc(codes[+n])}</code>`);
  s = s.replace(/\u0002/g, "<br>");
  return s;
}

const HEADING = /^(#{1,6})\s+(.*?)\s*#*\s*$/;
const FENCE = /^\s*(```|~~~)\s*([\w+-]*)\s*$/;
const HR = /^\s*([-*_])(\s*\1){2,}\s*$/;
const ITEM = /^(\s*)([-*+]|\d+[.)])\s+(.*)$/;
const TABLE_SEP = /^\s*\|?\s*:?-{2,}:?\s*(\|\s*:?-{2,}:?\s*)*\|?\s*$/;

function isBlockStart(line, next) {
  return HEADING.test(line) || FENCE.test(line) || HR.test(line) ||
    ITEM.test(line) || (line.includes("|") && next !== undefined && TABLE_SEP.test(next));
}

function splitRow(line) {
  let l = line.trim();
  if (l.startsWith("|")) l = l.slice(1);
  if (l.endsWith("|")) l = l.slice(0, -1);
  return l.split("|").map((c) => c.trim());
}

function alignOf(sep) {
  const s = sep.trim();
  const left = s.startsWith(":");
  const right = s.endsWith(":");
  if (left && right) return "center";
  if (right) return "right";
  if (left) return "left";
  return "";
}

function renderTable(lines, i) {
  const header = splitRow(lines[i]);
  const aligns = splitRow(lines[i + 1]).map(alignOf);
  i += 2;
  const rows = [];
  while (i < lines.length && lines[i].includes("|") && !/^\s*$/.test(lines[i])) {
    rows.push(splitRow(lines[i]));
    i++;
  }
  const cell = (tag, c, j) => {
    const a = aligns[j] ? ` style="text-align:${aligns[j]}"` : "";
    return `<${tag}${a}>${inline(c)}</${tag}>`;
  };
  const thead = `<thead><tr>${header.map((h, j) => cell("th", h, j)).join("")}</tr></thead>`;
  const tbody = `<tbody>${rows
    .map((r) => `<tr>${r.map((c, j) => cell("td", c, j)).join("")}</tr>`)
    .join("")}</tbody>`;
  return { html: `<table>${thead}${tbody}</table>`, next: i };
}

// Collect consecutive list lines (plus indented continuations) as flat items.
function collectItems(lines, start) {
  const items = [];
  let i = start;
  while (i < lines.length) {
    const line = lines[i];
    const m = line.match(ITEM);
    if (m) {
      items.push({
        indent: m[1].replace(/\t/g, "    ").length,
        ordered: /\d/.test(m[2]),
        text: m[3],
      });
      i++;
    } else if (items.length && /^\s+\S/.test(line) &&
               !isBlockStart(line, lines[i + 1])) {
      items[items.length - 1].text += "\n" + line.trim();
      i++;
    } else {
      break;
    }
  }
  return { items, next: i };
}

function buildList(items) {
  const out = [];
  const stack = []; // { indent, ordered }
  const openList = (ordered) => out.push(ordered ? "<ol>" : "<ul>");
  const closeList = () => out.push(stack[stack.length - 1].ordered ? "</ol>" : "</ul>");
  for (const it of items) {
    if (stack.length === 0) {
      stack.push({ indent: it.indent, ordered: it.ordered });
      openList(it.ordered);
      out.push(`<li>${inline(it.text)}`);
      continue;
    }
    const top = stack[stack.length - 1];
    if (it.indent > top.indent) {
      // nested list inside the currently-open <li>
      stack.push({ indent: it.indent, ordered: it.ordered });
      openList(it.ordered);
      out.push(`<li>${inline(it.text)}`);
      continue;
    }
    // close the deepest open <li>, then unwind any deeper lists
    out.push("</li>");
    while (stack.length > 1 && it.indent < stack[stack.length - 1].indent) {
      closeList();
      out.push("</li>");
      stack.pop();
    }
    const t2 = stack[stack.length - 1];
    if (t2.ordered !== it.ordered) {
      closeList();
      stack[stack.length - 1] = { indent: it.indent, ordered: it.ordered };
      openList(it.ordered);
    }
    out.push(`<li>${inline(it.text)}`);
  }
  while (stack.length) {
    out.push("</li>");
    closeList();
    stack.pop();
  }
  return out.join("");
}

function render(md) {
  const lines = md.replace(/\r\n?/g, "\n").split("\n");
  const out = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];
    if (/^\s*$/.test(line)) { i++; continue; }

    const fence = line.match(FENCE);
    if (fence) {
      const close = fence[1] === "```" ? /^\s*```\s*$/ : /^\s*~~~\s*$/;
      const lang = fence[2];
      const buf = [];
      i++;
      while (i < lines.length && !close.test(lines[i])) { buf.push(lines[i]); i++; }
      i++; // consume closing fence (or EOF)
      const cls = lang ? ` class="lang-${esc(lang)}"` : "";
      out.push(`<pre class="code"><code${cls}>${esc(buf.join("\n"))}</code></pre>`);
      continue;
    }

    const heading = line.match(HEADING);
    if (heading) {
      const lvl = heading[1].length;
      out.push(`<h${lvl} id="${slug(heading[2])}">${inline(heading[2])}</h${lvl}>`);
      i++;
      continue;
    }

    if (HR.test(line)) { out.push("<hr>"); i++; continue; }

    if (line.includes("|") && i + 1 < lines.length && TABLE_SEP.test(lines[i + 1])) {
      const t = renderTable(lines, i);
      out.push(t.html);
      i = t.next;
      continue;
    }

    if (ITEM.test(line)) {
      const { items, next } = collectItems(lines, i);
      out.push(buildList(items));
      i = next;
      continue;
    }

    // paragraph
    const buf = [];
    while (i < lines.length && !/^\s*$/.test(lines[i]) &&
           !(buf.length > 0 && isBlockStart(lines[i], lines[i + 1]))) {
      buf.push(lines[i]);
      i++;
    }
    out.push(`<p>${inline(buf.join("\n"))}</p>`);
  }
  return out.join("\n");
}

function ringNav(sourceName, ordered, position) {
  const pos = position === "bottom" ? "margin:32px 0 0" : "";
  const r = ringFor(sourceName, ordered);
  const label = (f) => esc(f.replace(/\.md$/, ""));
  const prev = `<a href="${esc(rewriteUrl(r.prev))}">⟲ ${label(r.prev)}</a>`;
  const next = `<a href="${esc(rewriteUrl(r.next))}">${label(r.next)} ⟳</a>`;
  const step = r.step ? `step ${r.step} / ${r.n} in the ring` : `${r.n} docs in the ring`;
  return `<div class="ring"${pos ? ` style="${pos}"` : ""}>${prev}<span class="step">${step}</span>${next}</div>`;
}

function page(title, sourceName, body, ordered) {
  return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)} — Agalma docs</title>
<style>${CSS}</style>
</head>
<body>
<div class="wrap">
<div class="nav">← <a href="${ROADMAP}">Progress roadmap</a> · source: <a href="${esc(sourceName)}">${esc(sourceName)}</a></div>
${ringNav(sourceName, ordered, "top")}
${mapHtml(ordered, sourceName)}
<article>
${body}
</article>
${ringNav(sourceName, ordered, "bottom")}
<footer>Rendered from <span style="font-family:ui-monospace,monospace">${esc(sourceName)}</span> by <span style="font-family:ui-monospace,monospace">scripts/md-to-html.mjs</span>. Edit the markdown, then run <span style="font-family:ui-monospace,monospace">make docs-html</span>.</footer>
</div>
</body>
</html>
`;
}

function firstHeading(md) {
  const m = md.match(HEADING);
  return m ? m[2] : "(untitled)";
}

function convert(mdPath, outPath, ordered) {
  const md = readFileSync(mdPath, "utf8");
  const sourceName = basename(mdPath);
  const html = page(firstHeading(md), sourceName, render(md), ordered);
  writeFileSync(outPath, html);
  return `${basename(mdPath)} -> ${basename(outPath)} (${html.length} bytes)`;
}

function referencedDocs() {
  const roadmap = join(docsDir, ROADMAP);
  const refs = new Set();
  if (!existsSync(roadmap)) return [];
  const html = readFileSync(roadmap, "utf8");
  for (const m of html.matchAll(/href="([^"#]+\.(?:md|html))"/g)) {
    let f = decodeURIComponent(m[1]).replace(/\.(md|html)$/, ".md");
    if (f === ROADMAP.replace(/\.html$/, ".md")) continue;
    refs.add(f);
  }
  return [...refs].sort().filter((f) => existsSync(join(docsDir, f)));
}

function main() {
  const args = process.argv.slice(2).filter((a) => !a.startsWith("-"));
  const files = args.length ? args : referencedDocs();
  if (files.length === 0) {
    console.error("no documents to convert");
    process.exit(1);
  }
  const ordered = ring(files.map((f) => basename(f)));
  let ok = 0;
  for (const f of files) {
    const mdPath = f.endsWith(".md") ? f : `${f}.md`;
    const abs = existsSync(mdPath) ? mdPath : join(docsDir, mdPath);
    if (!existsSync(abs)) { console.error(`skip (missing): ${abs}`); continue; }
    const outPath = abs.replace(/\.md$/, ".html");
    console.log(convert(abs, outPath, ordered));
    ok++;
  }
  console.log(`\nconverted ${ok}/${files.length} document(s); ring=${ordered.length}`);
}

main();
