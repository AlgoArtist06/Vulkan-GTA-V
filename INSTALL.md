# Installing GTA V

## Before you start

You need:

- A Windows x64, macOS 12 or later (Intel or Apple silicon), or Linux x64 computer with a web browser.
- A USB data cable.
- An Android 12 or later phone with ARM64 and Vulkan, and at least 23 GB of free storage. The installer counts files that are already on the phone.
- GTA5Webport.zip, downloaded with the torrent or magnet link in the [README](README.md#assets). Do not extract it; the installer reads the ZIP directly.

The game has only been tested on a Samsung Galaxy S24.
There is no iOS or desktop version of the game.

## Open the installer

- **Windows:** extract GTA-V-Installer-windows.zip and double-click GTA-V-Installer.exe. Windows can show an "unknown publisher" warning because the program has no paid signing certificate. Click **More info**, then **Run anyway**. Some phones also need the manufacturer's Android USB driver.
- **macOS:** extract GTA-V-Installer-macOS.zip and open GTA V Installer.app. The app is not notarized. If macOS blocks it, try to open it once, then go to **System Settings > Privacy & Security** and click **Open Anyway** for this app. Do not turn off macOS security globally.
- **Linux:** extract the tar.gz and run `./GTA-V-Installer` from a terminal as your normal user. If needed, run `chmod +x GTA-V-Installer` first. If ADB reports "insufficient permissions", set up your distribution's Android udev rules.

The installer opens a page at 127.0.0.1, which is only on your own computer.
If the browser does not open, copy the address that the program prints.
The `--no-browser` option prints the address without opening a browser.
Click **Quit installer** when you are done.

## Connect your phone

1. On the phone, enable Developer options, then enable USB debugging.
2. Connect the USB cable, unlock the phone, and accept the USB debugging prompt. Some manufacturers also have a separate "install via USB" setting.
3. In the installer, click **Set up USB tools** if ADB is missing. This downloads Android Platform Tools from Google. You can also enter the path to your own ADB under **Optional local file paths**.
4. Click **Refresh phones** and select your phone. A phone that shows as unauthorized or offline cannot be used.

Installing ADB does not install Windows USB drivers or Linux USB permission rules.
You must set those up separately if your system needs them.

## Prepare and install

Paste the full path to GTA5Webport.zip in the assets field.
Paths in quotes, as copied from Windows Explorer, are accepted.
You can also use an extracted folder that contains `data/` and `shaders/`.

Click **Prepare and install**.
The installer downloads the APK and the native menu patches from this release and checks their SHA-256 checksums.
It applies the patches, checks every required asset file, and checks the phone's free storage.
Then it updates the app with `adb install -r` and copies the missing or changed files to:

```text
/sdcard/Android/obb/com.algoartist.gta5native.dev/data/
/sdcard/Android/obb/com.algoartist.gta5native.dev/shaders/
```

After the copy, the installer checks the checksum of every file on the phone.
Keep the phone connected until **Installation verified** appears.
Then click **Open GTA V on phone**.

## Offline setup and cache

The installer downloads GTA-V.apk and GTA-V-native-patches.zip from the v2026.10.19 GitHub release.
Their SHA-256 checksums are built into the installer.
Android Platform Tools comes from Google's current download, which is not pinned to one version.

To install without GitHub access, put the APK and the patches in the cache folder below before you start, and enter the path to an existing ADB.
You must already have the assets.

| OS | Default cache folder |
| --- | --- |
| Windows | %LocalAppData%/Vulkan-GTA-V/2026.10.19/ |
| macOS | ~/Library/Caches/Vulkan-GTA-V/2026.10.19/ |
| Linux | ~/.cache/Vulkan-GTA-V/2026.10.19/ (or $XDG_CACHE_HOME/Vulkan-GTA-V/2026.10.19/) |

The `--cache` option sets a different cache folder.
The installer checks cached files before it uses them.
It never uploads your assets or seeds the torrent.

## If setup stops

- **Asset checksum error or missing file:** check the download in your torrent client and make sure you selected the correct ZIP or folder. The installer stops before it changes anything on the phone.
- **USB disconnected:** reconnect, accept the debugging prompt, click **Refresh phones**, and try again. Files that are already correct are kept; damaged or incomplete files are copied again.
- **Not enough storage:** free some space without deleting your saves, then try again.
- **Signature mismatch:** another copy of the app with a different signature is installed. The installer stops and does not remove it. Back up your saves before you decide how to resolve it.
- **The game fails after setup:** open an issue with your phone model, Android version, and the error you see. Remove personal information first.

The installer never uninstalls the app or clears its data.
An update still replaces the app, so keep your own backup of important saves.
Do not delete the app's save folders to fix a transfer problem.
