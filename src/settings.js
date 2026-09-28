// 设置页逻辑。
//
// 说明：tauri.conf.json 里的 withGlobalTauri 只暴露**核心** API，
// autostart 这类插件的 JS 包不在其中。为了让前端完全不依赖 npm 打包，
// 插件的功能统一由 Rust 侧的 command 暴露（见 lib.rs）。

const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);

const SECRET_KEYS = {
  cookie: "bailian-cookie",
  akSecret: "aliyun-accesskey-secret",
  generic: "generic-credential",
};

/** 三种网关预设。字段与 Rust 侧 OneConsoleCfg 一一对应。 */
const PRESETS = {
  bailian_cn: {
    gatewayHost: "bailian-cs.console.aliyun.com",
    pageHost: "bailian.console.aliyun.com",
    action: "BroadScopeAspnGateway",
    consoleSite: "BAILIAN_ALIYUN",
    region: "cn-beijing",
    language: "zh-CN",
  },
  bailian_intl: {
    gatewayHost: "bailian-singapore-cs.alibabacloud.com",
    pageHost: "modelstudio.console.alibabacloud.com",
    action: "IntlBroadScopeAspnGateway",
    // 这是官方历史拼写错误，照抄不要"修正"
    consoleSite: "MODELSTUDIO_ALBABACLOUD",
    region: "ap-southeast-1",
    language: "en-US",
  },
  qianwen: {
    gatewayHost: "cs-data.qianwenai.com",
    pageHost: "platform.qianwenai.com",
    action: "BroadScopeAspnGateway",
    consoleSite: "QIANWENAI",
    region: "cn-beijing",
    language: "zh-CN",
  },
};

let cfg = null;

function setStatus(text, kind) {
  const el = $("status");
  el.textContent = text || "";
  el.className = "status" + (kind ? " " + kind : "");
}

/* ───────────── cURL 解析 ───────────── */

/** 按 shell 规则切词，处理单/双引号与反斜杠续行。 */
function tokenizeCurl(raw) {
  // Windows 的 cmd 用 ^ 续行，bash 用 \，都处理掉
  const joined = raw
    .replace(/\^\s*\r?\n/g, " ")
    .replace(/\\\s*\r?\n/g, " ")
    .replace(/\r?\n/g, " ");

  const out = [];
  let cur = "";
  let quote = null;

  for (let i = 0; i < joined.length; i++) {
    const c = joined[i];
    if (quote) {
      if (c === quote) quote = null;
      else if (c === "\\" && quote === '"' && i + 1 < joined.length) cur += joined[++i];
      else cur += c;
    } else if (c === "'" || c === '"') {
      quote = c;
    } else if (/\s/.test(c)) {
      if (cur) out.push(cur);
      cur = "";
    } else {
      cur += c;
    }
  }
  if (cur) out.push(cur);
  return out;
}

/**
 * 从 cURL 命令里抽出 URL、方法、请求头、body。
 * 认得出 DevTools「Copy as cURL」的输出即可，不需要支持全部 curl 语法。
 */
function parseCurl(raw) {
  const toks = tokenizeCurl(raw);
  const res = { url: "", method: "", headers: [], body: "" };

  for (let i = 0; i < toks.length; i++) {
    const t = toks[i];
    const next = () => toks[++i];

    if (t === "-H" || t === "--header") {
      const h = next() || "";
      const idx = h.indexOf(":");
      if (idx > 0) res.headers.push([h.slice(0, idx).trim(), h.slice(idx + 1).trim()]);
    } else if (t === "-X" || t === "--request") {
      res.method = (next() || "").toUpperCase();
    } else if (t === "-d" || t === "--data" || t === "--data-raw" || t === "--data-binary") {
      res.body = next() || "";
      if (!res.method) res.method = "POST";
    } else if (t === "--url") {
      res.url = next() || "";
    } else if (t === "-b" || t === "--cookie") {
      res.headers.push(["cookie", next() || ""]);
    } else if (!t.startsWith("-") && !res.url && /^https?:\/\//i.test(t)) {
      res.url = t;
    }
    // 其余参数（--compressed / -s 等）忽略
  }

  if (!res.method) res.method = "GET";
  return res;
}

