# Paired allocator effects

Median candidate-minus-reference changes and 95% bootstrap intervals. CPU and latency are percent; RSS and executable size are KiB. Every non-size interval resamples matched rounds. `n` records complete pairs, including incomplete campaigns; incomplete rows cannot justify adoption.

## Sessions at 128 attached clients

| Campaign | Runner | Layout | n | Idle RSS, KiB | Active CPU, % | Turn p99, % |
| --- | --- | --- | ---: | ---: | ---: | ---: |
| local-20 | m3 | A2 | 20 | -64.00 [-352.00, +296.00] | -0.52 [-5.56, +8.86] | -0.09 [-1.88, +1.10] |
| local-20 | m3 | jemalloc | 20 | +46104.00 [+45912.00, +46264.00] | +0.37 [-5.11, +4.95] | +1.86 [-1.20, +5.04] |
| local-20 | m3 | mimalloc | 20 | +56040.00 [+55400.00, +56248.00] | +1.19 [-2.43, +6.01] | +1.64 [-1.72, +3.45] |
| local-20 | m3 | rest | 20 | +296.00 [-600.00, +568.00] | -4.20 [-8.00, -0.68] | +0.11 [-1.19, +2.00] |
| local-20 | m3 | rpmalloc | 20 | +92320.00 [+91944.00, +92520.00] | -1.21 [-8.47, +2.32] | +0.59 [-1.74, +3.37] |
| local-20 | m3 | snmalloc | 20 | +59688.00 [+59224.00, +59960.00] | +0.95 [-3.98, +9.06] | +1.49 [-1.92, +2.08] |
| local-followup-20 | m3 | A2 | 20 | +80.00 [-200.00, +304.00] | +0.52 [-1.95, +5.87] | +1.03 [-1.42, +2.54] |
| local-followup-20 | m3 | candidate | 20 | -2256.00 [-2440.00, -1952.00] | +0.45 [-2.86, +2.87] | -0.90 [-1.97, +0.77] |
| local-followup-20 | m3 | rest | 20 | +184.00 [-408.00, +360.00] | -0.05 [-3.09, +2.62] | +0.66 [-1.25, +2.59] |
| local-followup-20 | m3 | snmalloc-release | 20 | +59504.00 [+59064.00, +59592.00] | -1.27 [-6.64, +2.87] | -0.60 [-1.61, +1.66] |
| sessions-frames | ubuntu-24.04-arm | A2 | 20 | -60.00 [-198.00, +156.00] | -0.70 [-1.23, -0.42] | -0.62 [-2.50, +2.28] |
| sessions-frames | ubuntu-24.04-arm | arenas | 20 | +604.00 [+392.00, +750.00] | -0.42 [-2.32, +0.13] | +0.92 [-2.12, +3.04] |
| sessions-frames | ubuntu-24.04-arm | candidate | 20 | -2062.00 [-2262.00, -1832.00] | -0.96 [-1.78, -0.14] | +0.62 [-1.12, +2.37] |
| sessions-frames | ubuntu-24.04-arm | rest | 20 | -678.00 [-932.00, -464.00] | -0.42 [-0.98, +0.42] | -0.37 [-2.10, +2.84] |
| sessions-frames | ubuntu-latest | A2 | 20 | +62.00 [-68.00, +266.00] | -0.39 [-1.52, +0.42] | -1.43 [-3.06, +2.12] |
| sessions-frames | ubuntu-latest | arenas | 20 | +832.00 [+654.00, +1034.00] | +0.20 [-4.65, +1.53] | -0.64 [-3.60, +0.18] |
| sessions-frames | ubuntu-latest | candidate | 20 | -1978.00 [-2114.00, -1778.00] | -1.65 [-3.10, +1.79] | -1.57 [-3.88, +2.38] |
| sessions-frames | ubuntu-latest | rest | 20 | -634.00 [-716.00, -442.00] | -0.42 [-2.53, +0.74] | -1.86 [-3.77, +0.44] |
| sessions-frames | windows-11-arm | A2 | 20 | -18.00 [-144.00, +64.00] | -0.63 [-15.30, +4.13] | -0.02 [-2.63, +2.11] |
| sessions-frames | windows-11-arm | candidate | 20 | -2114.00 [-2192.00, -2044.00] | -2.36 [-6.58, +2.85] | -2.06 [-5.09, +3.20] |
| sessions-frames | windows-11-arm | rest | 20 | +60.00 [-38.00, +136.00] | -2.27 [-4.85, +2.72] | -1.79 [-3.66, +0.08] |
| sessions-frames | windows-latest | A2 | 20 | -218.00 [-416.00, +262.00] | -3.23 [-7.68, +0.58] | +1.35 [-0.82, +3.55] |
| sessions-frames | windows-latest | candidate | 20 | -1996.00 [-2476.00, -1470.00] | -0.54 [-8.42, +2.62] | +0.26 [-2.53, +2.33] |
| sessions-frames | windows-latest | rest | 20 | -1630.00 [-1834.00, -1354.00] | -1.76 [-9.26, +9.17] | +1.21 [-2.12, +2.30] |
| sessions-frames-mac-arm-128 | macos-latest | A2 | 20 | -520.00 [-632.00, -96.00] | -0.92 [-2.24, +1.03] | -1.81 [-3.09, -0.29] |
| sessions-frames-mac-arm-128 | macos-latest | candidate | 20 | -2256.00 [-2384.00, -2056.00] | +0.98 [-1.36, +4.18] | -0.66 [-1.82, +1.15] |
| sessions-frames-mac-arm-128 | macos-latest | rest | 20 | -280.00 [-584.00, +80.00] | -0.37 [-2.45, +2.03] | -1.12 [-3.24, -0.05] |
| sessions-frames-mac-intel-128 | macos-15-intel | A2 | 20 | -10.00 [-42.00, +32.00] | -1.01 [-1.46, +4.09] | -0.71 [-2.85, +1.42] |
| sessions-frames-mac-intel-128 | macos-15-intel | candidate | 20 | -2016.00 [-2036.00, -1978.00] | +2.04 [-2.95, +6.88] | +1.72 [-0.45, +3.34] |
| sessions-frames-mac-intel-128 | macos-15-intel | rest | 20 | -1636.00 [-1696.00, -1618.00] | -3.07 [-5.74, +8.45] | +1.22 [-1.00, +3.81] |
| sessions-jemalloc-snmalloc | ubuntu-24.04-arm | A2 | 20 | -44.00 [-214.00, +134.00] | -0.13 [-1.08, +1.98] | +0.40 [-0.98, +1.94] |
| sessions-jemalloc-snmalloc | ubuntu-24.04-arm | arenas | 20 | +814.00 [+606.00, +1010.00] | +0.00 [-1.98, +1.32] | +0.96 [-0.46, +2.93] |
| sessions-jemalloc-snmalloc | ubuntu-24.04-arm | jemalloc | 20 | +30140.00 [+29690.00, +30436.00] | -1.61 [-2.38, +0.00] | +7.51 [+5.99, +9.53] |
| sessions-jemalloc-snmalloc | ubuntu-24.04-arm | rest | 20 | -626.00 [-796.00, -510.00] | -0.14 [-1.07, +1.22] | +0.82 [-0.66, +2.89] |
| sessions-jemalloc-snmalloc | ubuntu-24.04-arm | snmalloc | 20 | +63902.00 [+63756.00, +64282.00] | -2.26 [-2.91, -0.40] | +6.03 [+5.40, +7.35] |
| sessions-jemalloc-snmalloc | ubuntu-latest | A2 | 20 | -48.00 [-208.00, +160.00] | -0.55 [-3.13, +0.01] | -0.96 [-2.85, +1.31] |
| sessions-jemalloc-snmalloc | ubuntu-latest | arenas | 20 | +514.00 [+354.00, +690.00] | +0.74 [-0.41, +2.19] | -1.18 [-1.73, +1.81] |
| sessions-jemalloc-snmalloc | ubuntu-latest | jemalloc | 20 | +81056.00 [+78258.00, +87220.00] | -0.85 [-2.81, +1.32] | +1.50 [-1.81, +2.58] |
| sessions-jemalloc-snmalloc | ubuntu-latest | rest | 20 | -698.00 [-870.00, -588.00] | -0.41 [-2.22, +0.54] | +0.80 [-2.01, +3.70] |
| sessions-jemalloc-snmalloc | ubuntu-latest | snmalloc | 20 | +100770.00 [+99060.00, +100998.00] | -1.13 [-2.30, -0.32] | -2.35 [-4.24, -0.46] |
| sessions-jemalloc-snmalloc | windows-latest | A2 | 20 | -4.00 [-216.00, +460.00] | +2.39 [-11.39, +7.76] | -0.65 [-3.50, +1.76] |
| sessions-jemalloc-snmalloc | windows-latest | rest | 20 | -1556.00 [-2046.00, -1262.00] | +1.96 [-3.22, +7.85] | +2.97 [-1.29, +5.36] |
| sessions-jemalloc-snmalloc | windows-latest | snmalloc | 20 | +66310.00 [+65964.00, +66716.00] | -0.33 [-6.96, +7.50] | +1.68 [-0.29, +5.07] |
| sessions-jemalloc-snmalloc-mac-arm-128 | macos-latest | A2 | 20 | +24.00 [-80.00, +80.00] | -0.90 [-2.92, +2.57] | +0.92 [-1.42, +2.49] |
| sessions-jemalloc-snmalloc-mac-arm-128 | macos-latest | jemalloc | 20 | +42864.00 [+42728.00, +43008.00] | -0.97 [-5.31, +1.95] | +1.02 [-3.21, +2.83] |
| sessions-jemalloc-snmalloc-mac-arm-128 | macos-latest | rest | 20 | +8.00 [-32.00, +72.00] | +0.94 [-1.89, +4.24] | -0.91 [-2.09, +1.25] |
| sessions-jemalloc-snmalloc-mac-arm-128 | macos-latest | snmalloc | 20 | +59368.00 [+59016.00, +59744.00] | +1.93 [-2.52, +4.05] | -0.11 [-2.42, +2.58] |
| sessions-jemalloc-snmalloc-mac-intel-128 | macos-15-intel | A2 | 20 | -4.00 [-30.00, +34.00] | -1.36 [-4.08, -0.18] | +0.27 [-1.17, +1.85] |
| sessions-jemalloc-snmalloc-mac-intel-128 | macos-15-intel | jemalloc | 20 | +19606.00 [+19484.00, +19864.00] | -0.63 [-3.31, +2.71] | -0.54 [-2.78, +1.76] |
| sessions-jemalloc-snmalloc-mac-intel-128 | macos-15-intel | rest | 20 | -1636.00 [-1678.00, -1604.00] | -0.11 [-3.15, +0.44] | -1.14 [-3.59, +1.21] |
| sessions-jemalloc-snmalloc-mac-intel-128 | macos-15-intel | snmalloc | 20 | +59650.00 [+59430.00, +60006.00] | -1.58 [-3.13, +0.45] | -1.15 [-3.03, -0.05] |
| sessions-mimalloc | macos-15-intel | A2 | 20 | -2.00 [-34.00, +16.00] | +1.29 [-0.45, +1.90] | -0.05 [-2.43, +1.96] |
| sessions-mimalloc | macos-15-intel | mimalloc | 20 | +22616.00 [+22434.00, +22758.00] | -2.54 [-4.71, -0.79] | +2.08 [+0.45, +3.45] |
| sessions-mimalloc | macos-15-intel | rest | 20 | -1616.00 [-1656.00, -1578.00] | +0.08 [-1.83, +2.98] | +0.84 [-3.33, +2.27] |
| sessions-mimalloc | macos-latest | A2 | 19 | -80.00 [-160.00, +32.00] | -1.13 [-2.93, +0.59] | -0.72 [-2.20, +0.79] |
| sessions-mimalloc | macos-latest | mimalloc | 19 | +53120.00 [+52912.00, +53408.00] | -1.52 [-3.09, +0.84] | +1.48 [-2.27, +3.17] |
| sessions-mimalloc | macos-latest | rest | 19 | -32.00 [-176.00, +160.00] | -1.31 [-3.52, +0.84] | -0.86 [-3.09, +0.54] |
| sessions-mimalloc | ubuntu-24.04-arm | A2 | 20 | -72.00 [-286.00, +48.00] | +0.27 [-0.13, +0.91] | -0.96 [-2.58, -0.04] |
| sessions-mimalloc | ubuntu-24.04-arm | arenas | 20 | +656.00 [+440.00, +840.00] | +0.82 [-0.53, +1.45] | -1.82 [-4.16, +0.08] |
| sessions-mimalloc | ubuntu-24.04-arm | mimalloc | 20 | +242322.00 [+241642.00, +243732.00] | -3.07 [-4.96, -2.08] | +3.29 [+1.03, +5.48] |
| sessions-mimalloc | ubuntu-24.04-arm | rest | 20 | -732.00 [-828.00, -506.00] | -0.13 [-0.78, +0.82] | -1.44 [-2.92, +0.93] |
| sessions-mimalloc | ubuntu-latest | A2 | 20 | +112.00 [-70.00, +398.00] | -0.17 [-1.06, +1.24] | +0.59 [-2.24, +1.40] |
| sessions-mimalloc | ubuntu-latest | arenas | 20 | +696.00 [+394.00, +1060.00] | -0.17 [-1.23, +1.38] | -1.68 [-2.53, +0.08] |
| sessions-mimalloc | ubuntu-latest | mimalloc | 20 | +239488.00 [+237870.00, +240310.00] | -1.42 [-2.27, -0.48] | -3.79 [-5.15, -0.42] |
| sessions-mimalloc | ubuntu-latest | rest | 20 | -436.00 [-736.00, -286.00] | +0.71 [-0.70, +3.74] | +0.12 [-0.45, +0.50] |
| sessions-mimalloc | windows-11-arm | A2 | 20 | +30.00 [-198.00, +142.00] | +2.41 [-1.38, +6.87] | -0.77 [-2.66, +4.82] |
| sessions-mimalloc | windows-11-arm | mimalloc | 20 | +38406.00 [+37684.00, +38994.00] | -4.62 [-10.66, +1.40] | -0.53 [-3.82, +1.03] |
| sessions-mimalloc | windows-11-arm | rest | 20 | +70.00 [-50.00, +116.00] | +2.13 [-6.59, +6.28] | -1.63 [-2.84, +2.91] |
| sessions-mimalloc | windows-latest | A2 | 20 | +252.00 [-1116.00, +716.00] | +4.29 [-1.86, +8.90] | -3.00 [-7.08, +5.00] |
| sessions-mimalloc | windows-latest | mimalloc | 20 | +31408.00 [+30812.00, +31900.00] | -2.67 [-6.75, +3.03] | -5.84 [-10.24, +0.86] |
| sessions-mimalloc | windows-latest | rest | 20 | -1660.00 [-2758.00, -1332.00] | +5.65 [-10.15, +11.37] | -5.44 [-10.08, +2.56] |
| sessions-mimalloc-mac-128 | macos-latest | A2 | 20 | -16.00 [-176.00, +72.00] | +0.68 [-4.26, +3.26] | -1.52 [-2.71, +1.92] |
| sessions-mimalloc-mac-128 | macos-latest | mimalloc | 20 | +53200.00 [+53016.00, +53352.00] | +0.31 [-3.47, +10.46] | -1.76 [-2.67, +2.15] |
| sessions-mimalloc-mac-128 | macos-latest | rest | 20 | +8.00 [-192.00, +144.00] | +1.63 [-3.10, +5.11] | -0.33 [-2.19, +0.96] |
| sessions-rpmalloc-mac-128 | macos-latest | A2 | 20 | -16.00 [-152.00, +40.00] | +2.27 [+0.41, +5.76] | +0.09 [-2.38, +3.21] |
| sessions-rpmalloc-mac-128 | macos-latest | rest | 20 | -56.00 [-120.00, +16.00] | +0.08 [-1.98, +8.34] | +0.45 [-1.17, +1.83] |
| sessions-rpmalloc-mac-128 | macos-latest | rpmalloc | 20 | +88536.00 [+88448.00, +88736.00] | -0.28 [-2.29, +5.97] | -0.55 [-2.69, +1.43] |
| sessions-rpmalloc-segment | macos-15-intel | A2 | 20 | +12.00 [-66.00, +42.00] | +2.17 [+0.16, +9.04] | -3.32 [-5.99, +3.52] |
| sessions-rpmalloc-segment | macos-15-intel | rest | 20 | -1658.00 [-1696.00, -1628.00] | +1.55 [-0.10, +5.48] | -1.26 [-2.93, +2.61] |
| sessions-rpmalloc-segment | macos-15-intel | rpmalloc | 20 | +55572.00 [+55372.00, +55726.00] | -2.53 [-17.75, +6.05] | +5.28 [+0.02, +8.42] |
| sessions-rpmalloc-segment | macos-latest | A2 | 19 | -32.00 [-80.00, +48.00] | +2.73 [+0.44, +3.45] | -0.31 [-2.15, +0.12] |
| sessions-rpmalloc-segment | macos-latest | rest | 19 | -16.00 [-144.00, +96.00] | +0.98 [-0.46, +2.70] | +0.49 [-1.39, +2.65] |
| sessions-rpmalloc-segment | macos-latest | rpmalloc | 19 | +88464.00 [+88368.00, +88624.00] | +0.28 [-1.91, +1.01] | +1.65 [-2.84, +3.12] |
| sessions-rpmalloc-segment | ubuntu-24.04-arm | A2 | 20 | -54.00 [-312.00, +218.00] | -0.12 [-0.93, +0.94] | +0.36 [-2.57, +1.24] |
| sessions-rpmalloc-segment | ubuntu-24.04-arm | arenas | 20 | +586.00 [+330.00, +810.00] | -0.35 [-2.03, +0.82] | +0.60 [-0.53, +1.99] |
| sessions-rpmalloc-segment | ubuntu-24.04-arm | rest | 20 | -680.00 [-1000.00, -498.00] | +0.46 [-1.14, +1.74] | +1.75 [-0.02, +2.78] |
| sessions-rpmalloc-segment | ubuntu-24.04-arm | rpmalloc | 20 | +56402.00 [+56214.00, +56928.00] | -2.41 [-2.87, -1.48] | +3.95 [+3.29, +6.10] |
| sessions-rpmalloc-segment | ubuntu-latest | A2 | 20 | -90.00 [-246.00, +54.00] | +0.29 [-0.39, +0.89] | -0.72 [-2.13, +0.84] |
| sessions-rpmalloc-segment | ubuntu-latest | arenas | 20 | +452.00 [+280.00, +802.00] | -0.10 [-1.07, +0.87] | -0.20 [-2.18, +1.36] |
| sessions-rpmalloc-segment | ubuntu-latest | rest | 20 | -800.00 [-990.00, -584.00] | +0.09 [-0.68, +0.48] | -0.27 [-2.48, +1.57] |
| sessions-rpmalloc-segment | ubuntu-latest | rpmalloc | 20 | +312308.00 [+305030.00, +322832.00] | -2.19 [-2.49, -1.44] | -1.18 [-3.55, +0.31] |
| sessions-rpmalloc-segment | windows-latest | A2 | 20 | +430.00 [-290.00, +868.00] | +9.25 [-0.93, +17.58] | -0.06 [-1.91, +2.95] |
| sessions-rpmalloc-segment | windows-latest | rest | 20 | -1758.00 [-2008.00, -1190.00] | +8.16 [-5.34, +15.87] | -0.59 [-2.37, +3.80] |
| sessions-rpmalloc-segment | windows-latest | rpmalloc | 20 | +76214.00 [+74256.00, +77146.00] | +6.15 [-0.12, +14.26] | -1.50 [-3.16, +1.83] |
| sessions-rpmalloc-segment | windows-latest | segment | 20 | -4426.00 [-4800.00, -3978.00] | +5.95 [-7.04, +19.92] | +1.28 [-1.76, +4.95] |
| sessions-segment-arm | windows-11-arm | A2 | 20 | +38.00 [-190.00, +96.00] | -2.13 [-7.19, +5.50] | -0.96 [-1.94, -0.29] |
| sessions-segment-arm | windows-11-arm | rest | 20 | +0.00 [-136.00, +100.00] | +0.06 [-7.57, +7.07] | -1.24 [-4.52, +1.23] |
| sessions-segment-arm | windows-11-arm | segment | 20 | -54.00 [-198.00, +20.00] | +2.38 [-4.94, +9.70] | -1.21 [-4.24, +0.51] |

