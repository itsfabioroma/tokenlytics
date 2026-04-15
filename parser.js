import { readFile, stat } from "fs/promises";
import { homedir } from "os";
import { Glob } from "bun";

const CLAUDE_DIR = `${homedir()}/.claude`;
const PROJECTS_DIR = `${CLAUDE_DIR}/projects`;
const STATS_CACHE = `${CLAUDE_DIR}/stats-cache.json`;

// per-file cache: only re-parse when mtime changes
const fileCache = new Map();

// Parse JSONL, return raw entries with dedup keys
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
        date: obj.timestamp?.slice(0, 10),
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

// Process all JSONL files with date filter + last-entry-wins dedup
// Matches Claude Code's processSessionFiles behavior
async function processMessages(fromDate, toDate) {
  const lastSeen = new Map();
  const glob = new Glob("**/*.jsonl");

  for await (const path of glob.scan(PROJECTS_DIR)) {
    const entries = await parseJSONL(`${PROJECTS_DIR}/${path}`);
    for (const entry of entries) {
      // date filtering
      if (fromDate && entry.date < fromDate) continue;
      if (toDate && entry.date > toDate) continue;

      // last-entry-wins dedup (streaming progress writes)
      if (entry.dedupKey) {
        lastSeen.set(entry.dedupKey, entry);
      } else {
        lastSeen.set(`nokey-${lastSeen.size}`, entry);
      }
    }
  }

  return [...lastSeen.values()];
}

// Aggregate messages into model usage + token totals
function aggregateMessages(messages) {
  const modelUsage = {};
  let totalInput = 0, totalOutput = 0;

  for (const msg of messages) {
    totalInput += msg.input;
    totalOutput += msg.output;

    if (!modelUsage[msg.model]) {
      modelUsage[msg.model] = { input: 0, output: 0, cacheRead: 0, cacheCreation: 0, messages: 0 };
    }
    modelUsage[msg.model].input += msg.input;
    modelUsage[msg.model].output += msg.output;
    modelUsage[msg.model].cacheRead += msg.cacheRead;
    modelUsage[msg.model].cacheCreation += msg.cacheCreation;
    modelUsage[msg.model].messages += 1;
  }

  return { modelUsage, totalTokens: totalInput + totalOutput };
}

// Read stats-cache
async function readStatsCache() {
  const raw = await readFile(STATS_CACHE, "utf-8");
  return JSON.parse(raw);
}

// Calculate streaks from dailyActivity (matches Claude Code exactly)
function calculateStreaks(dailyActivity, hasActivityToday) {
  const activeDates = new Set(dailyActivity.map((d) => d.date));

  // include today if we have live JSONL activity
  if (hasActivityToday) {
    activeDates.add(new Date().toISOString().slice(0, 10));
  }

  if (!activeDates.size) return { currentStreak: 0, longestStreak: 0 };
  const today = new Date();
  today.setHours(0, 0, 0, 0);

  // current streak: walk backwards from today
  let currentStreak = 0;
  const check = new Date(today);
  while (true) {
    const dateStr = check.toISOString().slice(0, 10);
    if (!activeDates.has(dateStr)) break;
    currentStreak++;
    check.setDate(check.getDate() - 1);
  }

  // longest streak: scan sorted dates for consecutive days
  let longestStreak = 0;
  const sorted = [...activeDates].sort();
  let tempStreak = 1;

  for (let i = 1; i < sorted.length; i++) {
    const prev = new Date(sorted[i - 1]);
    const curr = new Date(sorted[i]);
    const diff = Math.round((curr - prev) / (24 * 60 * 60 * 1000));
    if (diff === 1) {
      tempStreak++;
    } else {
      longestStreak = Math.max(longestStreak, tempStreak);
      tempStreak = 1;
    }
  }
  longestStreak = Math.max(longestStreak, tempStreak);

  return { currentStreak, longestStreak };
}

// Format duration
function formatDuration(ms) {
  const days = Math.floor(ms / (24 * 60 * 60 * 1000));
  const hours = Math.floor((ms % (24 * 60 * 60 * 1000)) / (60 * 60 * 1000));
  const mins = Math.floor((ms % (60 * 60 * 1000)) / (60 * 1000));
  const parts = [];
  if (days) parts.push(`${days}d`);
  if (hours) parts.push(`${hours}h`);
  parts.push(`${mins}m`);
  return parts.join(" ");
}

