# lidup 项目说明

## 项目用途

lidup 是 macOS 菜单栏程序。用户选定一台外接显示器后，程序在该显示器连接时关闭 MacBook 内建屏，在它断开时恢复内建屏。菜单还支持手动切换显示器及管理登录自启；`list`、`recover`、`selftest` 和 `autostart` 是命令行子命令。

## 代码地图

- `src/main.rs`：菜单、事件循环、后台 worker 和自动规则的调度。
- `src/displays.rs`：CoreGraphics 显示器枚举和重配置回调；通过运行时加载的私有 SkyLight `CGSConfigureDisplayEnabled` 开关显示器。
- `src/auto.rs`：选定外接屏与内建屏之间的自动规则及其单元测试。
- `src/config.rs`：配置的 JSON 读写。默认位置为 `~/Library/Application Support/lidup/config.json`，可用 `LIDUP_CONFIG` 覆盖。
- `src/launch.rs`：通过 `SMAppService` 管理登录自启。
- `src/bin/autotest.rs`：会真实开关内建屏的硬件自检。
- `pack.sh`：构建、签名并打包菜单栏 `.app`；`.github/workflows/build.yml` 在发布标签上运行打包。

## 开发约束

- 显示器状态在睡眠、唤醒和插拔期间会短暂变化。`CGDisplayRegisterReconfigurationCallback` 的 begin 通知发生在配置完成前，只应在完成通知后读取最终显示器状态，并合并短时间内重复的回调。
- `CGDisplayIsAsleep` 与 `CGDisplayIsActive` 含义不同。内建屏睡眠时不要调用显示配置 API；外接屏短暂离线时不要立即恢复内建屏。额外的显示配置可能重置外接屏的 HDR 设置。
- 自动模式只管理内建屏；手动切换显示器后关闭自动模式。关屏操作必须保留至少一块可用显示器。关闭程序时恢复内建屏。
- 被软件关闭的内建屏可能不在 `CGGetOnlineDisplayList` 结果中；使用配置中缓存的显示器 ID 恢复。恢复操作会调用私有 API，应避免无条件或重复执行。
- 对纯决策逻辑运行 `cargo test`，并用 `cargo fmt --check` 检查格式。`src/bin/autotest.rs` 和实际睡眠唤醒验证会操作本机显示器，不应当作普通自动测试运行。
- 修改应用入口或系统 API 后，在 macOS 上运行 `cargo build`；打包使用 `./pack.sh`。裸二进制不能可靠注册登录项，测试菜单程序应运行 `.app`。
