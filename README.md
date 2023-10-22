# <img src="https://raw.githubusercontent.com/LiveSplit/LiveSplit/master/LiveSplit/Resources/Icon.png" alt="LiveSplit" height="42" width="45" align="top"/> asr-debugger

This repository hosts the debugger for LiveSplit One's [auto splitting
runtime](https://github.com/LiveSplit/livesplit-core/tree/master/crates/livesplit-auto-splitting).

## Features

- Hot reloading of the auto splitters is supported.
- Stepping through the auto splitter's code is possible by attaching LLDB.
- The performance of the auto splitter can be measured.
- All the log output is shown directly in the debugger.
- All the variables that the auto splitter has set are shown.
- The settings of the auto splitter can be quickly changed.
- For deeper debugging, the memory of the auto splitter can be dumped.

## Build Instructions

In order to build the asr-debugger you need the [Rust
compiler](https://www.rust-lang.org/). You can then build and run the project
with:

```bash
cargo run
```

In order to build and run a release build, use the following command:

```bash
cargo run --release
```
