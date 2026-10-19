// Diff harness: run the Python and Rust dumpers over a directory of .eml
// files and report any behavioral divergence.
//
//   node tools/diff-impls.mjs <eml-dir> [--show N]
//
// Requires: a Python 3.11+ interpreter on PATH (or PYTHON_BIN env) that can
// import src/aimap (stdlib only), and cargo to build the Rust example.

import { spawnSync } from "node:child_process";
import { mkdirSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { join } from "node:path";

const dir = process.argv[2];
const showIdx = process.argv.indexOf("--show");
const show = showIdx === -1 ? 5 : Number(process.argv[showIdx + 1]);
if (!dir) {
  console.error("usage: node tools/diff-impls.mjs <eml-dir>");
  process.exit(2);
}

const rustRoot = new URL("..", import.meta.url).pathname;
const pythonBin = process.env.PYTHON_BIN ?? "python3";

spawnSync("cargo", ["build", "--release", "--example", "dump"], {
  cwd: rustRoot,
  stdio: "inherit",
});

const rustDump = join(rustRoot, "target/x86_64-unknown-linux-musl/release/examples/dump");
const pythonDump = join(rustRoot, "tools/dump-python.py");

const files = readdirSync(dir, { recursive: true }).filter((f) => f.endsWith(".eml"));
const outDir = "/tmp/aimap-diff";
rmSync(outDir, { recursive: true, force: true });
mkdirSync(join(outDir, "py"), { recursive: true });
mkdirSync(join(outDir, "rs"), { recursive: true });

const toEpoch = (s) => (s == null ? null : Date.parse(s));

function run(cmd, args, input) {
  const r = spawnSync(cmd, args, { input, maxBuffer: 64 * 1024 * 1024 });
  if (r.status !== 0) {
    return { error: (r.stderr?.toString() ?? "").split("\n").slice(-3).join("; ") };
  }
  return { json: JSON.parse(r.stdout.toString()) };
}

let match = 0;
const mismatches = [];
let errors = 0;

for (const file of files) {
  const raw = readFileSync(join(dir, file));
  const py = run(pythonBin, [pythonDump], raw);
  const rs = run(rustDump, [], raw);
  const slug = file.replaceAll("/", "_");
  if (py.error || rs.error) {
    errors += 1;
    console.error(`ERROR ${file}: py=${py.error ?? "ok"} rs=${rs.error ?? "ok"}`);
    continue;
  }
  const diffs = [];
  const pm = py.json.meta;
  const rm = rs.json.meta;
  for (const k of ["rfc822_message_id", "from_email", "from_name", "subject", "in_reply_to", "references"]) {
    if ((pm[k] ?? null) !== (rm[k] ?? null)) diffs.push(`meta.${k}: py=${JSON.stringify(pm[k])} rs=${JSON.stringify(rm[k])}`);
  }
  if (toEpoch(pm.sent_at) !== toEpoch(rm.sent_at)) diffs.push(`meta.sent_at: py=${pm.sent_at} rs=${rm.sent_at}`);
  if (pm.has_list_headers !== rm.has_list_headers) diffs.push(`meta.has_list_headers: py=${pm.has_list_headers} rs=${rm.has_list_headers}`);
  const cp = (s) => [...s].length; // code points, like Python len()
  if (py.json.body_text !== rs.json.body_text) {
    diffs.push(`body_text: chars py=${cp(py.json.body_text)} rs=${cp(rs.json.body_text)}`);
  }
  if (diffs.length === 0) {
    match += 1;
  } else {
    mismatches.push({ file, diffs });
    for (const d of diffs) console.error(`DIFF ${file}: ${d}`);
  }
}

console.log(`\n${files.length} messages: ${match} identical, ${mismatches.length} divergent, ${errors} errors`);
if (mismatches.length > 0) {
  console.log(`\nfirst ${Math.min(show, mismatches.length)} divergent files:`);
  for (const m of mismatches.slice(0, show)) console.log(`  ${m.file}\n    ${m.diffs.join("\n    ")}`);
}
