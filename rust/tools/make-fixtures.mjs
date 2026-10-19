// Dev scaffolding: writes tests/fixtures/*.eml and computes their expected
// values by transcribing src/aimap/parse.py in JS (independent of the Rust
// port). Run from rust/:  node tools/make-fixtures.mjs  — then delete/ignore.
import { writeFileSync, mkdirSync } from "node:fs";
import { createHash } from "node:crypto";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const dir = join(dirname(fileURLToPath(import.meta.url)), "..", "tests", "fixtures");
mkdirSync(dir, { recursive: true });

const b64 = (s) => Buffer.from(s, "utf8").toString("base64");
const sha256hex = (buf) => createHash("sha256").update(buf).digest("hex");

// ---- fixtures (exact bytes) -------------------------------------------------

const simple = [
  'From: "Ana Diaz" <Ana@Example.com>',
  "To: me@example.com",
  "Subject:  Quarterly   report",
  "Date: Thu, 01 Jan 2026 10:00:00 +0000",
  "Message-ID: <m1@example.com>",
  "",
  "Hi, the report is attached.",
  "-- ",
  "Ana",
  "",
].join("\r\n");

const htmlPart =
  "<html><style>x{}</style><p>Hello &amp; welcome to our newsletter</p>" +
  '<p>Deal: <a href="https://x.example/track">https://x.example/track</a></p></html>';

const newsletter = [
  "From: Weekly Deals <deals@shop.example>",
  "To: me@example.com",
  "Subject: =?utf-8?B?" + b64("Wöchentlicher Newsletter") + "?=",
  "Date: Thu, 01 Jan 2026 09:00:00 -0500",
  "Message-ID: <nl-2026-01@shop.example>",
  "List-Unsubscribe: <https://shop.example/u?tok=1>",
  "Precedence: bulk",
  "MIME-Version: 1.0",
  'Content-Type: multipart/mixed; boundary="b1"',
  "",
  "--b1",
  'Content-Type: multipart/alternative; boundary="b2"',
  "",
  "--b2",
  "Content-Type: text/plain; charset=utf-8",
  "Content-Transfer-Encoding: quoted-printable",
  "",
  "Hello & welcome to our newsletter\r",
  "See inside for deals.",
  "--b2",
  "Content-Type: text/html; charset=utf-8",
  "Content-Transfer-Encoding: base64",
  "",
  b64(htmlPart),
  "--b2--",
  "--b1",
  'Content-Type: image/png; name="logo.png"',
  "Content-Transfer-Encoding: base64",
  'Content-Disposition: attachment; filename="logo.png"',
  "",
  "iVBORw0KGgo=",
  "--b1--",
  "",
].join("\r\n");

const reply = [
  "From: =?utf-8?q?Jos=C3=A9_Garc=C3=ADa?= <jose@example.com>",
  "To: me@example.com",
  "Subject: RE: Meeting\r\n Invariants",
  "Date: Fri, 02 Jan 2026 08:30:00 +0100",
  "In-Reply-To: <m1@example.com>",
  "References: <m0@example.com> <m1@example.com>",
  "Message-ID: <m2@example.com>",
  "",
  "Sounds good.",
  "",
  "On Thu, Jan 1, 2026 at 10:00 AM Ana <ana@example.com> wrote:",
  "> Hi, the report is attached.",
  "",
].join("\r\n");

const attachments = [
  "From: Big <big@example.com>",
  "To: me@example.com",
  "Subject: Files",
  "Date: Thu, 01 Jan 2026 08:00:00 +0000",
  "Message-ID: <files-1@big.example>",
  'Content-Type: multipart/mixed; boundary="bb"',
  "",
  "--bb",
  "Content-Type: text/plain",
  "",
  "See attached.",
  "--bb",
  'Content-Type: application/pdf; name="report.pdf"',
  'Content-Disposition: attachment; filename="report.pdf"',
  "",
  "JVBERi0=",
  "--bb",
  "Content-Type: text/plain; charset=utf-8",
  'Content-Disposition: attachment; filename="notes.txt"',
  "",
  "bm90ZXM=",
  "--bb",
  'Content-Type: application/zip; name="data.zip"',
  "",
  "VVRaSVA=",
  "--bb--",
  "",
].join("\r\n");

