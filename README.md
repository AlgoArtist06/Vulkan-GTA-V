# GTA V

An unofficial native Android port using Vulkan. This repository is named **Vulkan-GTA-V**. The installed Android app is named **GTA V**.

**2026.10.19 is an experimental release.** Gameplay evidence is from a Samsung Galaxy S24. Other phones, Windows, and Linux have not received a full installation test. The desktop programs prepare and install the Android game over USB; they do not run the game on your computer.

## Downloads

Download and extract the package for your computer from [the release page](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/tag/v2026.10.19).

| Computer | Installer | Run after extraction |
| --- | --- | --- |
| Windows x64 | [Windows ZIP](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA-V-Installer-windows.zip) | GTA-V-Installer.exe |
| macOS 12 or later, Intel or Apple silicon | [Universal macOS ZIP](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA-V-Installer-macOS.zip) | GTA V Installer.app |
| Linux x64 with a desktop browser | [Linux tar.gz](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA-V-Installer-linux.tar.gz) | ./GTA-V-Installer |

The installer opens a local browser window, obtains this release's [GTA-V.apk](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA-V.apk) and [native menu patches](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA-V-native-patches.zip), checks the asset files, installs the APK, and transfers the required data through Android Debug Bridge (ADB). No Android SDK, compiler, or manual APK build is required by players.

The desktop downloads are unsigned by a commercial publisher. macOS uses an ad-hoc signature and is not notarized. See [installation instructions](INSTALL.md) for opening them and resolving USB issues.

## Assets

Game assets are **not committed to this repository**. Supply the matching **GTA5Webport.zip** (21,193,445,535 bytes). The original ZIP can be used directly; a second extracted 21 GB copy is unnecessary.

- [Download the torrent metadata](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA5Webport-base.torrent).
- Copy the direct magnet below into your torrent client's Add Magnet Link dialog, or [download the magnet as text](https://github.com/AlgoArtist06/Vulkan-GTA-V/releases/download/v2026.10.19/GTA5Webport-base.magnet.txt). GitHub removes clickable magnet links from rendered Markdown.

```text
magnet:?xt=urn:btih:57a4193cc3d3f069ce436fcda040ed4c705b76c7&dn=GTA5Webport.zip&tr=udp://tracker.opentrackr.org:1337/announce&tr=udp://open.stealth.si:80/announce&tr=udp://tracker.torrent.eu.org:451/announce&tr=udp://open.demonii.com:1337/announce&tr=udp://exodus.desync.com:6969/announce&tr=http://tracker2.dler.org:80/announce&tr=http://tracker.dler.com:6969/announce
```

**Availability:** a magnet link locates peers; it does not host files. This link's torrent metadata matches the local complete archive. Outside peers have reported a complete copy, but an independent data download was not verified. Downloading while the original uploader's Mac is off is therefore **not confirmed**. At least one reachable peer with the required pieces must remain online. Trackers and a magnet alone cannot guarantee that.

The installer applies the two matching native menu patches automatically. It verifies all **12,430 required files**, totalling **21,089,470,607 bytes**, against the release manifest. An extracted folder containing `data/` and `shaders/`, or the original nested mirror layout, is also accepted. Unused browser files are not installed.

Use only content you have the right to use and distribute. This project's source license does not grant rights to the game or its assets.

## Install

1. Keep at least **23 GB free** on a compatible Android phone. The APK requires **Android 12 or later, ARM64, and Vulkan**. The Galaxy S24 is the device used for saved gameplay measurements; these minimum checks do not guarantee compatibility with another GPU.
2. Enable Developer options and USB debugging, connect a data-capable USB cable, unlock the phone, and approve its USB debugging prompt.
3. Open the installer. Click **Set up USB tools** if needed, then **Refresh phones** and choose the intended device.
4. Paste the local path to GTA5Webport.zip or the matching asset folder. Click **Prepare and install**. Keep the phone connected until verification completes.
5. Click **Open GTA V on phone**.

The installer updates the existing package without uninstalling it or clearing saves. After an interrupted transfer, reconnect and repeat setup; files that already match are reused. See [INSTALL.md](INSTALL.md) for details and offline file placement.

## What is in this release

The Android payload is the final native memory build completed on 19 October 2026 at approximately 06:03 IST. Only the APK's app name, version metadata, ZIP alignment, and signature packaging were updated. All 59 non-manifest payload entries, including all eight native libraries, match that frozen APK.

Saved Galaxy S24 measurements at High settings and 75% rendering scale used three runs for each build. Combined PSS and SwapPSS fell from 5,859 MiB to 4,449 MiB (24.1%). These are recorded measurements for that workload, not a promise of performance on every device. See [CHANGELOG.md](CHANGELOG.md).

Release validation covered APK signature/metadata, unchanged game payload, every local asset checksum, the actual macOS interface, and installer failure/resume behavior in a simulated ADB environment. Windows and Linux packages were cross-compiled. A new end-to-end phone installation was not completed for these desktop installers.

The APK retains the development package identity `com.algoartist.gta5native.dev` and development signing/debug settings to preserve compatibility with the tested build. It is an experimental distribution, not an official Rockstar release.

## Source

Public source contains the Android shell, active native C host, Rust Vulkan renderer, platform/input services, and desktop installer. It excludes asset folders, generated game-engine translation, alternate architecture experiments, standalone diagnostics, tests, benchmark material, agent files, and private signing keys. Runtime compatibility handlers linked into the release remain part of the runtime source.

The production C host was matched to the frozen build's source hashes. Later diagnostic additions are excluded. Rust test-only blocks are omitted; runtime code is retained. The source tree is a production-code snapshot, not the original development workspace or its history.

The installer can be built with [Go 1.23 or later](https://go.dev/):

```sh
cd installer
go build -trimpath -o GTA-V-Installer .
```

Its interface, checksums, and release metadata are embedded in the executable. Native Android source requires Rust, Android SDK/NDK, matching generated engine input, native dependencies, and separately supplied artwork/assets. Those external inputs and the private build/experiment harness are not included, so this repository alone cannot reproduce a complete APK. Players should use the published APK through the installer.

Project-authored source is under [the MIT license](LICENSE). Third-party components retain their own licenses; see [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md).

GTA V and related names belong to their respective owners. This project is not affiliated with or endorsed by Rockstar Games or Take-Two Interactive.
