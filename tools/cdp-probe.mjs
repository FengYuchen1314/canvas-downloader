import fs from "node:fs";
import path from "node:path";

const DEBUG_ORIGIN = process.env.CDP_ORIGIN ?? "http://127.0.0.1:9222";
const mode = process.argv[2] ?? "snapshot";

async function canvasTarget() {
  const targets = await fetch(`${DEBUG_ORIGIN}/json`).then((response) => response.json());
  const target = targets.find(
    (item) => item.type === "page" && new URL(item.url).hostname === "oc.sjtu.edu.cn",
  );
  if (!target) throw new Error("没有找到已打开的 Canvas 标签页");
  return target;
}

class CdpConnection {
  constructor(url) {
    this.socket = new WebSocket(url);
    this.nextId = 1;
    this.pending = new Map();
    this.listeners = new Set();
  }

  async open() {
    await new Promise((resolve, reject) => {
      this.socket.addEventListener("open", resolve, { once: true });
      this.socket.addEventListener("error", reject, { once: true });
    });
    this.socket.addEventListener("message", (event) => {
      const message = JSON.parse(event.data);
      if (message.id && this.pending.has(message.id)) {
        const { resolve, reject } = this.pending.get(message.id);
        this.pending.delete(message.id);
        if (message.error) reject(new Error(message.error.message));
        else resolve(message.result);
        return;
      }
      for (const listener of this.listeners) listener(message);
    });
  }

  send(method, params = {}, sessionId) {
    const id = this.nextId++;
    const payload = { id, method, params };
    if (sessionId) payload.sessionId = sessionId;
    this.socket.send(JSON.stringify(payload));
    return new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
  }

  onEvent(listener) {
    this.listeners.add(listener);
  }
}

async function snapshot(connection) {
  await connection.send("Runtime.enable");
  const expression = `JSON.stringify({
    title: document.title,
    url: location.href,
    headings: [...document.querySelectorAll('h1,h2,h3')].slice(0, 30).map((node) => node.textContent.trim()).filter(Boolean),
    links: [...document.querySelectorAll('a[href]')].slice(0, 150).map((node) => ({
      text: node.textContent.trim().replace(/\\s+/g, ' ').slice(0, 120),
      href: node.href
    })).filter((item) => item.text || item.href),
    frames: [...document.querySelectorAll('iframe')].map((node) => ({ title: node.title, src: node.src }))
  })`;
  const result = await connection.send("Runtime.evaluate", {
    expression,
    returnByValue: true,
  });
  process.stdout.write(`${result.result.value}\n`);
}

function safeNetworkRecord(message) {
  if (message.method === "Network.requestWillBeSent") {
    const { request, type, documentURL, initiator } = message.params;
    return {
      at: new Date().toISOString(),
      event: "request",
      method: request.method,
      type,
      url: request.url,
      documentUrl: documentURL,
      initiatorUrl: initiator?.url ?? initiator?.stack?.callFrames?.[0]?.url,
    };
  }
  if (message.method === "Network.responseReceived") {
    const { response, type } = message.params;
    return {
      at: new Date().toISOString(),
      event: "response",
      status: response.status,
      mimeType: response.mimeType,
      type,
      url: response.url,
    };
  }
  return null;
}

function isInteresting(record) {
  if (!record) return false;
  const value = `${record.url} ${record.mimeType ?? ""}`.toLowerCase();
  return (
    record.type === "Media" ||
    /\.(m3u8|mpd|mp4|m4v|webm|m4a|ts)(?:[?#]|$)/.test(value) ||
    /(mpegurl|dash\+xml|video\/|audio\/)/.test(value) ||
    /(video|vod|stream|playback|playlist|manifest)/.test(value)
  );
}

async function monitor(connection) {
  const outputDir = path.resolve(".explore-logs");
  fs.mkdirSync(outputDir, { recursive: true });
  const outputPath = path.join(outputDir, "network.jsonl");
  fs.writeFileSync(outputPath, "", "utf8");

  const sessions = new Set();
  connection.onEvent(async (message) => {
    if (message.method === "Target.attachedToTarget") {
      const sessionId = message.params.sessionId;
      if (!sessions.has(sessionId)) {
        sessions.add(sessionId);
        await connection.send("Network.enable", {}, sessionId).catch(() => {});
        await connection
          .send(
            "Target.setAutoAttach",
            { autoAttach: true, flatten: true, waitForDebuggerOnStart: false },
            sessionId,
          )
          .catch(() => {});
      }
      return;
    }
    const record = safeNetworkRecord(message);
    if (isInteresting(record)) {
      fs.appendFileSync(outputPath, `${JSON.stringify(record)}\n`, "utf8");
    }
  });

  await connection.send("Network.enable");
  await connection.send("Target.setAutoAttach", {
    autoAttach: true,
    flatten: true,
    waitForDebuggerOnStart: false,
  });
  process.stdout.write(`${outputPath}\n`);
  await new Promise(() => {});
}

const target = await canvasTarget();
const connection = new CdpConnection(target.webSocketDebuggerUrl);
await connection.open();

if (mode === "snapshot") {
  await snapshot(connection);
  connection.socket.close();
}
else if (mode === "monitor") await monitor(connection);
else throw new Error(`未知模式：${mode}`);
