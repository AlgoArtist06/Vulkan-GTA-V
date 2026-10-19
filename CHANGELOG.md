# Changelog

## 2026.10.19

- Published the native memory build completed around 06:03 IST on 19 October 2026, with the Android launcher name GTA V and version 2026.10.19.
- Preserved all native libraries, DEX, and other executable/asset payload entries from the frozen APK. No later game diagnostics were incorporated.
- Added separate Windows x64, universal macOS, and Linux x64 desktop setup packages. Setup reuses the fixed APK, applies two matching native menu patches, validates the supplied archive, and installs over USB with resume and final checksums.
- Published the active native port and installer source without assets, generated game-engine translation, experimental architecture trees, standalone diagnostics, tests, agent tools, signing keys, or development history.

### Saved memory comparison

Samsung Galaxy S24, High settings, 75% rendering scale; three recorded runs for each build:

| Measure | Before | Final |
| --- | ---: | ---: |
| PSS + SwapPSS | 5,859 MiB | 4,449 MiB |
| Anonymous memory + swap | 2,483 MiB | 1,808 MiB |
| Swap | 1,659 MiB | 887 MiB |
| Host malloc | 420 MiB | 230 MiB |
| Average FPS in the saved workload | 24.72 | 26.84 |

PSS apportions shared physical memory; SwapPSS accounts for apportioned swapped pages. Their sum is used here for the combined memory footprint. These counters describe different views and must not be added together again. These are saved workload results, not a new broad gameplay or compatibility claim.

### Known limits

Experimental release. Device evidence is from the Galaxy S24. Windows and Linux setup binaries have not been run on those operating systems, and a fresh full USB installation was not completed for the new installer. Gameplay and graphics issues may remain. The APK retains development signing/debug settings and its existing package identity. Torrent downloads require reachable peers; availability without the original uploader online remains unconfirmed.
