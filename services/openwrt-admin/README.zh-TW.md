# 獨立的 OpenWrt 管理

<!-- Translation: zh-TW | Source: services/openwrt-admin/README.md | Source SHA256: 52b173d36ce90518b04cd0947f9c6e38e79ea71df88c6a676b369d920bce2e76 -->

[English](README.md) | [简体中文](README.zh-CN.md) | 繁體中文 | [日本語](README.ja.md)

`rustlanbus-openwrt-admin` 是面向 RustLanBus 原生套件的短時本機管理命令。音訊/聯絡守護行程仍是獨立行程。停止該守護行程後，組態與管理入口仍然保留。

本應用程式擁有一份 UCI 檔案 `/etc/config/gw_app_rustlanbus`、既有的 procd 服務，以及自有的 GL Tools 頁/CGI。管理使用官方 GL 管理員工作階段。沒有額外的管理監聽器、帳戶、閘道控制器相依，也沒有私有的應用程式安裝平台。

部署適配器提供認證、跨行程原生鎖、請求長度限制與 CGI 逾時。本命令只接受 `status` 或 `request`；後者從標準輸入讀取有界 JSON。呼叫端在發出本機命令時，必須持有同一應用程式鎖。

## 契約

status 把組態與實際的原生服務、行程和就緒狀態分開報告。執行階段檔案必須與行程 PID、啟動 ticks、可執行檔及固定參數相符。未活動的服務仍可設定。密碼僅以 `password_set` 表示。

JSON 請求帶有 `action`：`start`、`stop`、`restart`、`enable`、`disable` 或 `save`。save 還需要最近一次 status 的 `revision` 以及一個 `config` 物件：`enabled`、`bind`、`return_bind`、`room_name`、`password_action`、`password`。`password_action` 明確保留、取代或清除已儲存的值。未知欄位會被拒絕。`return_bind` 為空或省略時，關閉選用的回傳主機。通訊端欄位使用明確 IP 與非零連接埠。

`enabled` 只是開機偏好。start/stop 不改變它；enable 登記開機啟動且不啟動服務，disable 停止服務並取消登記。save 時若只改開機偏好，不會停止暫時執行。儲存房間/密碼/繫結的變化時，僅重新啟動已經處於活動狀態的本應用程式。失敗的變更會嘗試還原原始組態位元組與執行階段意圖；外部檔案變更予以保留，不完整的還原會被報告。

## 原生生命週期與範圍

普通套件的 init 在自動啟動時遵循其開機偏好。管理器僅向明確的 start/restart 命令提供 `RUSTLANBUS_TEMPORARY_START=1`。受控的原生升級保留目前組態以及實際的執行階段/rc 意圖，以 `IPKG_NO_SCRIPT=1` 僅跳過 SDK 的 postinst，並還原已記錄的意圖。原生 prerm 仍可能停止本應用程式的舊行程。保留一份相容的先前 IPK 以及組態快照，供復原使用。

原生 GL 檢視、CGI、procd 配方與部署記錄屬於路由器整合倉儲。它們使用廠商既有的擴充點，無需重新啟用已退役的閘道控制器。介面與聽音測試由所有者操作。本管理適配器不改變音訊線路與目錄協定。
