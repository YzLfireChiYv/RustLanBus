# RustLanBus

> 源码状态 — 2026-10-26：当前为开发候选。共享音频与控制实现已经存在；实际启动、界面、音频与网络验收分别记录。参见[架构说明](docs/ARCHITECTURE.md)与[产品计划](docs/PLAN.zh-CN.md)。

<!-- Translation: zh-CN | Source: README.md | Source SHA256: 6023b12878252d15c469aaad70ffb2145c70df8fafe2ae36bebd0564647cf445 -->

[English](README.md) | 简体中文 | [繁體中文](README.zh-TW.md) | [日本語](README.ja.md)

RustLanBus 在局域网计算机之间共享原始音频。主要用途是把多台设备的声音汇集到一台连接耳机或扬声器的 Windows 计算机上。它保留采集到的 PCM 精度，并尽量降低可避免的延迟。播放包含明确的时钟校正、混音与扬声器适配。

Windows 是第一个平台。桌面与正式 CLI 共用一个 Rust 控制器。2026-10-26 的源码增加了完整的卡片与设置管理、按需采集、音源/输出诊断，以及同时使用的 IPv4/IPv6 路径。产品实现与所有者操作的测试分开记录。更早的便携归档仍是历史候选，不包含本批改动。参见[当前产品计划](docs/PLAN.zh-CN.md)与[架构说明](docs/ARCHITECTURE.md)。

