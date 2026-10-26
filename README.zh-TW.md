# RustLanBus

> 原始碼狀態 — 2026-10-26：目前為開發候選。共享音訊與控制實作已經存在；實際啟動、介面、音訊與網路驗收分別記錄。參見[架構說明](docs/ARCHITECTURE.md)與[產品計畫](docs/PLAN.zh-TW.md)。

<!-- Translation: zh-TW | Source: README.md | Source SHA256: 0a885eb519e38a43fd5d14ce76229ee293d8c661b7652dcc04c9aa8a438c3965 -->

[English](README.md) | [简体中文](README.zh-CN.md) | 繁體中文 | [日本語](README.ja.md)

RustLanBus 在區域網路電腦之間共享原始音訊。主要用途是把多台裝置的聲音匯集到一台連接耳機或揚聲器的 Windows 電腦上。它保留擷取到的 PCM 精度，並盡量降低可避免的延遲。播放包含明確的時鐘校正、混音與揚聲器適配。

Windows 是第一個平台。桌面與正式 CLI 共用一個 Rust 控制器。2026-10-26 的原始碼增加了完整的卡片與設定管理、按需擷取、音源/輸出診斷，以及同時使用的 IPv4/IPv6 路徑。產品實作與所有者操作的測試分開記錄。更早的可攜式封存仍是歷史候選，不包含本批改動。參見[目前產品計畫](docs/PLAN.zh-TW.md)與[架構說明](docs/ARCHITECTURE.md)。

