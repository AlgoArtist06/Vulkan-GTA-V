# Third-party notices

The MIT license at the repository root covers project-authored source only. Existing third-party copyrights and licenses remain in force. The accompanying third-party-licenses directory contains license and notice texts for the port's runtime dependencies and the desktop executable. Desktop packages include these notices; a separate license archive accompanies the APK release.

| Component | License | Notice location |
| --- | --- | --- |
| Go 1.23.4 runtime and standard library, used by the desktop installers | BSD-style Go license | third-party-licenses/Go/LICENSE |
| Rust standard library and its dependencies | MIT / Apache-2.0 and component-specific terms | third-party-licenses/Rust/COPYRIGHT-library.html and licenses/ |
| WABT wasm2c runtime support, included in native/direct-c/support/runtime | Apache-2.0 | third-party-licenses/WABT/LICENSE |
| DXVK 2.6.2 libraries retained in the frozen APK | Zlib | third-party-licenses/DXVK/LICENSE |
| libadrenotools | MIT | third-party-licenses/libadrenotools/LICENSE |
| liblinkernsbypass | MIT | third-party-licenses/liblinkernsbypass/LICENSE |

The published source is a selection of the active native port. It does not include third-party game-engine translation, game assets, or alternate architecture source trees. Their presence in an external input or precompiled APK does not place them under this repository's MIT license. GTA V names, artwork, music, and other content remain subject to their owners' rights. No affiliation or endorsement is implied.

Android Platform Tools is downloaded separately from Google when requested; it is not bundled in the desktop archive. Setup retains the downloaded package's NOTICE.txt. The Android SDK terms and the tool's own licenses apply.

## Rust package notices

The following package license metadata and accompanying notice texts were collected from the release dependency source cache. This includes build-time and conditional packages; a listed package is not necessarily linked into every binary. Missing platform-specific registry sources are not redistributed here. Cargo.lock records the dependency versions.

| Package | Version | Declared license |
| --- | --- | --- |
| anyhow | 1.0.104 | MIT OR Apache-2.0 |
| arrayvec | 0.7.8 | MIT OR Apache-2.0 |
| ash | 0.38.0+1.3.281 | MIT OR Apache-2.0 |
| autocfg | 1.5.1 | Apache-2.0 OR MIT |
| bit-set | 0.10.0 | Apache-2.0 OR MIT |
| bit-vec | 0.9.1 | Apache-2.0 OR MIT |
| bitflags | 2.13.2 | MIT OR Apache-2.0 |
| bytes | 1.12.1 | MIT |
| cesu8 | 1.1.0 | Apache-2.0/MIT |
| cfg_aliases | 0.2.2 | MIT |
| cfg-if | 1.0.5 | MIT OR Apache-2.0 |
| codespan-reporting | 0.13.1 | Apache-2.0 |
| combine | 4.6.8 | MIT |
| equivalent | 1.0.2 | Apache-2.0 OR MIT |
| foldhash | 0.2.0 | Zlib |
| gpu-allocator | 0.28.0 | MIT OR Apache-2.0 |
| half | 2.7.1 | MIT OR Apache-2.0 |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 |
| indexmap | 2.14.2 | Apache-2.0 OR MIT |
| itoa | 1.0.18 | MIT OR Apache-2.0 |
| jni | 0.21.1 | MIT/Apache-2.0 |
| jni-sys | 0.3.1 | MIT OR Apache-2.0 |
| jni-sys | 0.4.1 | MIT OR Apache-2.0 |
| jni-sys-macros | 0.4.1 | MIT OR Apache-2.0 |
| libm | 0.2.16 | MIT |
| log | 0.4.34 | MIT OR Apache-2.0 |
| memchr | 2.8.3 | Unlicense OR MIT |
| naga | 30.0.1 | MIT OR Apache-2.0 |
| naga-types | 30.0.1 | MIT OR Apache-2.0 |
| num-traits | 0.2.19 | MIT OR Apache-2.0 |
| once_cell | 1.21.4 | MIT OR Apache-2.0 |
| presser | 0.3.1 | MIT OR Apache-2.0 |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 |
| quote | 1.0.47 | MIT OR Apache-2.0 |
| rustc-hash | 1.1.0 | Apache-2.0/MIT |
| same-file | 1.0.6 | Unlicense/MIT |
| serde | 1.0.229 | MIT OR Apache-2.0 |
| serde_core | 1.0.229 | MIT OR Apache-2.0 |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 |
| serde_json | 1.0.151 | MIT OR Apache-2.0 |
| spirv | 0.4.0+sdk-1.4.341.0 | Apache-2.0 |
| syn | 2.0.119 | MIT OR Apache-2.0 |
| syn | 3.0.6 | MIT OR Apache-2.0 |
| thiserror | 1.0.69 | MIT OR Apache-2.0 |
| thiserror | 2.0.21 | MIT OR Apache-2.0 |
| thiserror-impl | 1.0.69 | MIT OR Apache-2.0 |
| thiserror-impl | 2.0.21 | MIT OR Apache-2.0 |
| unicode-ident | 1.0.26 | (MIT OR Apache-2.0) AND Unicode-3.0 |
| unicode-width | 0.2.2 | MIT OR Apache-2.0 |
| walkdir | 2.5.0 | Unlicense/MIT |
| zerocopy | 0.8.62 | BSD-2-Clause OR Apache-2.0 OR MIT |
| zerocopy-derive | 0.8.62 | BSD-2-Clause OR Apache-2.0 OR MIT |
| zmij | 1.0.23 | MIT |

Full texts are in the corresponding third-party-licenses/<package>-<version>/ directory. The spirv package declares Apache-2.0; its notice records the upstream project and author from its package metadata. Rust standard-library notices are preserved separately.

No license in this document grants permission to redistribute game assets or proprietary engine code. Users must obtain the necessary rights for separately supplied content.
