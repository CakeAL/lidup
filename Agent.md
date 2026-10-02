# lidup 项目说明

## 项目用途

lidup 是 macOS 菜单栏程序。用户选定一台外接显示器后，程序在该显示器连接时关闭 MacBook 内建屏，在它断开时恢复内建屏。菜单还支持手动切换显示器及管理登录自启。程序没有命令行模式。

## 代码地图

- `src/main.rs`：菜单、事件循环、后台 worker 和自动规则的调度。
- `src/displays.rs`：CoreGraphics 显示器枚举和重配置回调；通过运行时加载的私有 SkyLight `CGSConfigureDisplayEnabled` 开关显示器。
- `src/lid_angle.rs`：通过 IOKit HID 读取 MacBook 开合角，并对阈值切换进行防抖。
- `src/hdr.rs`：读取绑定外接屏的 HDR 偏好，并在唤醒后按睡前状态恢复；通过运行时加载私有 MonitorPanel 框架实现。
- `src/diagnostics.rs`：记录关键电源事件与显示器配置调用到 `~/Library/Logs/lidup.log`，便于与 WindowServer 日志对照。
- `src/auto.rs`：选定外接屏与内建屏之间的自动规则及其单元测试。
- `src/config.rs`：配置的 JSON 读写。默认位置为 `~/Library/Application Support/lidup/config.json`，可用 `LIDUP_CONFIG` 覆盖。
- `src/launch.rs`：通过 `SMAppService` 管理登录自启。
- `src/updates.rs`：使用 `self_update` 检查 GitHub 最新正式 Release、下载并替换当前 `.app`。
- `assets/app-icon.svg`：应用图标的矢量源文件；`assets/app-icon.icns` 是预生成的 macOS 图标资源。
- `pack.sh`：构建、签名并打包菜单栏 `.app`，包括应用图标；`.github/workflows/build.yml` 在发布标签上运行打包。

## 开发约束

- 显示器状态在睡眠、唤醒和插拔期间会短暂变化。`CGDisplayRegisterReconfigurationCallback` 的 begin 通知发生在配置完成前，只应在完成通知后读取最终显示器状态，并合并短时间内重复的回调。
- `CGDisplayIsAsleep` 与 `CGDisplayIsActive` 含义不同。内建屏睡眠时不要调用显示配置 API；外接屏短暂离线时不要立即恢复内建屏。额外的显示配置可能重置外接屏的 HDR 设置。
- 通过 `NSWorkspace` 的整机和屏幕睡眠/唤醒通知暂停唤醒后的自动关屏；唤醒后 CoreGraphics 可能暂时把内建屏报告为已开启，不得因此再次调用 `CGSConfigureDisplayEnabled`。绑定外接屏确实持续缺席超过唤醒保护期后才能重新启用自动关屏；断线恢复内建屏仍按五秒确认执行。
- WindowServer 也可能在 lidup 没有调用显示配置 API 时将唤醒后的外接屏重建为 SDR。只对睡前明确开启 HDR 的绑定外接屏执行恢复；收到屏幕唤醒通知后尽早检查，通知缺失时采用延迟检查，且重复通知不得重置恢复计时。避免覆盖用户持续关闭 HDR 的选择。MonitorPanel 私有接口不可用时跳过 HDR 恢复，不影响内建屏插拔逻辑。
- MonitorPanel 读取 HDR 状态会枚举显示器模式；只在显示器事件、切屏前、睡眠前及唤醒恢复期间读取，稳定时不轮询 HDR。显示器连接状态的两秒检查独立保留，用于未发出回调的拔线恢复。出现 HDR 异常时对照 `lidup.log` 的电源、切屏及 HDR 记录定位来源。
- 自动模式只管理内建屏；手动切换显示器后关闭自动模式。关屏操作必须保留至少一块可用显示器。关闭程序时恢复内建屏。
- 角度阈值仅在绑定外接屏在线且可用时生效；高于阈值点亮内建屏，低于阈值关闭内建屏。传感器读数缺失时保持当前状态，拔线恢复仍独立执行。睡眠期间不读传感器，唤醒后等待显示配置稳定。角度切换前保留外接屏 HDR 状态。
- 被软件关闭的内建屏可能不在 `CGGetOnlineDisplayList` 结果中；使用配置中缓存的显示器 ID 恢复。恢复操作会调用私有 API，应避免无条件或重复执行。
- 内建屏关闭时必须从显示布局中移除；只关背光不满足用户需求。唤醒后是否首帧即为 HDR 需要真实睡眠/唤醒验证，不能仅凭恢复调用成功推断。
- 排查 HDR 时区分“仅显示器睡眠”和整机进入 Deep Idle：2026-09-28 的短时显示器休眠直接沿用 `RGB_PQ_10bit`；2026-09-29 的整机长睡眠后 WindowServer 报告外接屏首选模式暂不可用，先输出 SDR，再由 lidup 恢复 HDR。时间长短本身不是已证实的触发条件；应对照 `pmset -g log` 与 WindowServer 的显示模式日志。
- 更新检查和安装必须在独立线程执行，不能阻塞菜单事件循环或显示器 worker。GitHub 请求有超时；网络或安装失败只影响更新状态。`self_update` 安装整个 `lidup.app`，从 Release 中选择 `lidup.zip` 并验证解压后的应用签名；更新完成后从菜单重启。应用版本来自 `CARGO_PKG_VERSION`；`pack.sh` 从 `Cargo.toml` 读取包版本写入 `Info.plist`。
- 对纯决策逻辑运行 `cargo test`，并用 `cargo fmt --check` 检查格式。实际睡眠唤醒验证会操作本机显示器，不应当作普通自动测试运行。
- 修改应用入口或系统 API 后，在 macOS 上运行 `cargo build`；打包使用 `./pack.sh`。裸二进制不能可靠注册登录项，测试菜单程序应运行 `.app`。
- 修改应用图标后运行 `./assets/make-icon.sh`，提交 SVG 源文件和生成的 ICNS；发布构建直接复制已提交的 ICNS，不依赖 CI 安装绘图工具。