公開原始碼：[YzLfireChiYv/RustLanBus](https://github.com/YzLfireChiYv/RustLanBus)。公開歷史包含已審閱的快照。私密研究與部署記錄保持分開。SonoBus 是設計與程式碼參考。RustLanBus 使用自己的協定，與 SonoBus 不互通。

## 桌面與音訊設定

桌面使用 **Tauri 2、React 19 與 TypeScript**，由已安裝的 Microsoft Edge WebView2 執行環境在本機彩現。Rust 負責共享控制器、擷取、網路與播放。音訊不經過 JavaScript。

解壓可攜式 Windows 封存並執行 `RustLanBus.exe`。Windows 會要求系統管理員權限。視窗帶有工作列按鈕。**X** 隱藏視窗，背景音訊繼續。按一下系統匣圖示可恢復視窗，從其選單選擇 **退出** 可停止程式。視窗也有明確的結束操作。

選擇一個虛擬 **播放** 端點作為系統迴路音源，並在 Windows 中把各應用程式的聲音路由到該端點。選擇實體耳機或揚聲器作為 RustLanBus 的輸出。這些端點保持分開時，接收到的音訊留在原始擷取之外。虛擬端點來自另行安裝的社群驅動程式。RustLanBus 不提供自訂核心驅動程式。應用程式內指南會開啟官方專案頁面與 Windows 設定頁。請按[首次使用指南](docs/FIRST-USE.zh-CN.md)操作；若使用選用的 FxSound 虛擬裝置，其中包含「先退出再改路由」的順序。

在 Windows 上建立房間，或加入 Windows/OpenWrt 房間服務。該服務告知對等端位址。音訊在成員之間直接傳送。密碼防止誤入。聯絡服務不可用時，已經建立的直接音訊繼續。預設情況下，一個執行個體同時繫結 IPv4 與 IPv6，並透過普通握手探測候選位址對。健康的路徑保持被選中。進階設定可以嚴格限制網路介面或本機位址；被限制且不可用的網路會等待，不會悄悄改走其它路徑。

預設是 **向所有人傳送，不收聽任何人；系統聲音開，麥克風關；本機監聽關**。選擇要收聽的成員。收聽覆蓋該成員目前與之後的音源，展開的詳情中有逐個音源的控制項。多個音源可以共用一個輸出，每個音源也可以送往多個輸出。

全域 JSON 規則與按房間的 JSON 規則明確比對名稱、IP 或兩者。後比對規則中明確賦值的欄位優先。房間規則在全域規則之後執行。僅按名稱的控制會影響所有同名裝置。儲存失敗與決定性規則可見。檔案編輯在重新載入或下次進入房間時套用。探索不會填補儲存時省略的 IP。已儲存的卡片帶有編輯與刪除操作；刪除目前活動卡片前，先離開該卡片。

原生整數 PCM16/24/32 與 F32/F64 在傳輸中保留各自宣告的表示。工作緩衝區使用 F64。常見的單聲道/立體聲以及 5.1/7.1 佈局帶有面向揚聲器的對應。不受支援的目標按音源/輸出回報。沒有接收需求、也沒有本機監聽時，原生擷取會關閉，發佈意圖、P2P 聯絡與 keepalive 仍然保留。目前媒體協定：RLC5/RLP5；目錄協定：JSON version2，在同一 UDP 連接埠上攜帶有界的位址備選。更早的開發中對等端需要一併更新。

選用的登入啟動預設為關。明確的 Windows 操作可以註冊提高權限的登入工作，或允許本程式來自本地子網路的傳入 TCP/UDP 流量。這些操作不安裝音訊驅動程式，也不更改全域防火牆預設值。移動可執行檔後可能需要重新註冊；介面顯示實際註冊狀態，並提供修復與移除。

## 設定與復原

資料保存在 Windows 使用者目錄中。**應用程式與設定** 顯示目前組態以及 GUI/核心的建置識別，並支援開啟另一份組態、匯出一份可讀的 JSON 設定套件、預覽匯入並選擇匯入目標。匯出包含目前生效的設定（包括尚未儲存的值），不會改寫原檔。匯出內容含有房間密碼，應與私密備份一同保管。

選擇 **還原到這台電腦** 可保留身分與本機繫結；選擇 **複製到新裝置** 會建立新身分，並要求重新確認本機擷取/輸出裝置。複製模式會關閉自動進入、監聽與登入啟動。它保留符號性的 Main 路由，移除固定端點目標並把它們列出供重新指定；僅含固定目標的路由會保持為空。預覽會標明目標將被取代，以及保留的復原目錄。已損毀的卡片仍可在設定套件中以原始位元組形式取得。

關閉視窗只會隱藏視窗。明確結束時會列出尚未儲存的檔案，並可選擇返回或有意捨棄。匯入或切換組態時也會標出待處理改動。供人閱讀的 JSON 檔案仍可在程式外編輯；使用重新載入以套用這些編輯。

## 無介面 Windows 入口

CLI 與桌面使用同一個 Rust 控制器。一份設定對應一個正在執行的執行個體。從提高權限的終端機執行引擎。被重新導向的輸入/輸出保持不變。

```text
rustlanbus-engine participant --help
rustlanbus-engine participant init local.json --name Headphones --bind 0.0.0.0:0
rustlanbus-engine participant inspect local.json
rustlanbus-engine participant cards local.json
rustlanbus-engine participant run local.json --stdin --json
```

不帶 `--stdin` 時，Run 保持活動，直到中斷，或直到明確的 `--seconds N` 限制到達。帶 `--stdin` 時，EOF 會乾淨地停止。互動命令與命令 JSON 覆蓋房間、裝置、音源、規則、輸出、混音、網路選擇以及設定/卡片管理。`settings pending` 列出尚未儲存的檔案；`settings export PATH` 匯出目前生效的設定。離線的 `settings preview` 與 `settings import` 需要明確的模式、來源修訂與取代確認；詳見命令說明。正式入口會拒絕原始內部診斷命令。在房間之外使用 `name "Headphone PC"` 重新命名。已加入房間時，共享控制器會阻止重新命名。JSON 模式把帶標籤的 snapshot/result/error 記錄寫入 stdout。說明寫入 stderr。離線檢查不啟動音訊執行單元。

下面的互動規則範例收聽一名成員，並排除其麥克風：

```text
rule room --match name Music --listen on
rule room --match name Music --source microphone --listen off
```

輸出選擇獨立於接收。`main` 跟隨所選的主輸出。固定端點中斷連線後等待復原。輸出清單為空時，該範圍保留接收，並停用本機播放。

房間服務具備回傳能力時，`return start` 建立一名虛擬成員，返回這台電腦的原始音訊。`return stop` 移除該成員。Start 啟用並儲存所需的普通房間方向。時序報告把封包往返，以及從擷取到回傳接收，與實體聆聽延遲區分開。[延遲邊界](docs/LATENCY.zh-TW.md)說明原生週期、有界佇列、送達儲備與轉換；封包 RTT 不作為總延遲。

## 建置與原始碼佈局

實際的應用程式、介面、音訊、網路與測試執行由所有者操作。原始碼檢查與有針對性的建置檢查支援實作，其所確立的範圍是建置狀態。

```powershell
cargo check -p rustlanbus-engine -p rustlanbus-ui --offline
cd apps/desktop
npm ci
npm run build
```

`scripts/build-windows.ps1 -Build` 組裝一個新的可攜式 GUI/CLI 目錄，並附上用法、授權與雜湊。該指令碼不安裝驅動程式，不更改啟動或防火牆設定，也不發布原始碼。

- `crates/rlb-model/`：身分、PCM 精度與揚聲器位置。
- `crates/rlb-peer/`：對等協定與發布契約。
- `apps/engine/`：共享參與者控制器與 Windows 音訊執行環境。
- `apps/desktop/`：Tauri 外殼與 React 介面。
- `services/contactd/`：房間聯絡與選用的虛擬回傳支援。
- `crates/rlb-return/`：普通虛擬回傳參與者。
- `apps/peer-lab/` 與 `apps/audio-lab/`：明確的開發診斷。

授權條款為 [GNU GPL v3](LICENSE)，附加許可見 [LICENSE_EXCEPTION](LICENSE_EXCEPTION)。