公开源代码：[YzLfireChiYv/RustLanBus](https://github.com/YzLfireChiYv/RustLanBus)。公开历史包含已审阅的快照。私密研究与部署记录保持分开。SonoBus 是设计与代码参考。RustLanBus 使用自己的协议，与 SonoBus 不互通。

## 桌面与音频设置

桌面使用 **Tauri 2、React 19 与 TypeScript**，由已安装的 Microsoft Edge WebView2 运行时在本地渲染。Rust 负责共享控制器、采集、网络与播放。音频不经过 JavaScript。

解压便携 Windows 归档并运行 `RustLanBus.exe`。Windows 会请求管理员权限。窗口带有任务栏按钮。**X** 隐藏窗口，后台音频继续。点击系统托盘图标可恢复窗口，从其菜单选择 **退出** 可停止程序。窗口也有明确的退出操作。

选择一个虚拟 **播放** 端点作为系统环回音源，并在 Windows 中把各应用程序的声音路由到该端点。选择物理耳机或扬声器作为 RustLanBus 的输出。这些端点保持分开时，接收到的音频留在原始采集之外。虚拟端点来自另行安装的社区驱动。RustLanBus 不提供定制内核驱动。应用内指南会打开官方项目页面与 Windows 设置页。请按[首次使用指南](docs/FIRST-USE.zh-CN.md)操作；若使用可选的 FxSound 虚拟设备，其中包含“先退出再改路由”的顺序。

在 Windows 上创建房间，或加入 Windows/OpenWrt 房间服务。该服务告知对等端地址。音频在成员之间直接传送。密码防止误入。联络服务不可用时，已经建立的直接音频继续。默认情况下，一个实例同时绑定 IPv4 与 IPv6，并通过普通握手探测候选地址对。健康的路径保持被选中。高级设置可以严格限制网卡或本地地址；被限制且不可用的网络会等待，不会悄悄改走其它路径。

默认是 **向所有人发送，不收听任何人；系统声音开，麦克风关；本机监听关**。选择要收听的成员。收听覆盖该成员当前与此后的音源，展开的详情中有逐个音源的控制。多个音源可以共用一个输出，每个音源也可以送往多个输出。

全局 JSON 规则与按房间的 JSON 规则显式匹配名称、IP 或两者。后匹配规则中明确赋值的字段优先。房间规则在全局规则之后运行。仅按名称的控制会影响所有同名设备。保存失败与决定性规则可见。文件编辑在重新载入或下次进入房间时应用。发现不会填补保存时省略的 IP。已保存的卡片带有编辑与删除操作；删除当前活动卡片前，先离开该卡片。

原生整数 PCM16/24/32 与 F32/F64 在传输中保留各自声明的表示。工作缓冲区使用 F64。常见的单声道/立体声以及 5.1/7.1 布局带有面向扬声器的映射。不受支持的目标按音源/输出报告。没有接收需求、也没有本机监听时，原生采集会关闭，发布意图、P2P 联络与 keepalive 仍然保留。当前媒体协议：RLC5/RLP5；目录协议：JSON version2，在同一 UDP 端口上携带有界的地址备选。更早的开发中对等端需要一并更新。

可选的登录启动默认为关。明确的 Windows 操作可以注册提升权限的登录任务，或允许本程序来自本地子网的入站 TCP/UDP 流量。这些操作不安装音频驱动，也不更改全局防火墙默认值。移动可执行文件后可能需要重新注册；界面显示实际注册状态，并提供修复与移除。

在 GL SDK4 OpenWrt 目标上，RustLanBus 拥有独立的原生软件包与 Tools 页，并使用官方管理员会话。其 CGI 与短时管理命令在联络守护进程停止时仍可工作。普通应用维护无需遗留的网关控制器，也不改动代理/网络后端；参见[OpenWrt 管理](services/openwrt-admin/README.zh-CN.md)。

## 设置与恢复

数据保存在 Windows 用户目录中。**应用程序与设置** 显示当前配置以及 GUI/核心的构建标识，并支持打开另一份配置、导出一份可读的 JSON 设置包、预览导入并选择导入目标。导出包含当前生效的设置（包括尚未保存的值），不会改写原文件。导出内容含有房间密码，应与私密备份一同保管。

选择 **还原到这台计算机** 可保留身份与本地绑定；选择 **复制到新设备** 会创建新身份，并要求重新确认本地采集/输出设备。复制模式会关闭自动进入、监听与登录启动。它保留符号性的 Main 路由，移除固定端点目标并把它们列出供重新指定；仅含固定目标的路由会保持为空。预览会标明目标将被替换，以及保留的恢复目录。已损坏的卡片仍可在设置包中以原始字节形式取得。

关闭窗口只会隐藏窗口。明确退出时会列出尚未保存的文件，并可选择返回或有意丢弃。导入或切换配置时也会标出待处理改动。人可读的 JSON 文件仍可在程序外编辑；使用重新载入以应用这些编辑。

## 无界面 Windows 入口

CLI 与桌面使用同一个 Rust 控制器。一份配置对应一个正在运行的实例。从提升权限的终端运行引擎。被重定向的输入/输出保持不变。

```text
rustlanbus-engine participant --help
rustlanbus-engine participant init local.json --name Headphones --bind 0.0.0.0:0
rustlanbus-engine participant inspect local.json
rustlanbus-engine participant cards local.json
rustlanbus-engine participant run local.json --stdin --json
```

不带 `--stdin` 时，Run 保持活动，直到中断，或直到明确的 `--seconds N` 限制到达。带 `--stdin` 时，EOF 会干净地停止。交互命令与命令 JSON 覆盖房间、设备、音源、规则、输出、混音、网络选择以及设置/卡片管理。`settings pending` 列出尚未保存的文件；`settings export PATH` 导出当前生效的设置。离线的 `settings preview` 与 `settings import` 需要明确的模式、源修订与替换确认；详见命令帮助。正式入口会拒绝原始内部诊断命令。在房间之外使用 `name "Headphone PC"` 重命名。已加入房间时，共享控制器会阻止重命名。JSON 模式把带标签的 snapshot/result/error 记录写入 stdout。帮助信息写入 stderr。离线检查不启动音频执行单元。

下面的交互规则示例收听一名成员，并排除其麦克风：

```text
rule room --match name Music --listen on
rule room --match name Music --source microphone --listen off
```

输出选择独立于接收。`main` 跟随所选的主输出。固定端点断开后等待恢复。输出列表为空时，该范围保留接收，并停用本地播放。

房间服务具备回传能力时，`return start` 创建一名虚拟成员，返回这台计算机的原始音频。`return stop` 移除该成员。Start 启用并保存所需的普通房间方向。时序报告把数据包往返，以及从采集到回传接收，与物理聆听延迟区分开。[延迟边界](docs/LATENCY.zh-CN.md)说明原生周期、有界队列、送达储备与转换；数据包 RTT 不作为总延迟。

## 构建与源码布局

实际的应用程序、界面、音频、网络与测试执行由所有者操作。源码检查与有针对性的构建检查支持实现，其确立的范围是构建状态。

```powershell
cargo check -p rustlanbus-engine -p rustlanbus-ui --offline
cd apps/desktop
npm ci
npm run build
```

`scripts/build-windows.ps1 -Build` 组装一个新的便携 GUI/CLI 目录，并附上用法、许可证与哈希。该脚本不安装驱动，不更改启动或防火墙设置，也不发布源代码。

- `crates/rlb-model/`：身份、PCM 精度与扬声器位置。
- `crates/rlb-peer/`：对等协议与发布契约。
- `apps/engine/`：共享参与者控制器与 Windows 音频运行时。
- `apps/desktop/`：Tauri 外壳与 React 界面。
- `services/contactd/`：房间联络与可选的虚拟回传支持。
- `crates/rlb-return/`：普通虚拟回传参与者。
- `apps/peer-lab/` 与 `apps/audio-lab/`：明确的开发诊断。

许可协议为 [GNU GPL v3](LICENSE)，附加许可见 [LICENSE_EXCEPTION](LICENSE_EXCEPTION)。
