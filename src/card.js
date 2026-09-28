// 悬浮卡片的界面逻辑。
//
// 不用打包器：通过 tauri.conf.json 的 withGlobalTauri 拿 window.__TAURI__。

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const $ = (id) => document.getElementById(id);

let current = null;
let locked = false;

// 可拖拽的元素。启动时记下来，锁定后要把它们的拖动属性摘掉。
// （只靠 CSS 拦不住拖动——Tauri 在原生 mousedown 里检查这个属性来决定要不要
// 调 startDragging，所以必须真的移除属性。）
let dragEls = [];

/** 按锁定状态增删拖动区域属性。 */
function syncDragRegion() {
  dragEls.forEach((el) => {
    if (locked) {
      el.removeAttribute("data-tauri-drag-region");
    } else {
      el.setAttribute("data-tauri-drag-region", "");
    }
  });
}

/** 剩余量决定进度条颜色，阈值与托盘图标保持一致。 */
function levelClass(pct) {
  if (pct === null || pct === undefined) return "";
  if (pct < 20) return "danger";
  if (pct < 50) return "warn";
  return "ok";
}

/**
 * 固定两位小数 + 千分位：12400 → "12,400.00"
 *
 * 刻意**不做万/亿缩写**——缩写会把小数位吃掉（12400 → "1.24万"），
 * 与"精确到小数点后两位"的要求直接冲突。
 */
