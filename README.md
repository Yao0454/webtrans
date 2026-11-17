# Webtrans

一个 Rust 命令行 HTML 翻译工具，流程是：

```text
原始 HTML → 提取 JSON 翻译表 → OpenAI 兼容 API 分批翻译 → 校验 → 回填 HTML
```

JSON 的 `entries` 是以独立 ID 为键的映射，可直接拿来做前端表格。原文相同但位置不同的内容分别保留，避免同一句话在不同语境下被强制使用同一个译文。

## 终端日志

工具会在终端依次打印提取、发送 API、保存本批译文和回填的过程。提取后和翻译前会显示真实 JSON 翻译表结构；每批完成后显示该批条目的原文、译文、位置和占位符映射。

默认每次展示前 **5 条**，省略的数量会明确标注，保存到文件的 JSON 始终完整。添加 **`--verbose` / `-v`** 会打印全部条目。日志写入 stderr，不记录 API key 或 Authorization 请求头。

```bash
# 不需要 API key，即可直观看到完整哈希表。
cargo run -- extract examples/demo.html -o /tmp/strings.json --verbose

# 在终端查看全过程，同时保存为日志文件。
set -o pipefail
cargo run -- run examples/demo.html -o /tmp/demo.zh.html \
  --to zh-CN --verbose 2>&1 | tee translation.log
```

## 快速开始

需要支持 Rust 2024 edition 的 Rust / Cargo 工具链。

先填写项目根目录的 `config.json`。如果是新克隆的项目，可以复制 `config.example.json` 为 `config.json`：

```json
{
  "base_url": "https://your-provider.example/v1",
  "api_key": "你的 API key",
  "model": "服务商支持的模型名"
}
```

然后运行，不再需要导出环境变量或每次传入地址和模型：

```bash
cargo run -- run examples/demo.html -o /tmp/demo.zh.html --to zh-CN --verbose
```

结果包括：

- `/tmp/demo.zh.html`：翻译后的 HTML。
- `/tmp/demo.zh.translations.json`：可检查、修改和断点续译的翻译表。

## API 配置

`run` 和 `translate` 默认读取**当前工作目录**中的 `config.json`，也可以显式指定配置文件：

```bash
cargo run -- run examples/demo.html -o /tmp/demo.zh.html \
  --to zh-CN --config /path/to/provider.json
```

配置字段：

| 字段 | 说明 |
| --- | --- |
| `base_url` | API 基础路径，或完整的 `/chat/completions` 地址 |
| `api_key` | 直接填写服务商 API key |
| `model` | 服务商实际支持的模型名 |

`baseurl` 也可作为 `base_url` 的别名；`APIKEY`、`apikey`、`API_KEY` 也可作为 `api_key` 的别名。同一字段只使用一种写法。

取值优先级为 **命令行显式参数 > 配置文件 > 环境变量 > 默认值**：

- `--base-url`、`--model` 可临时覆盖文件中的对应设置。
- `--api-key-env MY_KEY` 显式使用指定环境变量中的密钥，覆盖文件中的 `api_key`。
- 配置未提供相应字段时，仍可使用 `WEBTRANS_BASE_URL`、`WEBTRANS_MODEL`、`OPENAI_API_KEY`；空字符串会报错并提示填写。
- 都没有设置地址时，默认基础路径为 `https://api.openai.com/v1`。
- 显式指定的配置文件缺失或内容无效时会报错；自动查找的 `config.json` 不存在则继续使用参数 / 环境变量。工具不自动加载 `.env` 文件。

根目录的 `config.json` 和 `config.*.json` 已加入 `.gitignore`，`config.example.json` 保留为可提交模板。运行日志只记录加载的配置文件路径，不输出密钥或配置文件内容。`extract`、`apply` 两个离线命令无需 API 配置。

## 分步使用

### 1. 提取

```bash
cargo run -- extract examples/demo.html -o /tmp/strings.json --from en
```

这一步不联网。表中每条记录包含：

| 字段 | 含义 |
| --- | --- |
| `kind` | 文本或属性 |
| `source` | 提供给翻译 API 的原文；内联标签和受保护变量用占位符表示 |
| `translation` | 译文；尚未翻译时为 `null` |
| `context` | 元素上下文，用于帮助翻译和人工检查 |
| `start` / `end` | 原始 UTF-8 HTML 中的字节位置 |
| `placeholders` | 占位符与原始 HTML / 变量的对应关系 |

顶层还包含 `version`、`source_sha256`、`source_language` 和 `target_language`。ID 按每次出现的位置分配；HTML 改变后需要重新提取，不能把旧表直接套上去。

### 2. 翻译表

```bash
cargo run -- translate /tmp/strings.json \
  -o /tmp/strings.zh.json \
  --to zh-CN
```

