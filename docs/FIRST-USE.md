# Windows first use: original audio into RustLanBus

[简体中文](FIRST-USE.zh-CN.md) · [Application overview](../README.md)

This route uses a dedicated virtual **playback** endpoint for the original sound and an explicitly selected physical output for monitoring and received sound. RustLanBus uses shared-mode endpoint loopback, as described by [Microsoft](https://learn.microsoft.com/en-us/windows/win32/coreaudio/loopback-recording).

## Prepare the Windows desktop

Extract the portable Windows x64 program and open `RustLanBus.exe`. It requests administrator privileges. Create the local configuration or open an existing one. The desktop requires the [Microsoft Edge WebView2 Runtime](https://developer.microsoft.com/en-us/microsoft-edge/webview2/#download-section); that official page offers the Evergreen installer options.

## 1. Choose a virtual playback device

An existing suitable virtual audio device can be reused. In **声音接入指南**, the application lists render endpoints whose names suggest a virtual device. This is a naming hint; it does not select a device or establish that another program is not forwarding its sound.

FxSound is an optional candidate. Its [official project](https://github.com/fxsound2/fxsound-app) identifies the installer as the way to install its virtual audio driver. Use the [official download page](https://www.fxsound.com/download) and its matching Windows architecture package. Follow the upstream installer and any restart it requests.

For this sink-only setup, perform the following in order:

1. Disable FxSound automatic startup in Windows **Settings → Apps → Startup**, or Task Manager's startup-app list. These controls are documented by [Microsoft](https://support.microsoft.com/en-us/windows/configure-startup-applications-in-windows-115a420a-0bff-4a6f-90e0-1934c844e473).
2. Explicitly exit the FxSound application from its menu/tray. Keep its audio processing/forwarding application stopped for this route.
3. **After exiting**, route the desired Windows sound back to the installed virtual playback endpoint. The reviewed [FxSound shutdown source](https://github.com/fxsound2/fxsound-app/blob/37463e50a1ac3cda3baff8445a6070a37bf495e4/audiopassthru/src/AudioPassthru/AudioPassthruPrivate.cpp#L44) restores the default physical device, so choosing the virtual default before exit can be undone.
4. Refresh devices in RustLanBus and choose the resulting virtual render endpoint as described below.

Keep any external path that separately plays this source to physical speakers disabled, including a Windows “Listen to this device” setting where provided. Retain components required by the installed driver. RustLanBus's own **本机监听** controls its local original-sound branch.

## 2. Send application sound to the virtual endpoint

For selected applications, open Windows **Settings → System → Sound → Volume mixer** while the application is producing audio. Choose the virtual endpoint as that application's output. An application with its own device selection also needs the intended endpoint. [Microsoft's app-audio guide](https://support.microsoft.com/en-us/windows/hardware/audio/fix-app-audio-not-working-while-system-sounds-work-in-windows) describes per-app output selection.

On Windows10, the corresponding Sound page can be named **App volume and device preferences** under advanced sound options; Microsoft uses that name in its [Windows10 audio instructions](https://support.microsoft.com/en-us/surface/accessories/headphones/fix-problems-with-using-your-surface-headphones).

For sound that follows the system default, select the virtual endpoint as Windows's output device. Existing application-specific overrides still need attention. The general output selector is covered in [Microsoft's sound guide](https://support.microsoft.com/en-us/windows/hardware/audio/fix-sound-or-audio-problems-in-windows).

Keep the shared application and virtual endpoint unmuted. Routing separation provides a quiet sender; the selected output and local-monitor branch control local playback.

The in-app buttons open Windows Volume mixer and Sound settings in their own windows. They use the [documented settings URIs](https://learn.microsoft.com/en-us/windows/apps/develop/launch/launch-settings). External documentation opens in the system browser and preserves the application page.

## 3. Select the physical output first, then capture the virtual endpoint

In RustLanBus:

1. Expand **输出**. Under **主输出设备**, explicitly choose the physical headphones or speakers.
2. Expand **本机声音和设备**. In the system-sound card, set **采集设备** to the virtual **playback** endpoint.
3. Leave **发布** enabled when you want to offer this original source to members. Enable **本机监听** separately if this computer should also hear it.

This order matters: when Windows defaults to the virtual device, RustLanBus's **系统默认播放设备** choice follows that virtual device too. Select the physical endpoint explicitly before selecting the virtual capture source. The same-endpoint guard prevents received playback from returning through original capture.

System publication defaults on; microphone publication and both local monitors default off. To use a microphone, choose its recording endpoint in its separate source card and turn on its **发布** switch. When no member receives a source and local monitoring is off, acquisition can remain in demand-driven standby while contact and keepalive continue.

## 4. Join and listen

Create or join the same room. On the headphone computer, click **接听** for the desired member. This includes that member's current and later-enabled sources; expand it to close individual sources. Output routing and local monitoring remain independent of room selection.

Use the per-source and per-output status to distinguish idle, waiting, unavailable, muted and playing. The selected virtual route supplies silence at the sender's physical speakers when external forwarding is stopped; the local monitor switch controls only RustLanBus's branch.

## Format boundary

The acquired format and speaker layout are read from the selected endpoint. [GetMixFormat](https://learn.microsoft.com/en-us/windows/win32/api/audioclient/nf-audioclient-iaudioclient-getmixformat) describes this shared-audio-engine boundary. A particular virtual device's range does not impose a global48kHz/24-bit limit on RustLanBus. Windows or an application's processing before this boundary remains distinct from precision-preserving network transport.

## Reference scope

Official references were read on2026-10-26. No installer was downloaded, installed or run in preparing this guide. It makes no claim that the current package's signature, source-to-binary correspondence or physical audio behavior has been verified on this machine. FxSound remains a candidate rather than a mandatory dependency.
