# 参与贡献

## 先提 Issue，再写代码

改动超过 20 行、或涉及协议/安全相关的部分，请先开 Issue 讨论。否则可能白写——不是因为不欢迎，是因为方向不一致时合并成本更高。

## 提交 Pull Request

1. Fork 本仓库，从 `main` 开分支
2. 一个 PR 只做一件事，标题写清"改了什么、为什么"
3. 保证能编译通过（见 README 的构建章节）
4. 不要提交任何凭据：`.env.local`、`*.jks`、`*.pem`、`keystore.properties`

## 不需要签 CLA

本项目使用 Apache-2.0，其第 5 条已包含贡献授权条款：你提交 PR 即表示同意按同一许可证授权你的贡献。所以你**不需要**签署任何额外的贡献者协议。

但请在 PR 里确认：这段代码是你自己写的，或你有权按 Apache-2.0 提交。

## 代码风格

| 部分 | 要求 |
|---|---|
| Dart | 跟 `flutter-shell/` 现有风格，`flutter analyze` 无 error |
| Rust | `cargo fmt` + `cargo clippy` 无 warning |
| TypeScript | 跟现有 ESLint 配置 |
| 注释 | 用中文，写"为什么"而不是"做了什么" |

## 不要做的事

- 不要把 `data-v-xxxxxxxx` 这类构建期哈希写进 CSS 选择器（不同构建不一样，会"本地好线上坏"）
- 不要硬编码服务地址、TURN 凭据、密钥——一律走环境变量
- 不要提交构建产物（`dist/`、`build/`、`*.apk`、`*.exe`）
- 不要提交编辑器生成的备份文件（`*.bak-*`）

## 关于上游

`web/` 目录衍生自 [BilldDesk](https://github.com/galaxy-s10/billd-desk)（MIT）。修改该目录时请保留 `web/LICENSE.txt` 与文件头部的原始版权声明，不要移除。
