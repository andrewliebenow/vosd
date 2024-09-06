# `vosd` (volume OSD)

`vosd` is a volume OSD for Linux (uses GTK). It listens to volume change events from PulseAudio and displays a volume OSD when a volume change has been detected.

## Installation

```shell
# TODO Publish to crates.io
cargo install --git https://github.com/andrewliebenow/vosd
```

## Usage

You will likely want to run `vosd` as a daemon (`setsid --fork vosd` works).

Demo:

![`vosd` demo](vosd.gif)

[![`vosd` demo](http://img.youtube.com/vi/SBrQ9eMF6KQ/0.jpg)](http://www.youtube.com/watch?v=SBrQ9eMF6KQ "`vosd` demo")

## License

MIT License, see <a href="LICENSE">LICENSE</a> file
