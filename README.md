# `vosd` (volume OSD)

`vosd` is a volume OSD for Linux (uses GTK). It listens to volume change events from PulseAudio and displays a volume OSD when a volume change has been detected.

## Installation

```Shell
# TODO Publish to crates.io
cargo install --git https://github.com/andrewliebenow/vosd
```

## Usage

```
❯ vosd --help
Render an OSD when the volume level is changed

Usage: vosd [OPTIONS]

Options:
  -d, --daemon   Run as a daemon
  -h, --help     Print help
  -V, --version  Print version
```

You will likely want to run `vosd` as a daemon (`vosd --daemon`).

## Demo

![`vosd` demo](vosd.gif)

[![`vosd` demo](http://img.youtube.com/vi/SBrQ9eMF6KQ/0.jpg)](http://www.youtube.com/watch?v=SBrQ9eMF6KQ "`vosd` demo")

## License

Author: Andrew Liebenow

Licensed under the MIT License, see <a href="./LICENSE">./LICENSE</a>.

`vosd` depends on libraries written by other authors. See <a href="./Cargo.toml">./Cargo.toml</a> for its direct (i.e. non-transitive) dependencies.