export async function getUsageData() {
  const cache = await readStatsCache();
  const now = new Date();
  const today = now.toISOString().slice(0, 10);

  // date ranges matching /usage: today - (N-1) through today inclusive
  const dayStr = (d) => new Date(now - d * 24 * 60 * 60 * 1000).toISOString().slice(0, 10);
  const from7d = dayStr(6);
  const from30d = dayStr(29);

  // for 7d/30d: scan JSONL directly (matches /usage behavior for ranged queries)
  const [msgs7d, msgs30d, msgsToday] = await Promise.all([
    processMessages(from7d, null),
    processMessages(from30d, null),
    processMessages(today, today),
  ]);

  const agg7d = aggregateMessages(msgs7d);
  const agg30d = aggregateMessages(msgs30d);
  const aggToday = aggregateMessages(msgsToday);

  // all-time: cache modelUsage + today's live (matches /usage "all" mode)
  let allTimeTokens = 0;
  const allTimeModelUsage = {};
  for (const [model, u] of Object.entries(cache.modelUsage || {})) {
    const tokens = (u.inputTokens || 0) + (u.outputTokens || 0);
    allTimeTokens += tokens;
    allTimeModelUsage[model] = {
      input: u.inputTokens || 0,
      output: u.outputTokens || 0,
      cacheRead: u.cacheReadInputTokens || 0,
      cacheCreation: u.cacheCreationInputTokens || 0,
      messages: 0,
    };
  }
  allTimeTokens += aggToday.totalTokens;

  // previous periods for trend comparison
  const prevFrom7d = dayStr(13);
  const prevTo7d = dayStr(7);
  const prevFrom30d = dayStr(59);
  const prevTo30d = dayStr(30);
  const yesterday = dayStr(1);

  const [msgsPrev7d, msgsPrev30d, msgsYesterday] = await Promise.all([
    processMessages(prevFrom7d, prevTo7d),
    processMessages(prevFrom30d, prevTo30d),
    processMessages(yesterday, yesterday),
  ]);

  const windows = {
    last24h: aggToday.totalTokens,
    last7d: agg7d.totalTokens,
    last30d: agg30d.totalTokens,
    allTime: allTimeTokens,
    prev24h: aggregateMessages(msgsYesterday).totalTokens,
    prev7d: aggregateMessages(msgsPrev7d).totalTokens,
    prev30d: aggregateMessages(msgsPrev30d).totalTokens,
  };

  // sparklines from dailyModelTokens + today
  const spark7d = new Array(7).fill(0);
  const spark30d = new Array(30).fill(0);
  const spark24h = new Array(24).fill(0);

  for (const entry of cache.dailyModelTokens || []) {
    const tokens = Object.values(entry.tokensByModel).reduce((a, b) => a + b, 0);
    const daysAgo = Math.floor((now - new Date(entry.date)) / (24 * 60 * 60 * 1000));
    if (daysAgo >= 0 && daysAgo < 7) spark7d[6 - daysAgo] += tokens;
    if (daysAgo >= 0 && daysAgo < 30) spark30d[29 - daysAgo] += tokens;
  }

  // today's hourly sparkline
  for (const msg of msgsToday) {
    const hoursAgo = (now - new Date(msg.timestamp)) / (60 * 60 * 1000);
    if (hoursAgo >= 0 && hoursAgo < 24) {
      spark24h[23 - Math.floor(hoursAgo)] += msg.input + msg.output;
    }
  }
  spark7d[6] += aggToday.totalTokens;
  spark30d[29] += aggToday.totalTokens;

  // streaks from dailyActivity
  const streaks = calculateStreaks(cache.dailyActivity || [], aggToday.totalTokens > 0);

  // activity heatmap: dailyActivity array (date + messageCount)
  const heatmap = (cache.dailyActivity || []).map((d) => ({
    date: d.date,
    messages: d.messageCount,
    sessions: d.sessionCount,
  }));

  // session stats from cache
  const sessionStats = {
    totalSessions: cache.totalSessions || 0,
    totalMessages: cache.totalMessages || 0,
    longestSession: cache.longestSession ? formatDuration(cache.longestSession.duration) : null,
    firstSessionDate: cache.firstSessionDate?.slice(0, 10) || null,
    activeDays: (cache.dailyActivity || []).length,
  };

  // peak stats
  const peakDay = (cache.dailyActivity || []).reduce(
    (max, d) => (d.messageCount > (max?.messageCount || 0) ? d : max), null
  );
  const peakHour = Object.entries(cache.hourCounts || {}).reduce(
    (max, [h, c]) => (c > max[1] ? [h, c] : max), ["0", 0]
  );

  // favorite model
  const favoriteModel = Object.entries(allTimeModelUsage)
    .sort(([, a], [, b]) => (b.input + b.output) - (a.input + a.output))[0]?.[0]
    ?.replace("claude-", "").replace(/-\d{8}$/, "") || null;

  const estimatedCost = estimateCost(allTimeModelUsage);

  return {
    windows,
    sparklines: { last24h: spark24h, last7d: spark7d, last30d: spark30d },
    modelUsage: allTimeModelUsage,
    estimatedCost,
    streaks,
    heatmap,
    sessionStats,
    favoriteModel,
    peakDay: peakDay?.date || null,
    peakHour: parseInt(peakHour[0]),
    lastUpdated: new Date().toISOString(),
  };
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
