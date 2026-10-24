# Matched runtime observations

Positive deltas mean full-minus-disabled, or full-minus-revision for fix candidates. Full-vs-main rows mean full-minus-main. Memory uses KiB; CPU and latency use percent. Brackets are within-runner bootstrap 95% intervals over 20 complete pairs. Sizes and every matched value are in [effects.csv](effects.csv). These intervals do not measure variation between runner hosts.

Decisions and their reasons are in the [mechanism comparisons](../resource-mechanisms.md#decisions). No control value is subtracted from a mechanism result.

| Mechanism | Target | Idle RSS, KiB | Driver CPU, % | Large JSON CPU, % | Cold reply, % | Attach p99, % | Decision |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| QoS scoping | macOS 15 Intel | -39.60 [-59.80, -23.60] | +3.20 [-3.87, +11.30] | +2.01 [-3.17, +6.85] | -9.97 [-24.18, -0.40] | -4.52 [-13.55, +5.91] | Keep |
| Pressure subscription | macOS 15 Intel | -6.40 [-21.80, +9.20] | -1.14 [-8.23, +7.53] | +4.19 [-2.42, +11.45] | -3.10 [-8.96, +2.36] | +8.40 [-0.57, +20.12] | Keep: measured cache release |
| Pressure subscription | Linux ARM64 | +3.20 [-1.20, +7.80] | -0.03 [-0.50, +0.42] | -2.45 [-2.93, -1.93] | +4.97 [+0.19, +11.17] | +0.41 [-0.97, +1.68] | Keep: measured cache release |
| Pressure subscription | Linux x64 | +11.80 [-23.00, +47.20] | -0.81 [-1.49, -0.12] | -0.41 [-0.90, +0.12] | -1.13 [-3.95, +1.71] | -2.34 [-4.15, -0.49] | Keep: measured cache release |
| Pressure subscription | Windows ARM64 | -14.00 [-19.80, -8.60] | -2.18 [-6.53, +2.48] | -1.38 [-4.34, +1.51] | +1.05 [-1.37, +4.12] | +7.72 [-2.29, +27.77] | Keep: measured cache release |
| Pressure subscription | Windows x64 | +1.20 [-22.40, +23.40] | +1.30 [-3.04, +5.50] | -0.51 [-2.39, +1.21] | -1.76 [-5.37, +1.16] | +0.49 [-1.99, +3.46] | Keep: measured cache release |
| Storage lookup | macOS 15 Intel | +56.00 [+43.00, +68.40] | -0.78 [-5.82, +4.41] | +0.54 [-4.20, +5.44] | -6.18 [-13.04, +1.44] | +0.50 [-18.72, +24.60] | Drop: no qualified slow-disk benefit |
| Storage lookup | Linux ARM64 | -12.60 [-18.60, -6.80] | -0.17 [-1.75, +1.23] | +0.16 [-0.61, +0.89] | -5.68 [-11.57, -0.76] | +0.52 [-0.71, +1.87] | Drop: no qualified slow-disk benefit |
| Storage lookup | Linux x64 | +22.20 [-3.00, +51.20] | +0.09 [-1.36, +1.74] | +0.13 [-1.37, +1.79] | -1.07 [-7.81, +4.15] | +16.75 [-0.88, +47.15] | Drop: no qualified slow-disk benefit |
| Storage lookup | Windows ARM64 | +18.20 [+10.00, +26.80] | +1.92 [-2.44, +6.68] | +0.97 [-2.51, +4.46] | -1.35 [-7.76, +4.51] | -0.01 [-1.78, +1.59] | Drop: no qualified slow-disk benefit |
| Storage lookup | Windows x64 | +55.40 [+31.60, +80.20] | -0.22 [-4.29, +3.84] | +3.37 [+0.65, +5.95] | +1.88 [+0.91, +2.95] | +0.65 [-2.64, +5.28] | Drop: no qualified slow-disk benefit |
| Worker sizing | macOS 15 Intel | -14.80 [-28.20, -0.60] | +0.14 [-1.44, +1.79] | +3.47 [+0.24, +7.06] | +3.06 [-1.59, +8.58] | +3.87 [-8.83, +18.97] | Drop: no width change, CPU above control |
| Worker sizing | Linux ARM64 | -16.00 [-21.60, -10.40] | -0.01 [-0.88, +1.18] | -0.63 [-0.91, -0.32] | -0.01 [-3.12, +2.95] | +1.02 [-1.04, +2.79] | Drop: no width change, CPU above control |
| Worker sizing | Linux x64 | +9.80 [-21.40, +40.00] | +3.14 [+2.22, +4.02] | -0.23 [-0.66, +0.27] | -5.53 [-23.04, +13.87] | -0.37 [-2.52, +1.46] | Drop: no width change, CPU above control |
| Worker sizing | Windows ARM64 | +4.60 [-1.60, +10.20] | +3.32 [-0.17, +7.11] | +0.80 [-1.54, +3.19] | +1.94 [-0.18, +4.15] | +3.89 [+0.34, +9.04] | Drop: no width change, CPU above control |
| Worker sizing | Windows x64 | +7.00 [-22.40, +35.00] | -2.03 [-4.93, +0.74] | +1.57 [-0.65, +3.85] | -7.04 [-19.82, +2.04] | +1.30 [-4.35, +10.52] | Drop: no width change, CPU above control |
| Effective CPU discovery | Windows ARM64 | +21.00 [+14.40, +28.20] | +3.49 [-1.41, +8.66] | -1.67 [-4.90, +1.48] | -1.58 [-3.39, +0.13] | +0.52 [-1.54, +2.71] | Keep: honours CPU sets and job caps at ~21 KiB |
| Effective CPU discovery | Windows x64 | +22.20 [+2.00, +43.80] | +0.37 [-2.50, +3.38] | +1.02 [-1.24, +3.37] | +1.66 [-0.73, +4.22] | -5.75 [-12.14, -0.86] | Keep: honours CPU sets and job caps at ~21 KiB |
| Oneshot | Linux ARM64 | +31.00 [+26.40, +36.00] | +0.40 [-0.50, +1.39] | -4.98 [-5.40, -4.57] | -1.53 [-5.16, +2.67] | -0.24 [-2.19, +1.17] | Drop: RSS and CPU regressions; fixed shared slot restored |
| Oneshot | Linux x64 | +46.20 [+3.00, +91.40] | +2.16 [+0.50, +4.09] | +1.40 [-0.27, +3.12] | +3.21 [-6.35, +20.28] | -6.32 [-23.37, +6.61] | Drop: RSS and CPU regressions; fixed shared slot restored |
| Oneshot | Windows ARM64 | +11.60 [+6.20, +16.60] | -1.40 [-5.81, +2.93] | +0.94 [-1.62, +3.43] | -1.45 [-5.93, +2.58] | -1.19 [-3.52, +0.95] | Drop: RSS and CPU regressions; fixed shared slot restored |
| Oneshot | Windows x64 | -1.20 [-19.60, +15.60] | -2.52 [-6.42, +1.53] | +3.16 [+1.24, +5.00] | +6.00 [-0.14, +15.23] | -6.66 [-18.42, +3.68] | Drop: RSS and CPU regressions; fixed shared slot restored |
| Catalog admission | Linux ARM64 | +12.80 [+7.40, +18.60] | -1.23 [-2.57, -0.10] | -1.61 [-2.92, -0.55] | +0.78 [-2.91, +5.03] | -1.37 [-5.69, +1.62] | Keep: shared storage limit at <=13 KiB |
| Catalog admission | Linux x64 | -1.00 [-36.40, +33.00] | -0.02 [-0.99, +0.99] | -0.51 [-1.54, +0.46] | -0.30 [-6.67, +5.33] | -2.47 [-6.89, +0.63] | Keep: shared storage limit at <=13 KiB |
| Catalog admission | Windows ARM64 | +7.80 [+2.80, +13.20] | -4.89 [-8.72, -1.11] | +1.18 [-1.26, +3.77] | +1.68 [-1.02, +4.26] | -1.20 [-4.87, +2.07] | Keep: shared storage limit at <=13 KiB |
| Catalog admission | Windows x64 | +9.80 [-4.40, +23.00] | +1.16 [-2.85, +5.21] | +0.80 [-0.41, +2.01] | +3.79 [-0.74, +10.78] | +2.79 [-0.89, +7.63] | Keep: shared storage limit at <=13 KiB |
| Compact oneshot revision | Linux ARM64 | +34.20 [+27.40, +41.00] | +2.02 [+0.50, +3.43] | -4.36 [-5.27, -3.31] | +7.08 [+0.54, +15.03] | +1.03 [-0.27, +2.61] | Drop |
| Compact oneshot revision | Linux x64 | +20.60 [-6.40, +54.60] | -0.55 [-1.33, +0.28] | +1.04 [+0.38, +1.80] | -0.69 [-2.46, +0.98] | +0.54 [-2.24, +4.05] | Drop |
| Compact oneshot revision | Windows ARM64 | +18.40 [+12.00, +24.60] | +0.83 [-2.06, +3.44] | -1.08 [-3.17, +0.94] | -0.38 [-2.94, +2.19] | +3.30 [-2.74, +13.97] | Drop |
| Compact oneshot revision | Windows x64 | +39.60 [+12.80, +65.80] | +1.98 [-5.63, +9.18] | -1.27 [-5.58, +3.22] | -2.77 [-6.17, +0.83] | -15.85 [-35.29, +3.36] | Drop |
| Generic pressure revision | Linux ARM64 | -7.20 [-12.60, -1.40] | +0.31 [-0.35, +0.99] | -2.46 [-3.15, -1.77] | +1.90 [-1.34, +4.52] | +1.89 [+0.16, +4.26] | Drop revision: CPU regression without RSS saving |
| Generic pressure revision | Linux x64 | -20.60 [-52.40, +16.40] | +0.91 [-0.49, +2.26] | -1.24 [-2.82, +0.00] | -4.11 [-10.88, +1.53] | -7.76 [-18.90, -0.72] | Drop revision: CPU regression without RSS saving |
| Generic pressure revision | Windows ARM64 | -8.60 [-15.00, -2.60] | -2.06 [-6.14, +2.20] | +0.47 [-2.37, +3.30] | +6.79 [-0.11, +14.49] | -2.52 [-10.26, +2.36] | Drop revision: CPU regression without RSS saving |
| Generic pressure revision | Windows x64 | +9.20 [-9.40, +27.80] | +2.80 [-0.85, +5.94] | +1.89 [-0.21, +3.96] | +1.65 [-0.31, +3.56] | -0.89 [-3.97, +1.61] | Drop revision: CPU regression without RSS saving |
| Independent pressure signal revision | Linux ARM64 | -2.60 [-7.60, +2.40] | +0.45 [-0.30, +1.34] | -4.61 [-5.22, -4.02] | +3.63 [-0.36, +8.07] | -0.18 [-1.15, +0.75] | Drop revision: CPU regression without RSS saving |
| Independent pressure signal revision | Linux x64 | +15.60 [-17.80, +49.40] | -1.80 [-2.44, -1.15] | -0.01 [-0.50, +0.45] | -2.47 [-4.84, -0.16] | +0.41 [-1.17, +1.52] | Drop revision: CPU regression without RSS saving |
| Same-source A/A | Linux ARM64 | +5.00 [+0.80, +9.40] | -0.50 [-1.76, +0.70] | +0.05 [-0.50, +0.61] | -1.43 [-5.49, +2.66] | -1.68 [-4.41, +0.22] | Control |
| Same-source A/A | Linux x64 | -6.00 [-46.40, +32.80] | +0.88 [+0.44, +1.30] | -0.06 [-0.40, +0.27] | -8.35 [-44.22, +47.34] | -8.20 [-22.22, +0.80] | Control |
| Same-source A/A | Windows ARM64 | -3.80 [-9.60, +2.00] | -2.34 [-6.63, +2.19] | -0.03 [-2.85, +2.76] | -3.62 [-11.42, +1.79] | -2.46 [-10.79, +2.99] | Control |
| Same-source A/A | Windows x64 | +14.00 [-9.60, +38.00] | -0.26 [-2.89, +2.60] | +0.82 [-1.67, +3.39] | +2.43 [+0.68, +4.46] | +3.09 [-3.02, +12.88] | Control |
| Full vs main checkpoint | macOS 15 Intel | +124.20 [+101.60, +145.80] | -2.88 [-8.72, +3.65] | -13.57 [-20.50, -6.58] | +2.88 [-3.93, +9.91] | -0.61 [-13.15, +13.66] | Control |
| Full vs main checkpoint | Windows x64 | +86.20 [+62.00, +108.60] | +0.27 [-4.35, +5.08] | +1.59 [-0.14, +3.28] | +3.00 [-0.41, +6.15] | -2.04 [-5.89, +1.83] | Control |
