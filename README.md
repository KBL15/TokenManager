# TokenMonitor

监控千问/百炼 **Token Plan** 额度的 Windows 桌面小工具。

- 桌面**置顶悬浮卡片**：剩余百分比、进度条、重置倒计时、最后刷新时间
- **托盘图标直接画出百分比数字**，颜色随额度变化（绿/黄/红）
- 额度低于阈值弹系统通知（同一段区间只提醒一次）
- 开机自启、位置记忆、可锁定

技术栈：Tauri 2（Rust）+ 原生 HTML/JS（**无前端打包器**，无 npm 运行时依赖）。

---

## 快速开始

```bash
# 1. 装依赖并运行（开发模式）
npm install
npm run dev

# 2. 打包成安装包
npm run build
# 产物：src-tauri/target/release/bundle/nsis/*.exe
```

首次运行后，**右键托盘图标 → 设置**，然后按你的套餐类型配置数据源。

---

## 数据源怎么选（重要）

Token Plan 的用量**没有统一的公开接口**，个人版和团队版走的是完全不同的两条路。
设置页有「自动」模式会先试 AccessKey、失败再退 Cookie，不确定就选它。

| | 个人版 | 团队版 |
|---|---|---|
| 用哪条路 | **Cookie 网关** | **AccessKey 接口** |
| 需要什么 | 从浏览器复制的 Cookie | RAM 用户的 AccessKey ID/Secret |
| 会过期吗 | **会**，几周后需重新粘贴 | 不会 |
| 接口性质 | 控制台内部接口（未公开） | 官方 OpenAPI（`modelstudio` 2026-02-10） |

### 个人版：配置 Cookie

1. 打开百炼控制台「订阅套餐」页面
2. 按 `F12` → **Network** 面板
3. 在筛选框输入 `tokenplan/personal/api/v2/usage`
4. 找到这条请求 → 右键 → **Copy as cURL**
5. 回到设置页，把整段 cURL 粘进「粘贴 cURL」框，点**解析并填入**
6. 点**保存并刷新**

> 不用手动挑 Cookie 字段——**整行复制**即可。手动挑字段是「登录态失效」的常见原因。
>
> Cookie 过期后卡片会明确提示「登录态已失效」，重新走一遍上面 5 步即可。

### 团队版：配置 AccessKey

1. 在 RAM 控制台创建一个 RAM 用户
2. 授予系统策略 **`AliyunTokenPlanReadOnlyAccess`**（只读）
3. 生成 AccessKey，把 ID 和 Secret 填进设置页

> 建议用 RAM 子账号而不是主账号 AccessKey。
> AccessKey Secret **只显示一次**，请当场保存。
>
> 注意：团队版接口返回的是**席位制**额度（`SeatCredits` / `SeatRemainingCredits`）。
> 个人版账号调用会返回「系统异常」——这是接口本身的限制，不是配置问题。

### 接口改版了怎么办

内置实现对未公开接口的容错有限。若某天取不到数：

- 设置页切到**自定义接口**模式，直接填 URL / 认证方式 / 字段路径
- 路径写法是 `a.b[0].c`；接口返回 0–1 小数时把「取值倍率」填 `100`

---

## 凭据安全

- Cookie / AccessKey Secret 存在 **Windows 凭据管理器**（服务名 `com.tokenmoniter.app`）
- **不写进配置文件**；`%APPDATA%/tokenmoniter/config.json` 里只有非敏感项
- 落盘前会经过 `AppConfig::stripped()` 剥离，有单元测试守着这条

---

## 代码结构

```
src/                        前端（原生 HTML/JS/CSS，无打包器）
  card.html / card.js         悬浮卡片
  settings.html / settings.js 设置页（含 cURL 解析）
src-tauri/src/
  lib.rs                      状态、Tauri 命令、调度器
  config.rs                   配置模型与持久化（含敏感字段剥离）
  secret.rs                   凭据 → Windows 凭据管理器
  icon.rs                     托盘图标的位图绘制（内置 3×5 点阵数字字体）
  tray.rs                     托盘菜单与更新
  notify.rs                   额度提醒状态机
  usage/
    mod.rs                    UsageSnapshot 统一模型
    provider.rs               数据源分派与自动回退
    bailian.rs                个人版：控制台网关
    oneconsole.rs             OneConsole 网关协议
    modelstudio.rs            团队版：官方 OpenAPI
    aliyun_sign.rs            阿里云 V2 签名（HMAC-SHA1）
    aliyun_v3.rs              阿里云 V3 签名（ACS3-HMAC-SHA256）
    generic.rs                自定义 HTTP + 字段映射
    path.rs                   字段路径解析
    error.rs                  错误分类（决定用户该做什么）
```

## 测试

```bash
cd src-tauri
cargo test
```

覆盖了几处容易写错、又不容易发现的地方：

- **阿里云 V2 签名**：用官方文档给的固定测试向量对拍（期望 `9NaGiOspFP5UPcwX8Iwt2YJXXuk=`）
- **V3 签名**：断言输出是**小写 hex**（V2 才是 base64）、空 body 哈希为固定常量、规范请求串里头与 SignedHeaders 之间**有空行**
- **字段路径解析**：越界、类型不符、字符串形式的数字（阿里云系接口习惯用字符串返回数值）
- **托盘图标**：四角必须透明、数字不越界、不同额度产出不同位图
- **提醒状态机**：同一区间只提醒一次，回升后才允许二次提醒

## 签名实现的坑（改代码前务必先读）

`usage/aliyun_sign.rs` 里有详细注释，这里列最要命的几条：

1. **二次编码**：StringToSign 里对已编码的查询串**再编码一次**，`%3A` → `%253A`。这是 `SignatureDoesNotMatch` 的头号原因
2. **`~` 不能编码**（Java 的 `URLEncoder` 会错编成 `%7E`）
3. **空格用 `%20` 不是 `+`**
4. **V3 的密钥不带尾部 `&`**，输出是小写 hex 而非 base64，且 **body 参与签名**
5. **V3 的规范请求串里头与 SignedHeaders 之间有一个空行**，这是规范要求不是笔误

## 已知限制

- 只支持 Windows（凭据存储与托盘行为都按 Windows 写的）
- 个人版依赖未公开的控制台接口，**上游随时可能变**；团队版走官方 OpenAPI 更稳
- 团队版是**席位制**接口，个人版账号不适用