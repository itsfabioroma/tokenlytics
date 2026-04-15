import { readFile, stat } from "fs/promises";
import { homedir } from "os";
import { Glob } from "bun";

const CLAUDE_DIR = `${homedir()}/.claude`;
const PROJECTS_DIR = `${CLAUDE_DIR}/projects`;
const STATS_CACHE = `${CLAUDE_DIR}/stats-cache.json`;

// per-file cache: only re-parse when mtime changes
const fileCache = new Map();

// Parse JSONL for today's data only (stats-cache covers past days)
async function parseJSONL(filePath) {
  const info = await stat(filePath).catch(() => null);
  if (!info) return [];
  const mtime = info.mtimeMs;
  const cached = fileCache.get(filePath);
  if (cached && cached.mtime === mtime) return cached.entries;

  const content = await readFile(filePath, "utf-8");
  const lines = content.split("\n").filter(Boolean);
  const entries = [];

  for (const line of lines) {
    try {
      const obj = JSON.parse(line);
      if (obj.type !== "assistant") continue;
      const usage = obj.message?.usage;
      if (!usage) continue;

      entries.push({
        timestamp: obj.timestamp,
        model: obj.message.model || "unknown",
        input: usage.input_tokens || 0,
        output: usage.output_tokens || 0,
        cacheRead: usage.cache_read_input_tokens || 0,
        cacheCreation: usage.cache_creation_input_tokens || 0,
        dedupKey: obj.message?.id && obj.requestId ? `${obj.message.id}:${obj.requestId}` : null,
      });
    } catch {}
  }

  fileCache.set(filePath, { mtime, entries });
  return entries;
}

// Read stats-cache — authoritative source, matches /usage exactly
async function readStatsCache() {
  const raw = await readFile(STATS_CACHE, "utf-8");
  return JSON.parse(raw);
}

// Get today's tokens from JSONL (unflushed data not in stats-cache yet)
async function getTodayFromJSONL(today) {
  const lastSeen = new Map();
  const glob = new Glob("**/*.jsonl");

  for await (const path of glob.scan(PROJECTS_DIR)) {
    const entries = await parseJSONL(`${PROJECTS_DIR}/${path}`);
    for (const entry of entries) {
      if (!entry.timestamp?.startsWith(today)) continue;
      if (entry.dedupKey) {
        lastSeen.set(entry.dedupKey, entry);
      } else {
        lastSeen.set(`nokey-${lastSeen.size}`, entry);
      }
    }
  }

  let tokens = 0;
  const sparkHours = new Array(24).fill(0);
  const now = new Date();

  for (const msg of lastSeen.values()) {
    const t = msg.input + msg.output;
    tokens += t;
    const hoursAgo = (now - new Date(msg.timestamp)) / (60 * 60 * 1000);
    if (hoursAgo >= 0 && hoursAgo < 24) {
      sparkHours[23 - Math.floor(hoursAgo)] += t;
    }
  }

  return { tokens, sparkHours };
}

export async function getUsageData() {
  const cache = await readStatsCache();
  const now = new Date();
  const today = now.toISOString().slice(0, 10);

  // cutoffs: /usage uses "last N days" exclusive, then adds today
  const day = (d) => new Date(now - d * 24 * 60 * 60 * 1000).toISOString().slice(0, 10);
  const cutoff7d = day(7);
  const cutoff30d = day(30);
  const cutoffPrev7d = day(14);
  const cutoffPrev30d = day(60);

  // aggregate from stats-cache dailyModelTokens (same source as /usage)
  const windows = { last24h: 0, last7d: 0, last30d: 0, allTime: 0, prev7d: 0, prev30d: 0, prev24h: 0 };
  const spark7d = new Array(7).fill(0);
  const spark30d = new Array(30).fill(0);
  const yesterday = day(1);

  for (const entry of cache.dailyModelTokens || []) {
    const d = entry.date;
    const tokens = Object.values(entry.tokensByModel).reduce((a, b) => a + b, 0);

    // time windows (exclusive cutoff: > not >=)
    if (d > cutoff7d) windows.last7d += tokens;
    if (d > cutoff30d) windows.last30d += tokens;
    if (d === yesterday) windows.prev24h += tokens;
    if (d > cutoffPrev7d && d <= cutoff7d) windows.prev7d += tokens;
    if (d > cutoffPrev30d && d <= cutoff30d) windows.prev30d += tokens;

    // sparkline buckets
    const daysAgo = Math.floor((now - new Date(d)) / (24 * 60 * 60 * 1000));
    if (daysAgo >= 0 && daysAgo < 7) spark7d[6 - daysAgo] += tokens;
    if (daysAgo >= 0 && daysAgo < 30) spark30d[29 - daysAgo] += tokens;
  }

  // today's live data from JSONL (not in stats-cache yet)
  const todayData = await getTodayFromJSONL(today);
  windows.last24h = todayData.tokens;
  windows.last7d += todayData.tokens;
  windows.last30d += todayData.tokens;
  spark7d[6] += todayData.tokens;
  spark30d[29] += todayData.tokens;

  // all-time from modelUsage + today
  for (const u of Object.values(cache.modelUsage || {})) {
    windows.allTime += (u.inputTokens || 0) + (u.outputTokens || 0);
  }
  windows.allTime += todayData.tokens;

  // model usage from stats-cache
  const modelUsage = {};
  for (const [model, u] of Object.entries(cache.modelUsage || {})) {
    modelUsage[model] = {
      input: u.inputTokens || 0,
      output: u.outputTokens || 0,
      cacheRead: u.cacheReadInputTokens || 0,
      cacheCreation: u.cacheCreationInputTokens || 0,
      messages: 0,
    };
  }

  // prev24h: use the day before yesterday from stats-cache
  // (already computed above from dailyModelTokens loop)

  const estimatedCost = estimateCost(modelUsage);
  const sparklines = { last24h: todayData.sparkHours, last7d: spark7d, last30d: spark30d };

  return { windows, sparklines, modelUsage, estimatedCost, lastUpdated: new Date().toISOString() };
}

// Estimate API cost
function estimateCost(modelUsage) {
  const pricing = {
    "claude-opus-4-6": { input: 15, output: 75, cacheRead: 1.5, cacheCreation: 18.75 },
    "claude-opus-4-5-20251101": { input: 15, output: 75, cacheRead: 1.5, cacheCreation: 18.75 },
    "claude-sonnet-4-5-20250929": { input: 3, output: 15, cacheRead: 0.3, cacheCreation: 3.75 },
    "claude-sonnet-4-6": { input: 3, output: 15, cacheRead: 0.3, cacheCreation: 3.75 },
    "claude-haiku-4-5-20251001": { input: 0.8, output: 4, cacheRead: 0.08, cacheCreation: 1 },
  };

  let total = 0;
  const breakdown = {};

  for (const [model, usage] of Object.entries(modelUsage)) {
    const priceKey = Object.keys(pricing).find((k) => model.includes(k) || k.includes(model));
    const price = pricing[priceKey] || pricing["claude-sonnet-4-5-20250929"];

    const cost =
      (usage.input / 1_000_000) * price.input +
      (usage.output / 1_000_000) * price.output +
      (usage.cacheRead / 1_000_000) * price.cacheRead +
      (usage.cacheCreation / 1_000_000) * price.cacheCreation;

    breakdown[model] = Math.round(cost * 100) / 100;
    total += cost;
  }

  return { total: Math.round(total * 100) / 100, breakdown };
}
