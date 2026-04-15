// Lightweight OTLP HTTP receiver for Claude Code telemetry
// Accepts metrics + events at http://localhost:4318

const OTEL_PORT = 4318;

// in-memory store for real-time OTel data
const otelStore = {
  // token usage from claude_code.token.usage metric
  tokens: { input: 0, output: 0, cacheRead: 0, cacheCreation: 0 },

  // cost from claude_code.cost.usage metric
  cost: 0,

  // per-model breakdown
  models: {},

  // recent events (ring buffer, last 100)
  events: [],

  // session tracking
  sessions: new Set(),

  // last updated
  lastUpdated: null,
};

// parse OTLP JSON metrics payload
function processMetrics(payload) {
  const resourceMetrics = payload.resourceMetrics || [];

  for (const rm of resourceMetrics) {
    const scopeMetrics = rm.scopeMetrics || [];

    for (const sm of scopeMetrics) {
      const metrics = sm.metrics || [];

      for (const metric of metrics) {
        const name = metric.name;

        // extract data points from sum or gauge
        const dataPoints = metric.sum?.dataPoints || metric.gauge?.dataPoints || [];

        for (const dp of dataPoints) {
          const value = dp.asDouble ?? dp.asInt ?? Number(dp.value) ?? 0;
          const attrs = parseAttributes(dp.attributes || []);

          if (name === "claude_code.token.usage") {
            const type = attrs.type || "unknown";
            const model = attrs.model || "unknown";

            // aggregate by type
            if (type === "input") otelStore.tokens.input += value;
            else if (type === "output") otelStore.tokens.output += value;
            else if (type === "cacheRead") otelStore.tokens.cacheRead += value;
            else if (type === "cacheCreation") otelStore.tokens.cacheCreation += value;

            // aggregate by model
            if (!otelStore.models[model]) {
              otelStore.models[model] = { input: 0, output: 0, cacheRead: 0, cacheCreation: 0, cost: 0 };
            }
            if (type === "input") otelStore.models[model].input += value;
            else if (type === "output") otelStore.models[model].output += value;
            else if (type === "cacheRead") otelStore.models[model].cacheRead += value;
            else if (type === "cacheCreation") otelStore.models[model].cacheCreation += value;
          }

          if (name === "claude_code.cost.usage") {
            otelStore.cost += value;
            const model = attrs.model || "unknown";
            if (!otelStore.models[model]) {
              otelStore.models[model] = { input: 0, output: 0, cacheRead: 0, cacheCreation: 0, cost: 0 };
            }
            otelStore.models[model].cost += value;
          }

          // track sessions
          if (attrs["session.id"]) otelStore.sessions.add(attrs["session.id"]);
        }
      }
    }
  }

  otelStore.lastUpdated = new Date().toISOString();
}

// parse OTLP JSON logs/events payload
function processLogs(payload) {
  const resourceLogs = payload.resourceLogs || [];

  for (const rl of resourceLogs) {
    const scopeLogs = rl.scopeLogs || [];

    for (const sl of scopeLogs) {
      const logRecords = sl.logRecords || [];

      for (const lr of logRecords) {
        const attrs = parseAttributes(lr.attributes || []);
        const event = {
          name: attrs["event.name"] || "unknown",
          timestamp: lr.timeUnixNano ? new Date(Number(lr.timeUnixNano) / 1_000_000).toISOString() : new Date().toISOString(),
          attributes: attrs,
        };

        // ring buffer — keep last 100 events
        otelStore.events.push(event);
        if (otelStore.events.length > 100) otelStore.events.shift();
      }
    }
  }

  otelStore.lastUpdated = new Date().toISOString();
}

// parse OTLP attribute array into key-value object
function parseAttributes(attrs) {
  const result = {};
  for (const attr of attrs) {
    const key = attr.key;
    const val = attr.value;
    if (!val) continue;
    result[key] = val.stringValue ?? val.intValue ?? val.doubleValue ?? val.boolValue ?? JSON.stringify(val);
  }
  return result;
}

// read request body
async function readBody(req) {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  return Buffer.concat(chunks).toString();
}

export function getOtelData() {
  return {
    tokens: { ...otelStore.tokens },
    cost: Math.round(otelStore.cost * 100) / 100,
    models: JSON.parse(JSON.stringify(otelStore.models)),
    recentEvents: [...otelStore.events],
    sessionCount: otelStore.sessions.size,
    lastUpdated: otelStore.lastUpdated,
  };
}

// start OTLP HTTP receiver
export function startOtelReceiver() {
  const server = Bun.serve({
    port: OTEL_PORT,
    async fetch(req) {
      const url = new URL(req.url);

      // OTLP metrics endpoint
      if (url.pathname === "/v1/metrics" && req.method === "POST") {
        try {
          const body = await req.json();
          processMetrics(body);
          return new Response("{}", { status: 200, headers: { "Content-Type": "application/json" } });
        } catch (err) {
          return new Response(JSON.stringify({ error: err.message }), { status: 400 });
        }
      }

      // OTLP logs/events endpoint
      if (url.pathname === "/v1/logs" && req.method === "POST") {
        try {
          const body = await req.json();
          processLogs(body);
          return new Response("{}", { status: 200, headers: { "Content-Type": "application/json" } });
        } catch (err) {
          return new Response(JSON.stringify({ error: err.message }), { status: 400 });
        }
      }

      return new Response("", { status: 404 });
    },
  });

  console.log(`OTel receiver at http://localhost:${server.port} (metrics + events)`);
  return server;
}
