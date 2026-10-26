# Windows 首次使用：把本机原声接入 RustLanBus

[English](FIRST-USE.md) · [中文使用说明](../README.zh-CN.md)

这条路线把需要共享的声音送入独立的虚拟**播放设备**。RustLanBus 对它进行共享模式回环采集，再把收到的声音和可选本机监听送到明确选择的物理耳机／音箱。[微软的回环说明](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording)描述了这个采集边界。

## 准备桌面程序

解压 Windows x64 便携包，运行 `RustLanBus.exe`，按既定方案授予管理员权限。建立本机配置，或打开已有配置。桌面需要[微软 Edge WebView2 运行时](https://developer.microsoft.com/en-us/microsoft-edge/webview2/#download-section)；官方页面提供 Evergreen 安装方式。

## 1. 准备独立的虚拟播放设备

已安装且适合这条路线的虚拟音频设备可以继续使用。“声音接入指南”会按名称列出可能的虚拟播放端点，供你定位。程序不会自动选中它；仅凭名称也无法判断外部程序是否还在转发声音。

如果尚无设备，可以考虑 FxSound。[官方项目](https://github.com/fxsound2/fxsound-app)说明其安装器会安装虚拟音频驱动。从[官方下载页面](https://www.fxsound.com/download)选择适合这台 Windows 架构的安装包，按上游向导安装；向导要求重启时先完成重启。

在本项目的纯虚拟音源路线中，顺序如下：

1. 到 Windows“设置 → 应用 → 启动”，或任务管理器的启动应用列表，关闭 FxSound 自动启动。[微软启动应用说明](https://support.microsoft.com/en-us/windows/configure-startup-applications-in-windows-115a420a-0bff-4a6f-90e0-1934c844e473)给出了这两个入口。
2. 从 FxSound 的菜单／托盘明确退出应用，让处理／转发应用保持停止。
3. **退出之后**，再把 Windows 的系统默认或指定应用输出设为已安装的虚拟播放端点。[已复核的 FxSound 退出源码](https://github.com/fxsound2/fxsound-app/blob/37463e50a1ac3cda3baff8445a6070a37bf495e4/audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp#L44)会恢复默认物理设备，因此先设虚拟默认、再退出，设置可能被改回。
4. 在 RustLanBus 刷新设备，按后面的顺序选择物理主输出和虚拟采集来源。

关闭会把这些来源另行播放到物理设备的外部监听链，包括设备属性中已有的“侦听此设备”。驱动需要的组件按其说明保留。本机原声的播放由 RustLanBus 的“本机监听”单独控制。

## 2. 在 Windows 中指定应用输出

只共享部分应用时，让目标应用开始播放，然后打开“设置 → 系统 → 声音 → 音量合成器”，在应用列表将它的输出设备改为虚拟播放端点。应用自身还有设备选择时，也要选择同一端点。[微软应用音频说明](https://support.microsoft.com/en-us/windows/hardware/audio/fix-app-audio-not-working-while-system-sounds-work-in-windows)介绍了逐应用输出选择。

Windows10 中对应入口可能称为高级声音选项下的“应用音量和设备首选项”，[微软 Windows10 音频说明](https://support.microsoft.com/en-us/surface/accessories/headphones/fix-problems-with-using-your-surface-headphones)也使用这个名称。

共享跟随系统默认的声音时，在 Windows“声音”中把输出设备设为虚拟播放端点。已经单独指定其它输出的应用需要另行确认。[微软声音说明](https://support.microsoft.com/en-us/windows/hardware/audio/fix-sound-or-audio-problems-in-windows)介绍了默认输出选择。

需要共享的应用和虚拟播放设备保持非静音。发送端物理设备上的原声安静依靠路由分离，本机听音由明确选择的输出和本机监听控制。

软件内的按钮会在 Windows 自己的窗口打开音量合成器和声音设置，使用[微软公布的设置 URI](https://learn.microsoft.com/en-us/windows/apps/develop/launch/launch-settings)。外部说明在系统浏览器打开，RustLanBus 页面保持在应用中。

## 3. 先选择物理主输出，再选择虚拟采集来源

在 RustLanBus 中依次操作：

1. 展开“输出”，在“主输出设备”明确选择物理耳机或音箱。
2. 展开“本机声音和设备”，在系统声音卡片的“采集设备”选择虚拟**播放端点**。
3. 需要向成员提供本机原声时保留“发布”；需要在本机听到它时，再单独开启“本机监听”。

顺序很重要：Windows 默认已经改为虚拟设备时，RustLanBus 的“系统默认播放设备”也会跟随它。因此这条路线要先明确选择物理输出，再选择虚拟采集来源。同端点保护会阻止把收到的播放再次送回原声采集。

首次默认系统声音发布开启，麦克风发布及两路本机监听关闭。要用麦克风，在独立麦克风卡片选择录音设备，再开启“发布”。没有成员接听且本机监听关闭时，采集可以按需待机，联系和保活继续维持。

## 4. 进入房间并选择接听

创建或加入同一房间。在连接耳机的电脑点击要听的成员“接听”，会接听该成员当前及以后开启的来源；展开后可以逐路关闭。输出路由、本机监听和房间选择各自独立。

通过逐来源、逐输出状态区分待机、等待、不可用、静音与播放。发送端物理扬声器保持原声静音，依靠声音进入虚拟端点且外部转发停止；本机监听开关只控制 RustLanBus 自己的播放支路。

## 格式边界

所选端点的真实采样率、有效精度和声道布局会在来源详情显示。[GetMixFormat 说明](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-getmixformat)描述了共享音频引擎提供的格式。某个虚拟设备的范围不会成为 RustLanBus 全局48kHz／24-bit上限。应用或 Windows 在采集边界之前的处理，与网络保持采集精度的传输分别看待。

## 资料边界

官方资料于2026-10-26查阅。编写这份说明期间没有下载安装器、安装设备或运行软件。本说明不把当前安装包的签名、源码与二进制对应关系、物理听音效果说成已经在本机验证。FxSound 是候选，已有合适设备也可继续使用。