## Separate executables and daemon

| Campaign | Runner | n | Idle RSS, KiB | Driver CPU, % | Large JSON CPU, % | Attach p99, % | Size, KiB |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| paired-aa | macos-15-intel | 20 | +4.00 [-2.00, +18.00] | -0.88 [-2.24, +0.84] | -1.45 [-4.79, +5.05] | +2.70 [-2.86, +8.63] | +0.00 |
| paired-aa | macos-latest | 20 | +8.00 [-16.00, +40.00] | +0.30 [-0.54, +0.66] | +0.11 [-0.23, +0.94] | -1.56 [-3.74, +1.56] | +0.00 |
| paired-aa | ubuntu-24.04-arm | 20 | -4.00 [-8.00, +0.00] | -0.37 [-0.89, +0.09] | +0.57 [+0.35, +0.99] | -0.29 [-2.21, +0.94] | +0.00 |
| paired-aa | ubuntu-latest | 20 | -20.00 [-100.00, +40.00] | -0.18 [-0.96, +0.46] | -0.11 [-0.57, +0.13] | +1.62 [+0.91, +1.94] | +0.00 |
| paired-aa | windows-11-arm | 6 | +8.00 [-4.00, +20.00] | -0.97 [-9.06, +7.82] | -5.33 [-11.52, +1.67] | +1.14 [-2.77, +4.79] | +0.00 |
| paired-aa | windows-latest | 20 | +32.00 [+4.00, +80.00] | +2.61 [-4.91, +6.32] | -6.62 [-11.21, -0.02] | +2.59 [-4.19, +5.05] | +0.00 |
| paired-aa-arm-retry | windows-11-arm | 18 | +2.00 [-2.00, +10.00] | -3.01 [-6.84, +2.31] | +0.85 [-2.84, +4.62] | -4.97 [-22.88, +9.49] | +0.00 |
| paired-jemalloc | macos-15-intel | 20 | +1734.00 [+1698.00, +1752.00] | -18.34 [-22.24, -17.33] | -14.59 [-17.67, -12.97] | +4.05 [-2.75, +11.56] | +247.51 |
| paired-jemalloc | macos-latest | 20 | +2544.00 [+2520.00, +2552.00] | +3.27 [-3.28, +15.31] | +1.19 [-4.44, +3.53] | +2.67 [-6.05, +39.45] | +225.95 |
| paired-jemalloc | ubuntu-24.04-arm | 20 | +2110.00 [+2098.00, +2122.00] | -12.48 [-12.82, -12.03] | -3.82 [-3.93, -3.36] | -0.34 [-1.91, +1.53] | +296.27 |
| paired-jemalloc | ubuntu-latest | 20 | +38882.00 [+38822.00, +38932.00] | -28.26 [-29.43, -27.77] | -6.99 [-7.52, -6.58] | +0.48 [-1.13, +3.00] | +270.98 |
| paired-jemalloc | windows-11-arm | 20 | +34.00 [+20.00, +38.00] | +0.69 [-4.53, +3.44] | -1.89 [-3.03, +4.82] | +1.16 [-2.25, +1.99] | +0.00 |
| paired-jemalloc | windows-latest | 20 | -18.00 [-36.00, +18.00] | -2.02 [-4.77, +1.74] | -1.47 [-2.90, +2.56] | -1.08 [-2.25, +1.52] | +0.00 |
| paired-mimalloc | macos-15-intel | 20 | +952.00 [+940.00, +964.00] | -15.67 [-17.52, -6.57] | -9.85 [-14.06, -4.92] | -6.87 [-13.97, +5.33] | +89.06 |
| paired-mimalloc | macos-latest | 20 | +1712.00 [+1696.00, +1736.00] | -0.87 [-5.92, +2.93] | -2.10 [-5.01, +0.01] | -3.41 [-9.38, +14.89] | +69.84 |
| paired-mimalloc | ubuntu-24.04-arm | 20 | +18814.00 [+18808.00, +18820.00] | -20.62 [-20.70, -20.37] | -8.43 [-8.89, -7.98] | +0.85 [-0.62, +3.76] | +109.91 |
| paired-mimalloc | ubuntu-latest | 20 | +21604.00 [+21570.00, +21636.00] | -19.48 [-19.72, -19.19] | -9.00 [-9.88, -8.14] | -9.49 [-13.85, -5.27] | +102.07 |
| paired-mimalloc | windows-11-arm | 20 | +1390.00 [+1380.00, +1404.00] | -1.38 [-8.09, +1.66] | -16.94 [-19.06, -14.85] | -3.52 [-5.89, -1.66] | +101.00 |
| paired-mimalloc | windows-latest | 20 | +1276.00 [+1238.00, +1284.00] | -20.72 [-22.58, -15.64] | -15.82 [-17.32, -13.95] | -1.23 [-2.94, +0.52] | +113.00 |
| paired-rest | macos-15-intel | 20 | -2.00 [-20.00, +24.00] | +2.59 [-2.32, +7.12] | +9.49 [+4.42, +14.50] | +2.48 [-4.04, +9.33] | +0.09 |
| paired-rest | macos-latest | 20 | +48.00 [+0.00, +88.00] | -1.22 [-9.40, +9.26] | +5.43 [-1.25, +18.91] | -4.55 [-23.26, +32.42] | +0.11 |
| paired-rest | ubuntu-24.04-arm | 20 | +0.00 [-6.00, +4.00] | -1.45 [-2.68, -0.17] | +1.27 [+0.03, +1.71] | +0.17 [-1.08, +1.52] | +0.25 |
| paired-rest | ubuntu-latest | 20 | +4.00 [-48.00, +70.00] | -1.53 [-1.86, -1.00] | +1.31 [+1.04, +1.55] | +0.09 [-0.69, +1.03] | +0.25 |
| paired-rest | windows-11-arm | 10 | -16.00 [-20.00, +2.00] | +1.63 [-6.37, +10.04] | -3.23 [-8.67, +4.67] | -0.94 [-1.96, +1.99] | +0.50 |
| paired-rest | windows-latest | 20 | -32.00 [-50.00, +8.00] | -5.71 [-9.51, +1.79] | +1.75 [+0.58, +5.29] | -0.54 [-3.36, +1.07] | +0.00 |
| paired-rest-arm-retry | windows-11-arm | 20 | -14.00 [-32.00, -2.00] | -3.74 [-5.91, +4.47] | -0.32 [-4.36, +4.34] | -0.21 [-4.32, +2.90] | +0.50 |
| paired-rpmalloc | macos-latest | 20 | +3256.00 [+3216.00, +3328.00] | -0.56 [-5.77, +2.38] | -0.83 [-7.85, +3.86] | -14.81 [-24.77, +14.82] | +0.48 |
| paired-rpmalloc | ubuntu-24.04-arm | 20 | +1992.00 [+1976.00, +2036.00] | -16.59 [-17.04, -16.27] | -3.63 [-4.21, -2.92] | +0.65 [-0.86, +1.72] | +12.38 |
| paired-rpmalloc | ubuntu-latest | 20 | +17384.00 [+17206.00, +19086.00] | -28.80 [-29.32, -28.35] | -2.19 [-2.56, -1.97] | +1.95 [+1.35, +2.88] | +13.16 |
| paired-rpmalloc | windows-latest | 20 | +2574.00 [+2550.00, +2610.00] | -30.48 [-32.53, -28.13] | -14.52 [-15.78, -12.26] | -0.68 [-3.88, +0.39] | +2.00 |
| paired-rpmalloc-intel-retry | macos-15-intel | 20 | +1838.00 [+1832.00, +1862.00] | -21.06 [-23.33, -18.19] | -17.38 [-23.66, -12.52] | +10.62 [-1.61, +20.74] | +12.51 |
| paired-segment-windows-11-arm | windows-11-arm | 20 | +8.00 [-4.00, +28.00] | +4.51 [-0.31, +10.81] | -2.47 [-3.86, +0.98] | +2.91 [+0.62, +4.17] | +0.00 |
| paired-segment-windows-latest | windows-latest | 20 | -92.00 [-112.00, -52.00] | -0.40 [-8.49, +6.51] | +0.00 [-2.43, +6.52] | +2.73 [-2.97, +5.15] | +0.00 |
| paired-snmalloc | macos-15-intel | 20 | +1996.00 [+1946.00, +2020.00] | +9.50 [+7.40, +11.61] | +25.97 [+22.72, +35.80] | -8.22 [-12.41, +17.81] | +137.53 |
| paired-snmalloc | macos-latest | 20 | +1720.00 [+1696.00, +1760.00] | +25.91 [+12.23, +43.57] | +22.73 [+18.40, +28.34] | -8.19 [-19.99, -1.21] | +146.77 |
| paired-snmalloc | ubuntu-24.04-arm | 20 | +9802.00 [+9776.00, +9840.00] | +15.07 [+14.56, +15.32] | +3.60 [+3.27, +4.27] | +1.76 [-1.53, +3.47] | +95.12 |
| paired-snmalloc | ubuntu-latest | 20 | +16146.00 [+15896.00, +16378.00] | +6.81 [+5.88, +7.28] | -1.29 [-2.30, -0.68] | +6.66 [+1.09, +12.20] | +91.76 |
| paired-snmalloc | windows-latest | 20 | +3250.00 [+3162.00, +3302.00] | -31.17 [-32.83, -27.26] | +11.92 [+10.21, +14.49] | -2.76 [-11.70, +1.05] | -33.50 |

The CSV retains the one-session comparisons, physical footprint, peak and post-release memory, idle CPU, release time, active wall time, and remaining latency/CPU metrics. Unsupported builds have no substituted measurements. Windows jemalloc uses the system allocator and is a control, not a jemalloc implementation. The original local snmalloc build enabled C++ debug mode; the corrected release result supersedes it. IFEO segment-heap sessions are unverified; executable comparisons embed the manifest.
