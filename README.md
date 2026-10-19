# GTA V

An unofficial native Android port of GTA V using Vulkan.

**Current release: 1.0.0.**
The game runs on the phone.
The desktop installers only prepare the files and install them on the phone over USB.

## Downloads

Download and extract the installer for your computer from [the 1.0.0 release page](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/tag/v2026.10.19).

| Computer | Installer | Run after extraction |
| --- | --- | --- |
| Windows x64 | [Windows ZIP](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA-V-Installer-windows.zip) | GTA-V-Installer.exe |
| macOS 12 or later, Intel or Apple silicon | [macOS ZIP](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA-V-Installer-macOS.zip) | GTA V Installer.app |
| Linux x64 with a desktop browser | [Linux tar.gz](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA-V-Installer-linux.tar.gz) | ./GTA-V-Installer |

The installer opens in your browser.
It downloads [GTA-V.apk](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA-V.apk) and the [native menu patches](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA-V-native-patches.zip), checks your asset files, installs the APK, and copies the game data to the phone through Android Debug Bridge (ADB).
You do not need the Android SDK or a compiler.

The installers are not signed with a paid certificate, and the macOS app is not notarized.
[INSTALL.md](INSTALL.md) explains how to open them.

## Assets

Game assets are **not included in this repository**.
You need **GTA5Webport.zip** (21,193,445,535 bytes).
The installer reads the ZIP directly, so you do not need to extract it.

- [Download the torrent file](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA5Webport-base.torrent).
- Or paste this magnet link into your torrent client ([also available as text](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA5Webport-base.magnet.txt)):

```text
magnet:?xt=urn:btih:57a4193cc3d3f069ce436fcda040ed4c705b76c7&dn=GTA5Webport.zip&tr=udp://tracker.opentrackr.org:1337/announce&tr=udp://open.stealth.si:80/announce&tr=udp://tracker.torrent.eu.org:451/announce&tr=udp://open.demonii.com:1337/announce&tr=udp://exodus.desync.com:6969/announce&tr=http://tracker2.dler.org:80/announce&tr=http://tracker.dler.com:6969/announce
```

The download only works while at least one seeder is online.
Please keep seeding after your download finishes.

The installer applies the native menu patches and checks all 12,430 required files (21,089,470,607 bytes) against the release manifest.
An extracted folder that contains `data/` and `shaders/` also works.

Use only content you have the right to use.
This project's license does not give you any rights to the game or its assets.

## Install

1. Use an Android 12 or later phone with ARM64 and Vulkan, and keep at least **23 GB free**.
2. Enable Developer options and USB debugging. Connect a USB data cable, unlock the phone, and accept the USB debugging prompt.
3. Open the installer. Click **Set up USB tools** if needed, then **Refresh phones**, and select your phone.
4. Paste the path to GTA5Webport.zip or the asset folder. Click **Prepare and install**. Keep the phone connected until the installer says the installation is verified.
5. Click **Open GTA V on phone**.

The installer updates the app without uninstalling it, so your saves are kept.
If a transfer stops, reconnect and run setup again; files that are already correct are not copied again.
See [INSTALL.md](INSTALL.md) for details and offline setup.

## Known limitations

- The game has only been tested on a Samsung Galaxy S24. Other phones and GPU drivers may have problems.
- The Windows and Linux installers have not been tested on real Windows or Linux machines yet.
- Some gameplay and graphics bugs remain.
- The APK uses a development package name (`com.algoartist.gta5native.dev`) and debug signing.

## Source

This repository contains the Android app, the native C host, the Rust Vulkan renderer, the input and platform services, and the desktop installer.
It does not contain game assets, the generated game-engine code, or the signing keys, so you cannot build a complete APK from this repository alone.
Players should use the release APK through the installer.

To build the installer, install [Go 1.23 or later](https://go.dev/) and run:

```sh
cd installer
go build -trimpath -o GTA-V-Installer .
```

## License

Project source is under [the MIT license](LICENSE).
Third-party components keep their own licenses; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

GTA V and related names belong to their owners.
This project is not affiliated with or endorsed by Rockstar Games or Take-Two Interactive.
