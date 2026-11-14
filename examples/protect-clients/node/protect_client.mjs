// Talk to Houndbit Protect from Node.js (example, not a finished SDK). No packages needed.
//
// Shows that any program can use Protect, not just Houndbit. Protect still decides and records
// only; it does not run anything yet.
//
//   node protect_client.mjs --app-id com.example.mytool --name "My tool" --read notes.txt
import crypto from "node:crypto";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";

const PROTOCOL_MAJOR = 1;

function args() {
  const a = { endpoint: defaultEndpoint(), read: "notes.txt", wait: 120 };
  const v = process.argv.slice(2);
  for (let i = 0; i < v.length; i += 2) a[v[i].replace(/^--/, "").replace(/-(.)/g, (_, c) => c.toUpperCase())] = v[i + 1];
  if (!a.appId || !a.name) { console.error("Use --app-id <id> --name <name>"); process.exit(1); }
  return a;
}

function defaultEndpoint() {
  if (process.platform === "win32") {
    const tag = (process.env.USERNAME || "user").replace(/[^A-Za-z0-9]/g, "_").toLowerCase();
    return `\\\\.\\pipe\\houndbit-protect-v1-${tag}`;
  }
  return path.join(os.homedir(), ".local", "share", "houndbit-protect", "engine.sock");
}

class Protect {
  static connect(endpoint) {
    return new Promise((ok, fail) => {
      const s = net.createConnection(endpoint); // works for Windows named pipes and Unix sockets
      s.once("connect", () => ok(new Protect(s)));
      s.once("error", fail);
    });
  }
  constructor(s) {
    this.s = s; this.buf = Buffer.alloc(0); this.waiting = []; this.next = 1;
    s.on("data", (d) => { this.buf = Buffer.concat([this.buf, d]); this.pump(); });
    s.on("close", () => this.waiting.splice(0).forEach((w) => w.fail(Object.assign(new Error("the engine closed the connection"), { code: null }))));
  }
  pump() {
    while (this.buf.length >= 4) {
      const n = this.buf.readUInt32BE(0);
      if (this.buf.length < 4 + n) return;
      const body = JSON.parse(this.buf.subarray(4, 4 + n).toString("utf8"));
      this.buf = this.buf.subarray(4 + n);
      const w = this.waiting.shift();
      if (body.error) w.fail(Object.assign(new Error(body.error.message), { code: body.error.data?.protect_code ?? null }));
      else w.ok(body.result);
    }
  }
  call(method, params = {}) {
    const body = Buffer.from(JSON.stringify({ jsonrpc: "2.0", id: `js-${this.next++}`, method, params }), "utf8");
    const head = Buffer.alloc(4); head.writeUInt32BE(body.length);
    return new Promise((ok, fail) => { this.waiting.push({ ok, fail }); this.s.write(Buffer.concat([head, body])); });
  }
  async signIn(appId, key) {
    const ch = await this.call("v1.auth.challenge", { protocol_major: PROTOCOL_MAJOR, app_id: appId });
    const msg = Buffer.from(`houndbit-protect-auth-v1\n${ch.engine_id}\n${ch.nonce}`);
    const signature = crypto.sign(null, msg, key).toString("base64");
    return this.call("v1.auth.prove", { protocol_major: PROTOCOL_MAJOR, app_id: appId, signature });
  }
  close() { this.s.end(); }
}

// Same key file format as the Rust and Python clients: base64 of the 32-byte secret.
const PKCS8_PREFIX = Buffer.from("302e020100300506032b657004220420", "hex");
function loadOrCreateKey(file) {
  let seed;
  if (fs.existsSync(file)) seed = Buffer.from(fs.readFileSync(file, "utf8").trim(), "base64");
  else {
    seed = crypto.randomBytes(32);
    fs.mkdirSync(path.dirname(path.resolve(file)), { recursive: true });
    fs.writeFileSync(file, seed.toString("base64"), { mode: 0o600 });
  }
  if (seed.length !== 32) throw new Error(`${file} doesn't hold a 32-byte key`);
  const key = crypto.createPrivateKey({ key: Buffer.concat([PKCS8_PREFIX, seed]), format: "der", type: "pkcs8" });
  const pub = crypto.createPublicKey(key).export({ format: "der", type: "spki" }).subarray(-32).toString("base64");
  return { key, pub };
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(what, check, seconds) {
  const end = Date.now() + seconds * 1000;
  while (Date.now() < end) { const r = await check(); if (r) return r; await sleep(500); }
  console.error(`Gave up waiting for ${what}.`); process.exit(1);
}

const a = args();
const { key, pub } = loadOrCreateKey(a.keyFile || `${a.appId}.key`);
let p;
try { p = await Protect.connect(a.endpoint); }
catch (e) { console.error(`Couldn't reach Houndbit Protect at ${a.endpoint} (${e.message}). Is it running?`); process.exit(1); }

try { await p.signIn(a.appId, key); }
catch (e) {
  if (e.code !== "AUTH_FAILED") throw e;
  const r = await p.call("v1.pairing.request", { protocol_major: PROTOCOL_MAJOR, app_id: a.appId, display_name: a.name, public_key: pub });
  console.log(`Asked to pair. Accept "${a.name}" in Houndbit Protect (pairing id ${r.pairing_id}).`);
  const state = await waitFor("the pairing to be accepted", async () => {
    const s = (await p.call("v1.pairing.status", { protocol_major: PROTOCOL_MAJOR, pairing_id: r.pairing_id })).state;
    return s !== "pending" && s;
  }, Number(a.wait));
  if (state !== "accepted") { console.error(`The pairing was ${state}.`); process.exit(1); }
  await p.signIn(a.appId, key);
}
console.log("Signed in.");
const hello = await p.call("v1.hello", { protocol_major: PROTOCOL_MAJOR });
console.log(`Protect ${hello.engine_version}, protocol ${hello.protocol_version}`);
console.log("Waiting for a folder to be added for this app...");
const grants = await waitFor("a folder", async () => { const g = (await p.call("v1.grants.list")).grants; return g.length && g; }, Number(a.wait));
console.log(`Folder: ${grants[0].folder}`);
const s = (await p.call("v1.sessions.request", { resource_grant_id: grants[0].grant_id })).id;
await p.call("v1.sessions.start", { session_id: s });
const act = await p.call("v1.actions.submit", { session_id: s, idempotency_key: `js-${Date.now()}`, operation_id: "files.read", arguments: { path: a.read } });
console.log(`Asked to read ${a.read}: decision=${act.decision} state=${act.state} executed=${act.executed}`);
const events = (await p.call("v1.events.read", { session_id: s })).events.map((e) => e.type);
console.log(`Record: ${events.join(", ")}`);
await p.call("v1.sessions.stop", { session_id: s });
console.log("RESULT " + JSON.stringify({ decision: act.decision, executed: act.executed, events }));
p.close();