const noMessageId = [
  "From: Shop <deals@shop.example>",
  "To: me@example.com",
  "Subject: Order confirmation",
  "Date: Thu, 01 Jan 2026 11:30:00 +0000",
  "",
  "Your order has shipped.",
  "",
].join("\r\n");

const garbage = Buffer.from([0xff, 0xfe, 0x20, ...Buffer.from("not an email", "utf8")]);

// ---- JS transcription of parse.py (golden computation) ----------------------

const invisible = /[\u200b-\u200f\u2060\u00ad\u034f\ufeff\u00a0]+/g;
const url = /https?:\/\/\S+|www\.\S+/g;
const replyPrefix = /^\s*(re|fw|fwd|aw|r|tr|sv|enc|rif|res|wg|antw|vs|ynt)\s*:\s*/iy;

function unescapeHtml(s) {
  const named = { amp: "&", lt: "<", gt: ">", quot: '"', apos: "'", nbsp: "\u00a0" };
  return s
    .replace(/&#x([0-9a-f]+);/gi, (_, h) => String.fromCodePoint(parseInt(h, 16)))
    .replace(/&#(\d+);/g, (_, d) => String.fromCodePoint(parseInt(d, 10)))
    .replace(/&([a-z]+);/gi, (m, name) => (name in named ? named[name] : m));
}

function casefold(s) {
  return s.toLowerCase().replaceAll("ß", "ss");
}

function normalizeSubject(subject) {
  if (!subject) return "";
  let s = subject;
  for (;;) {
    const prev = s;
    s = s.replace(replyPrefix, "");
    if (s === prev) break;
    replyPrefix.lastIndex = 0;
  }
  return casefold(s.replace(/\s+/g, " ").trim());
}

const SKIP = new Set(["style", "script", "head", "title"]);
const BLOCK = new Set(["p", "div", "br", "tr", "li", "h1", "h2", "h3", "h4", "table", "td"]);

function extractHtmlText(html) {
  const parts = [];
  let skip = 0;
  let i = 0;
  while (i < html.length) {
    const lt = html.indexOf("<", i);
    if (lt === -1) {
      if (!skip) parts.push(unescapeHtml(html.slice(i)));
      break;
    }
    if (!skip) parts.push(unescapeHtml(html.slice(i, lt)));
    if (html.startsWith("<!--", lt)) {
      const end = html.indexOf("-->", lt);
      if (end === -1) break;
      i = end + 3;
      continue;
    }
    let j = lt + 1;
    let quote = null;
    while (j < html.length) {
      const c = html[j];
      if (quote) { if (c === quote) quote = null; }
      else if (c === '"' || c === "'") quote = c;
      else if (c === ">") break;
      j++;
    }
    if (j >= html.length) { // unterminated tag: rest is data
      if (!skip) parts.push(unescapeHtml(html.slice(lt)));
      break;
    }
    const src = html.slice(lt + 1, j).trim();
    i = j + 1;
    if (src.startsWith("!") || src.startsWith("?")) continue;
    const endtag = src.startsWith("/");
    const selfclosing = src.endsWith("/");
    const name = (endtag ? src.slice(1) : src).match(/^[a-zA-Z0-9-:]*/)[0].toLowerCase();
    if (!name) continue;
    if (SKIP.has(name)) {
      if (endtag) skip = Math.max(0, skip - 1);
      else if (!selfclosing) skip += 1;
    } else if (BLOCK.has(name) && !endtag) {
      parts.push("\n");
    }
  }
  return parts.join("");
}

function cleanText(text) {
  text = unescapeHtml(text).replace(invisible, " ");
  text = text.replace(url, "");
  return text
    .split(/\r\n|\r|\n/)
    .map((ln) => ln.replace(/[ \t]+/g, " ").trim())
    .filter(Boolean)
    .join("\n");
}

function truncate(text, maxChars) {
  if ([...text].length > maxChars) {
    const cut = [...text].slice(0, maxChars).join("");
    const idx = cut.lastIndexOf(" ");
    return (idx === -1 ? cut : cut.slice(0, idx)) + " [...]";
  }
  return text;
}

// hand-derived expected values (headers were authored above; bodies via transcription)
const goldens = {
  "simple": {
    meta: {
      rfc822_message_id: "<m1@example.com>",
      from_email: "ana@example.com",
      from_name: "Ana Diaz",
      subject: "Quarterly report",
      sent_at: "2026-01-01T10:00:00Z",
      in_reply_to: null,
      references: null,
      has_list_headers: false,
    },
    thread_id: "<m1@example.com>",
    attachments: [],
    body_text: "Hi, the report is attached.\n--\nAna",
  },
  "newsletter": {
    meta: {
      rfc822_message_id: "<nl-2026-01@shop.example>",
      from_email: "deals@shop.example",
      from_name: "Weekly Deals",
      subject: "Wöchentlicher Newsletter",
      sent_at: "2026-01-01T14:00:00Z",
      in_reply_to: null,
      references: null,
      has_list_headers: true,
    },
    thread_id: "<nl-2026-01@shop.example>",
    attachments: ["logo.png"],
    body_text: truncate(cleanText(extractHtmlText(htmlPart)), 1500),
    body_text_100: truncate(cleanText(extractHtmlText(htmlPart)), 100),
  },
  "reply": {
    meta: {
      rfc822_message_id: "<m2@example.com>",
      from_email: "jose@example.com",
      from_name: "José García",
      subject: "RE: Meeting Invariants",
      sent_at: "2026-01-02T07:30:00Z",
      in_reply_to: "<m1@example.com>",
      references: "<m0@example.com> <m1@example.com>",
      has_list_headers: false,
    },
    thread_id: "<m0@example.com>",
    attachments: [],
    body_text: "Sounds good.\nOn Thu, Jan 1, 2026 at 10:00 AM Ana <ana@example.com> wrote:\n> Hi, the report is attached.",
    stripped: "Sounds good.",
  },
  "attachments": {
    meta: {
      rfc822_message_id: "<files-1@big.example>",
      from_email: "big@example.com",
      from_name: "Big",
      subject: "Files",
      sent_at: "2026-01-01T08:00:00Z",
      in_reply_to: null,
      references: null,
      has_list_headers: false,
    },
    thread_id: "<files-1@big.example>",
    attachments: ["report.pdf", "notes.txt", "data.zip"],
    body_text: "See attached.",
  },
  "no-message-id": {
    meta: {
      rfc822_message_id: null, // filled below
      from_email: "deals@shop.example",
      from_name: "Shop",
      subject: "Order confirmation",
      sent_at: "2026-01-01T11:30:00Z",
      in_reply_to: null,
      references: null,
      has_list_headers: false,
    },
    thread_id: null, // filled below
    attachments: [],
    body_text: "Your order has shipped.",
  },
  "garbage": {
    meta: {
      rfc822_message_id: null, // filled below
      from_email: null,
      from_name: null,
      subject: null,
      sent_at: null,
      in_reply_to: null,
      references: null,
      has_list_headers: false,
    },
    thread_id: null, // filled below
    attachments: [],
    // No blank line: everything is a defect header block, so the body is empty
    // (same in Python's email parser — its test only pins parse_meta here).
    body_text: "",
  },
};

const files = {
  "simple.eml": simple,
  "newsletter.eml": newsletter,
  "reply.eml": reply,
  "attachments.eml": attachments,
  "no-message-id.eml": noMessageId,
  "garbage.eml": garbage,
};

for (const [name, content] of Object.entries(files)) {
  writeFileSync(join(dir, name), content);
}

// hash-dependent goldens
const nmidBytes = files["no-message-id.eml"];
const nmidHash = "sha256:" + sha256hex(nmidBytes);
goldens["no-message-id"].meta.rfc822_message_id = nmidHash;
goldens["no-message-id"].thread_id =
  "thread:" + sha256hex(Buffer.from(normalizeSubject("Order confirmation") + "|", "utf8")).slice(0, 16);

const garbageHash = "sha256:" + sha256hex(files["garbage.eml"]);
goldens["garbage"].meta.rfc822_message_id = garbageHash;
goldens["garbage"].thread_id =
  "thread:" + sha256hex(Buffer.from("|", "utf8")).slice(0, 16);

for (const [name, golden] of Object.entries(goldens)) {
  const out = `${name}.expected.json`;
  writeFileSync(join(dir, out), JSON.stringify(golden, null, 2) + "\n");
}

console.log("fixtures + goldens written to", dir);
