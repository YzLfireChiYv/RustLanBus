# 独立的 OpenWrt 管理

<!-- Translation: zh-CN | Source: services/openwrt-admin/README.md | Source SHA256: 52b173d36ce90518b04cd0947f9c6e38e79ea71df88c6a676b369d920bce2e76 -->

[English](README.md) | 简体中文 | [繁體中文](README.zh-TW.md) | [日本語](README.ja.md)

`rustlanbus-openwrt-admin` 是面向 RustLanBus 原生软件包的短时本地管理命令。音频/联络守护进程仍是独立进程。停止该守护进程后，配置与管理入口仍然保留。

本应用拥有一份 UCI 文件 `/etc/config/gw_app_rustlanbus`、既有的 procd 服务，以及自有的 GL Tools 页/CGI。管理使用官方 GL 管理员会话。没有额外的管理监听器、账户、网关控制器依赖，也没有私有的应用安装平台。

部署适配器提供认证、跨进程原生锁、请求长度限制与 CGI 超时。本命令只接受 `status` 或 `request`；后者从标准输入读取有界 JSON。调用方在发出本地命令时，必须持有同一应用锁。

## 契约

status 把配置与实际的原生服务、进程和就绪状态分开报告。运行时文件必须与进程 PID、启动 ticks、可执行文件及固定参数相符。未活动的服务仍可配置。密码仅以 `password_set` 表示。

JSON 请求带有 `action`：`start`、`stop`、`restart`、`enable`、`disable` 或 `save`。save 还需要最近一次 status 的 `revision` 以及一个 `config` 对象：`enabled`、`bind`、`return_bind`、`room_name`、`password_action`、`password`。`password_action` 显式保留、替换或清除已保存的值。未知字段会被拒绝。`return_bind` 为空或省略时，关闭可选的回传主机。套接字字段使用显式 IP 与非零端口。

`enabled` 只是开机偏好。start/stop 不改变它；enable 登记开机启动且不启动服务，disable 停止服务并取消登记。save 时若只改开机偏好，不会停止临时运行。保存房间/密码/绑定的变化时，仅重启已经处于活动状态的本应用。失败的更改会尝试恢复原始配置字节与运行时意图；外部文件更改予以保留，不完整的恢复会被报告。

## 原生生命周期与范围

普通软件包的 init 在自动启动时遵循其开机偏好。管理器仅向显式的 start/restart 命令提供 `RUSTLANBUS_TEMPORARY_START=1`。受控的原生升级保留当前配置以及实际的运行/rc 意图，以 `IPKG_NO_SCRIPT=1` 仅跳过 SDK 的 postinst，并恢复已记录的意图。原生 prerm 仍可能停止本应用的旧进程。保留一份兼容的先前 IPK 以及配置快照，供恢复使用。

原生 GL 视图、CGI、procd 配方与部署记录属于路由器集成仓库。它们使用厂商既有的扩展点，无需重新启用已退役的网关控制器。界面与听音测试由所有者操作。本管理适配器不改变音频线路与目录协议。