/* ───────────── 表单读写 ───────────── */

function fillForm() {
  document.querySelectorAll("#mode-row .radio-card").forEach((el) => {
    el.classList.toggle("active", el.dataset.mode === cfg.sourceMode);
  });

  const c = cfg.cookieSource || {};
  $("cookie-input").value = c.cookie || "";

  // 反查当前用的是哪个预设
  let preset = "bailian_cn";
  for (const [key, p] of Object.entries(PRESETS)) {
    if (p.gatewayHost === c.gatewayHost) preset = key;
  }
  $("cookie-preset").value = preset;

  $("ak-id").value = (cfg.accessKeySource && cfg.accessKeySource.accessKeyId) || "";

  const g = cfg.genericSource || {};
  $("g-url").value = g.url || "";
  $("g-method").value = g.method || "GET";
  $("g-auth").value = (g.auth && g.auth.type) || "none";
  if (g.auth && g.auth.name) $("g-auth-name").value = g.auth.name;
  $("g-headers").value = (g.extraHeaders || []).map(([k, v]) => `${k}: ${v}`).join("\n");

  const w = (g.windows && g.windows[0]) || {};
  $("g-pct").value = w.remainingPct || "";
  $("g-used").value = w.used || "";
  $("g-total").value = w.total || "";
  $("g-reset").value = w.resetAt || "";
  $("g-scale").value = w.scale != null ? w.scale : 1;

  $("interval").value = cfg.refreshIntervalSecs;
  $("threshold").value = cfg.alertThresholdPct;
  $("tray-pct").checked = !!cfg.trayShowsPercentage;
  $("autostart").checked = !!cfg.autostart;

  syncAuthExtra();
  syncSections();
}

function syncAuthExtra() {
  const t = $("g-auth").value;
  const show = t === "customHeader" || t === "queryParam";
  $("g-auth-extra").style.display = show ? "" : "none";
  $("g-auth-extra-label").textContent = t === "queryParam" ? "参数名" : "请求头名称";
}

function syncSections() {
  const mode = cfg.sourceMode;
  // 自动模式三条路都可能用到，所以都显示
  $("sec-cookie").style.display =
    mode === "auto" || mode === "cookie" ? "" : "none";
  $("sec-ak").style.display = mode === "auto" || mode === "accessKey" ? "" : "none";
  $("sec-generic").style.display = mode === "generic" ? "" : "none";
}

function readForm() {
  const out = JSON.parse(JSON.stringify(cfg));

  out.cookieSource = out.cookieSource || {};
  out.cookieSource.cookie = $("cookie-input").value;
  Object.assign(out.cookieSource, PRESETS[$("cookie-preset").value]);

  out.accessKeySource = out.accessKeySource || {};
  out.accessKeySource.accessKeyId = $("ak-id").value.trim();

  const authType = $("g-auth").value;
  const authName = $("g-auth-name").value.trim();
  let auth = { type: authType };
  if (authType === "customHeader") auth = { type: "customHeader", name: authName, prefix: "" };
  else if (authType === "queryParam") auth = { type: "queryParam", name: authName };

  const headers = $("g-headers")
    .value.split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => {
      const i = l.indexOf(":");
      return i > 0 ? [l.slice(0, i).trim(), l.slice(i + 1).trim()] : null;
    })
    .filter(Boolean);

  out.genericSource = {
    url: $("g-url").value.trim(),
    method: $("g-method").value,
    auth,
    extraHeaders: headers,
    bodyTemplate: (out.genericSource && out.genericSource.bodyTemplate) || null,
    windows: [
      {
        label: "本月",
        remainingPct: $("g-pct").value.trim() || null,
        used: $("g-used").value.trim() || null,
        total: $("g-total").value.trim() || null,
        resetAt: $("g-reset").value.trim() || null,
        scale: parseFloat($("g-scale").value) || 1,
      },
    ],
  };

  out.refreshIntervalSecs = parseInt($("interval").value, 10) || 60;
  out.alertThresholdPct = parseFloat($("threshold").value) || 20;
  out.trayShowsPercentage = $("tray-pct").checked;
  out.autostart = $("autostart").checked;

  return out;
}

