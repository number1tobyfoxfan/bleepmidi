# bleepmidi

```text
 ______ _      _____ ______________  ______________ _____
| ___ \ |    |  ___|  ___| ___ \  \/  |_   _|  _  \_   _|
| |_/ / |    | |__ | |__ | |_/ / .  . | | | | | | | | |
| ___ \ |    |  __||  __||  __/| |\/| | | | | | | | | |
| |_/ / |____| |___| |___| |   | |  | |_| |_| |/ / _| |_
\____/\_____/\____/\____/\_|   \_|  |_/\___/|___/  \___/
beeps & boops for your MIDI
```

Turn a Standard MIDI File into PC-speaker beeps, play it straight to your
speakers, or render it to WAV or video.

bleepmidi is a small Rust program with a single dependency
([`midly`](https://crates.io/crates/midly)) for MIDI parsing.

## Requirements

- Rust 1.85 or newer (edition 2024) to build
- `aplay` from `alsa-utils` for playback
- `ffmpeg` with `libass` and `libx264`, plus the `DejaVu Sans Mono` font,
  for `--video`

On Debian/Ubuntu:

```sh
sudo apt install alsa-utils ffmpeg fonts-dejavu-core
```

## Install

```sh
git clone https://github.com/number1tobyfoxfan/bleepmidi.git
cd bleepmidi
make install
```

`make install` builds the release binaries and copies `bleepmidi` and
`bleepmidi-video` to `~/.local/bin` (override with `PREFIX=/usr/local`).

Alternatives:

```sh
cargo install --path .      # install both binaries via Cargo
cargo build --release       # just build into target/release/
```

## Usage

```sh
bleepmidi song.mid
bleepmidi --demo
```

```text
 ______ _      _____ ______________  ______________ _____
| ___ \ |    |  ___|  ___| ___ \  \/  |_   _|  _  \_   _|
| |_/ / |    | |__ | |__ | |_/ / .  . | | | | | | | | |
| ___ \ |    |  __||  __||  __/| |\/| | | | | | | | | |
| |_/ / |____| |___| |___| |   | |  | |_| |_| |/ / _| |_
\____/\_____/\____/\____/\_|   \_|  |_/\___/|___/  \___/
beeps & boops for your MIDI

bleepmidi: playing 33 notes · 00:16 · 44100 Hz
 00:07 / 00:16 [##########--------------] D4
```

`--demo` plays the embedded 33-note tune. Use `--no-banner` to skip the logo
and tagline, and `--no-progress` to hide the progress line.

### WAV

```sh
bleepmidi song.mid --output song.wav
bleepmidi --demo -o demo.wav
```

The WAV is 16-bit mono PCM at 44.1 kHz. A `.wav` extension is added when the
output path has none.

## Video export

`--video` renders the same terminal UI as playback to an MP4 with the audio
baked in:

```sh
bleepmidi song.mid --video song.mp4
bleepmidi --demo --video demo.mp4 --video-size 1920x1080 --fps 60
```

The frame is a plain 80×24 terminal grid (no window chrome): logo, tagline,
status line, and the live progress/current-note line, drawn with
`DejaVu Sans Mono` on `#0D1117`. The progress line updates every 0.1 s while
the video frame rate defaults to 30 fps, which keeps the subtitle file small
and the encode fast. `--no-banner` hides the logo and tagline in the video
too; the progress line is always part of the UI.

Output is H.264 (`yuv420p`) with AAC audio in an MP4 container, using
`-movflags +faststart`. The source name is stored in the MP4 title metadata.
The video runs one second longer than the song to match the audio tail.

`bleepmidi-video` is a native alias that puts the output file first:

```sh
bleepmidi-video song.mp4 song.mid
bleepmidi-video demo.mp4 --demo --video-size 1280x720
```

## Options

| Option | Description |
| --- | --- |
| `--output FILE`, `-o` | Write a WAV file instead of playing |
| `--video FILE` | Render a terminal-UI MP4 with audio instead of playing |
| `--video-size WxH` | Video dimensions (default `1280x720`, odd values rounded up) |
| `--fps N` | Video frame rate, 1–60 (default `30`) |
| `--no-banner` | Hide the logo/tagline in playback and video |
| `--no-progress` | Hide the console progress meter and encode progress |
| `--poly`, `--chords` | Accepted for compatibility; every note is already mixed |
| `--demo` | Use the built-in demo tune |
| `-h`, `--help` | Show help |
| `-V`, `--version` | Show the version |

`--video` and `--output` are mutually exclusive, and you cannot pass both a
MIDI file and `--demo`.

## How it works

1. **Parse** — `midly` reads the Standard MIDI File and bleepmidi collects
   every note-on/note-off pair, keeping note velocity.
2. **Synthesize** — each note becomes a square wave with a short attack and
   release so nothing clicks, plus a gentle exponential decay. All sounding
   notes are mixed; a limiter keeps chords from clipping. Velocity controls
   loudness.
3. **Play or export** — samples are rendered in 4 KiB chunks. Playback streams
   them to `aplay` while a one-line progress bar shows elapsed time and the
   highest sounding note. `--output` writes the same samples to a WAV.
4. **Video** — the audio is rendered to a temporary WAV, an ASS subtitle file
   is generated for the terminal text and timed progress lines, and `ffmpeg`
   composites both over a solid colour source.

## Tests

```sh
cargo test
```

The suite covers MIDI parsing, note segmentation, argument parsing, video
size handling, ASS timestamps, and the terminal layout. For a full check
including formatting and lints:

```sh
make fmt
make lint
make test
```

## Troubleshooting

- **No sound / device busy** — bleepmidi pipes raw PCM to `aplay`. Make sure
  ALSA is configured, or render a WAV with `-o song.wav` and play it with
  anything.
- **Video export fails** — check `ffmpeg -filters | grep ass` and
  `ffmpeg -encoders | grep libx264`. Install a full ffmpeg build if either is
  missing. The `ass` filter also needs `DejaVu Sans Mono` installed.
- **Encoding is slow** — lower `--fps`, use a smaller `--video-size`, or keep
  the default 1280×720.
- **Song plays at the wrong speed** — bleepmidi currently assumes 120 BPM and
  does not apply MIDI tempo changes; see Limitations.

## Limitations

- Tempo meta-events are ignored; timing assumes 500,000 µs per quarter note
  (120 BPM). SMPTE-timed files fall back to 480 PPQ.
- Playback is always polyphonic and includes every track and channel. There
  are no channel, track, transpose, speed, or instrument-selection options.
- `aplay` is the only playback backend.

## License

bleepmidi is licensed under the GNU Affero General Public License, version 3
or any later version (`AGPL-3.0-or-later`). See [LICENSE](LICENSE).