function fmtAmount(n) {
  if (n === null || n === undefined || !isFinite(n)) return "—";
  return n.toLocaleString("zh-CN", {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

/** 距重置还有多久 */
function countdown(iso) {
  if (!iso) return "";
  const target = new Date(iso).getTime();
  if (!isFinite(target)) return "";

  let ms = target - Date.now();
  if (ms <= 0) return "即将重置";

  const mins = Math.floor(ms / 60000);
  const days = Math.floor(mins / 1440);
  const hours = Math.floor((mins % 1440) / 60);
  const rem = mins % 60;

  if (days > 0) return `${days} 天 ${hours} 小时后重置`;
  if (hours > 0) return `${hours} 小时 ${rem} 分后重置`;
  return `${rem} 分钟后重置`;
}

/** 距上次更新多久 */
function ago(iso) {
  if (!iso) return "";
  const t = new Date(iso).getTime();
  if (!isFinite(t)) return "";
  const secs = Math.max(0, Math.floor((Date.now() - t) / 1000));
  if (secs < 5) return "刚刚更新";
  if (secs < 60) return `${secs} 秒前更新`;
  const mins = Math.floor(secs / 60);
  if (mins < 60) return `${mins} 分钟前更新`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours} 小时前更新`;
  return `${Math.floor(hours / 24)} 天前更新`;
}

function renderWindow(w) {
  // 百分比优先用接口直接给的，否则由 used/total 推算
  let pct = w.remainingPct;
  if ((pct === null || pct === undefined) && w.used != null && w.total) {
    pct = ((w.total - w.used) / w.total) * 100;
  }

  const cls = levelClass(pct);
  const unit = current && current.unit ? current.unit : "";

  let amounts = "";
  if (w.used != null && w.total != null) {
    amounts = `剩余 ${fmtAmount(w.total - w.used)} / 共 ${fmtAmount(w.total)} ${unit}`;
  } else if (w.total != null) {
    amounts = `共 ${fmtAmount(w.total)} ${unit}`;
  } else {
    amounts = unit;
  }

  const showPct = pct === null || pct === undefined ? "—" : Math.round(pct);

  return `
    <div class="window-block">
      ${current.windows.length > 1 ? `<div class="window-label">${w.label}</div>` : ""}
      <div class="big-pct">${showPct}<span class="unit">${pct == null ? "" : "%"}</span></div>
      <div class="bar"><div class="bar-fill ${cls}" style="width:${pct == null ? 0 : Math.max(0, Math.min(100, pct))}%"></div></div>
      <div class="amounts"><span>${amounts}</span></div>
    </div>`;
}

// 上次设置过的高度。只在变化时才回写，避免每次刷新都触发窗口抖动。
let lastHeight = 0;

/**
 * 按内容实际高度调整窗口。
 *
 * 卡片是固定宽高的无边框窗口，内容一变长就会被裁掉——数字位数变多、出现错误条、
 * 或者团队版有多个额度窗口时都会发生。这里量出真实内容高度再回写给 Rust。
 */
async function fitWindow() {
  const card = $("card");
  const cs = getComputedStyle(card);
  const pad = parseFloat(cs.paddingTop) + parseFloat(cs.paddingBottom);
  const gap = parseFloat(cs.rowGap) || 0;

  const kids = Array.from(card.children).filter(
    (el) => getComputedStyle(el).display !== "none"
  );

  let h = pad + gap * Math.max(0, kids.length - 1);
  for (const k of kids) h += k.getBoundingClientRect().height;

  h = Math.ceil(h);
  if (Math.abs(h - lastHeight) < 2) return;
  lastHeight = h;

  try {
    await invoke("resize_card", { height: h });
  } catch (e) {
    console.error("调整卡片高度失败", e);
  }
}

function render(snap) {
  current = snap;

  $("title").textContent = snap.sourceLabel || "TokenMonitor";

  // 错误条：既可能是彻底失败，也可能是"有旧数据但这次没取到"
  const bar = $("error-bar");
  if (snap.error) {
    bar.classList.add("show");
    $("error-msg").textContent = snap.error;
  } else {
    bar.classList.remove("show");
  }

  const body = $("body");

  if (!snap.windows || snap.windows.length === 0) {
    body.innerHTML = `<div class="empty-hint">还没有数据。<br>点右上角 ⚙ 填入 Cookie 或 AccessKey。</div>`;
    $("reset").textContent = "";
    $("updated").textContent = "";
    fitWindow();
    return;
  }

  body.innerHTML = snap.windows.map(renderWindow).join("");

  // 有数据但正在报错 → 整体置灰，表示这是上一次的存量
  body.classList.toggle("stale", !!snap.error);

  const first = snap.windows[0];
  $("reset").textContent = countdown(first.resetAt);
  $("updated").textContent = ago(snap.fetchedAt);

  fitWindow();
}

async function refresh() {
  const btn = $("btn-refresh");
  btn.classList.add("spinning");
  try {
    const snap = await invoke("refresh_now");
    render(snap);
  } catch (e) {
    console.error("刷新失败", e);
  } finally {
    btn.classList.remove("spinning");
  }
}

function hideCtx() {
  $("ctx-menu").classList.remove("show");
}

function showCtx(x, y) {
  const m = $("ctx-menu");
  m.classList.add("show");
  // 贴边时往回收，别超出窗口
  const rect = m.getBoundingClientRect();
  m.style.left = Math.min(x, window.innerWidth - rect.width - 4) + "px";
  m.style.top = Math.min(y, window.innerHeight - rect.height - 4) + "px";
  $("ctx-lock").textContent = locked ? "解除锁定" : "锁定位置";
}

async function applyLock(next) {
  locked = next;
  $("card").classList.toggle("locked", locked);
  // 这一步才是真正"锁住"：不摘掉拖动属性的话，窗口照样能被拖走
  syncDragRegion();
  try {
    await invoke("set_widget_locked", { locked });
  } catch (e) {
    console.error("锁定设置失败", e);
  }
}

async function init() {
  // 初始数据
  try {
    render(await invoke("get_snapshot"));
  } catch (e) {
    console.error("读取状态失败", e);
  }

  // 记下所有可拖拽元素（之后按锁定状态增删属性）
  dragEls = Array.from(document.querySelectorAll("[data-tauri-drag-region]"));

  // 读一次锁定状态，恢复到上次的样子
  try {
    const cfg = await invoke("get_config");
    locked = !!(cfg.widget && cfg.widget.locked);
    $("card").classList.toggle("locked", locked);
    syncDragRegion();
  } catch (e) {
    /* 忽略 */
  }

  // 后台每次刷新都会广播，界面跟着更新
  await listen("snapshot", (ev) => render(ev.payload));

  $("btn-refresh").addEventListener("click", refresh);
  $("btn-settings").addEventListener("click", () => invoke("open_settings"));

  document.addEventListener("contextmenu", (e) => {
    e.preventDefault();
    showCtx(e.clientX, e.clientY);
  });

  document.addEventListener("click", (e) => {
    if (!e.target.closest(".ctx-menu")) hideCtx();
  });

  $("ctx-menu").addEventListener("click", async (e) => {
    const act = e.target.dataset.act;
    if (!act) return;
    hideCtx();
    switch (act) {
      case "refresh":
        refresh();
        break;
      case "lock":
        applyLock(!locked);
        break;
      case "settings":
        invoke("open_settings");
        break;
      case "hide":
        invoke("hide_card");
        break;
      case "quit":
        invoke("quit_app");
        break;
    }
  });

  // 让"多久前更新"和倒计时自己走起来
  setInterval(() => {
    if (!current || !current.windows || current.windows.length === 0) return;
    $("updated").textContent = ago(current.fetchedAt);
    $("reset").textContent = countdown(current.windows[0].resetAt);
  }, 10000);
}

init();