/* ───────────── 凭据状态 ───────────── */

async function refreshSecretPills() {
  const s = await invoke("get_secret_status");
  const pill = (el, ok, on, off) => {
    el.textContent = ok ? on : off;
    el.className = "pill " + (ok ? "ok" : "off");
  };
  pill($("pill-cookie"), s.cookie, "已保存", "未配置");
  pill($("pill-ak"), s.akSecret, "已保存", "未配置");
  if (!s.storeAvailable) $("store-warn").style.display = "";
}

/* ───────────── 事件绑定 ───────────── */

function bind() {
  document.querySelectorAll("#mode-row .radio-card").forEach((el) => {
    el.addEventListener("click", () => {
      cfg.sourceMode = el.dataset.mode;
      document
        .querySelectorAll("#mode-row .radio-card")
        .forEach((x) => x.classList.toggle("active", x === el));
      syncSections();
    });
  });

  $("g-auth").addEventListener("change", syncAuthExtra);

  $("btn-parse-curl").addEventListener("click", () => {
    const raw = $("curl-input").value.trim();
    if (!raw) {
      setStatus("请先粘贴 cURL", "err");
      return;
    }
    const r = parseCurl(raw);
    if (!r.url && r.headers.length === 0) {
      setStatus("没解析出内容，确认粘贴的是完整 cURL", "err");
      return;
    }

    const cookie = r.headers.find(([k]) => k.toLowerCase() === "cookie");
    if (cookie) {
      $("cookie-input").value = cookie[1];
      setStatus(`已填入 Cookie（${cookie[1].length} 字符），记得点保存`, "ok");
    } else {
      setStatus("这条 cURL 里没有 Cookie，请复制带 Cookie 的那条请求", "err");
    }
  });

  $("btn-clear-cookie").addEventListener("click", async () => {
    await invoke("clear_secret", { key: SECRET_KEYS.cookie });
    $("cookie-input").value = "";
    await refreshSecretPills();
    setStatus("已清除 Cookie", "ok");
  });

  $("btn-refresh").addEventListener("click", async () => {
    setStatus("刷新中…");
    const snap = await invoke("refresh_now");
    if (snap.error) setStatus(snap.error, "err");
    else setStatus(`成功：${snap.sourceLabel}`, "ok");
  });

  $("btn-save").addEventListener("click", async () => {
    setStatus("保存中…");
    try {
      // 凭据单独存，不进配置文件。留空 = 不修改已保存的值
      const cookie = $("cookie-input").value.trim();
      if (cookie) await invoke("set_secret", { key: SECRET_KEYS.cookie, value: cookie });

      const akSecret = $("ak-secret").value.trim();
      if (akSecret) {
        await invoke("set_secret", { key: SECRET_KEYS.akSecret, value: akSecret });
        $("ak-secret").value = "";
      }

      const gCred = $("g-cred").value.trim();
      if (gCred) {
        await invoke("set_secret", { key: SECRET_KEYS.generic, value: gCred });
        $("g-cred").value = "";
      }

      const next = readForm();
      await invoke("save_config", { cfg: next });

      // 开机自启交给插件处理（通过 Rust 命令暴露）
      try {
        await invoke("set_autostart", { enabled: next.autostart });
      } catch (e) {
        console.warn("开机自启设置失败", e);
      }

      cfg = next;
      await refreshSecretPills();

      const snap = await invoke("get_snapshot");
      if (snap.error) setStatus(snap.error, "err");
      else setStatus(`已保存 · ${snap.sourceLabel}`, "ok");
    } catch (e) {
      setStatus(String(e), "err");
    }
  });
}

async function init() {
  cfg = await invoke("get_config");
  fillForm();
  bind();
  await refreshSecretPills();

  // 自启的真实状态以系统为准，不信配置文件
  try {
    $("autostart").checked = await invoke("get_autostart");
  } catch (e) {
    /* 拿不到就保持配置里的值 */
  }

  const snap = await invoke("get_snapshot");
  if (snap && snap.error) setStatus(snap.error, "err");
}

init();