请求采用 OpenAI 兼容的 Chat Completions 格式。API 只收到条目 ID、原文、元素上下文、占位符名称和语言信息；`script`、`style` 的代码以及完整 HTML 不作为翻译内容发送。

模型需返回 `{"条目ID":"译文"}`。工具拒绝缺失、多余、重复的 ID，以及空译文、被截断的响应和被破坏的占位符，整批通过校验后才会保存。兼容接口支持 JSON mode 时可加 `--json-mode`；默认不发送 `response_format`，方便兼容不同服务商。

主要选项：

| 选项 | 默认值 | 说明 |
| --- | --- | --- |
| `--batch-size` | `24` | 每批最多条目数 |
| `--max-batch-chars` | `12000` | 每批原文、上下文和占位符等的字符预算，并非模型 token 上限 |
| `--timeout` | `90` | 单次请求超时秒数 |
| `--retries` | `2` | 暂时性网络问题或 HTTP 408 / 429 / 5xx 的额外尝试次数 |
| `--from` | 自动识别 | 源语言提示 |
| `--json-mode` | 关闭 | 显式请求 JSON object 响应 |

超大的单条内容会在发送前报错；可提高 `--max-batch-chars`。服务商响应被截断时，可降低每批大小。格式不正确的模型回答不会直接写入 HTML。

### 3. 回填

```bash
cargo run -- apply examples/demo.html /tmp/strings.zh.json -o /tmp/demo.zh.html
```

这一步不联网。可以先手工检查和修改表里的 `translation`，再执行回填；不要改 ID、原文、偏移量或占位符映射。用于续译的表还应保留正确的目标语言。

默认必须有全部译文。添加 `--allow-missing` 可以只回填已经翻译的条目，其余内容保持原文。

## 断点续译与文件保护

每批成功后都会原子保存翻译表。发生中断或后续批次失败时，保留已经完成的批次：

```bash
cargo run -- translate /tmp/strings.json \
  -o /tmp/strings.zh.json \
  --to zh-CN --resume
```

一键命令同样支持 `--resume`，用 `--table /path/checkpoint.json` 可以指定检查点路径。续译会核对原始提取表和语言，只请求尚未完成的条目。目标语言不匹配时会拒绝继续，避免混用译文。

默认拒绝覆盖已有输出；`--force` 显式允许替换输出，并从本次提供的源文件或翻译表重新开始。`--resume` 与 `--force` 不可同时使用。输出路径不能与任何输入路径相同；原 HTML 始终保留。提取出的表与原 HTML 的 SHA-256 绑定，源文件被编辑后必须重新提取。

## HTML 处理方式与边界

- 使用 HTML tokenizer 读取标签、文本和原始字节范围，不使用正则表达式直接拆 HTML。
- 按片段回填，不重新序列化整份 DOM。没有修改的区域保留原字节，包括 JS、CSS、注释、标签写法、属性顺序等。
- 同一文本片段内的内联标签会变成 `[[[WT_000001]]]` 形式的占位符，和整段文本一起翻译。占位符必须完整保留，顺序不能改变；这是第一版保护结构的限制。
- 译文作为普通文本转义后再写入；受保护的原始标签和变量按映射恢复，API 生成的标签不会作为新 HTML 执行。
- 常用文本属性会单独提取。带可翻译属性的内联标签可能将整句分为多个片段，以避免属性和正文的写入范围重叠。
- 属性范围包括 `title`、`alt`、`placeholder`、`aria-label`、`aria-description`、按钮类 `input` 的 `value` 和 `meta[name=description]` 的 `content`。
- 跳过 `script`、`style`、`pre`、`code`、`noscript`、`template`、`textarea`、SVG / MathML、iframe 等代码或特殊内容，以及 `translate="no"`、`class="notranslate"` 子树。
- 保护简单的 `{{name}}`、`{{{name}}}`、`${name}`、`{name}` 模板变量；不包含完整模板语言解析器。`[[[WT_` 是保留前缀，原 HTML 包含它时会明确报错。
- 静态翻译不会运行页面 JS，也不会翻译 JS 字符串、CSS `content` 或运行后动态生成的文字。动态页面应先从应用的国际化资源入手，或另加渲染后文本提取流程。
- 输入必须为 UTF-8。工具不更改 `<html lang>`、文字方向或排版；这些属于翻译后的页面适配。

常见的段落、列表项、选项和表格可省略结束标签已做处理，但不实现浏览器完整的 DOM 修复规则。具体范围以 `src/html.rs` 及其测试为准。对于复杂或不规范的 HTML，应先修复源文件并预览最终页面。

## 开发与验证

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

测试包含原样回填、内联标签、重复文本、JS / CSS 保留、属性转义、源文件校验、API 返回校验、断点续译等场景。API 测试使用本地模拟服务器，不需要真实密钥，也不会产生模型调用费用。
