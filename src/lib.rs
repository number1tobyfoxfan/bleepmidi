// SPDX-License-Identifier: AGPL-3.0-or-later

//! Play Standard MIDI Files as PC-speaker beeps.
//!
//! This crate powers two command-line tools:
//!
//! - `bleepmidi` — play a MIDI file, write a WAV, or render a video.
//! - `bleepmidi-video` — same as `bleepmidi --video`, with the output file
//!   as the first argument.

use std::collections::{BTreeSet, HashMap};
use std::env;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use midly::{Smf, Timing};

const SAMPLE_RATE: usize = 44100;
const BASE_AMP: f64 = 32767.0 * 0.6;
const ATTACK_S: f64 = 0.0035;
const RELEASE_S: f64 = 0.0035;
const DECAY_TAU: f64 = 1.8;

const DEFAULT_TAGLINE: &str = "beeps & boops for your MIDI";
const DEMO_MIDI: &[u8] = include_bytes!("../midi/demo.mid");

const LOGO: &str = r#" ______ _      _____ ______________  ______________ _____
| ___ \ |    |  ___|  ___| ___ \  \/  |_   _|  _  \_   _|
| |_/ / |    | |__ | |__ | |_/ / .  . | | | | | | | | |
| ___ \ |    |  __||  __||  __/| |\/| | | | | | | | | |
| |_/ / |____| |___| |___| |   | |  | |_| |_| |/ / _| |_
\____/\_____/\____/\____/\_|   \_|  |_/\___/|___/  \___/ "#;

// The video is a plain 80x24 terminal. These ratios are the measured
// DejaVu Sans Mono advance in libass, so the text block stays centred while
// the characters inside it keep terminal columns.
const TERM_COLS: usize = 80;
const TERM_ROWS: usize = 24;
const TERM_CELL_RATIO: f64 = 0.517;
const TERM_LINE_RATIO: f64 = 1.0;
const ASS_TEXT: &str = "&H00D9D1C9&";
const VIDEO_BG: &str = "0x0D1117";

const VIDEO_STEP: f64 = 0.1;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Which command-line shape this process was started with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Invocation {
    /// `bleepmidi [options] <file.mid>`
    Bleepmidi,
    /// `bleepmidi-video OUTPUT.mp4 [options] <file.mid>`
    Video,
}

impl Invocation {
    fn name(self) -> &'static str {
        match self {
            Self::Bleepmidi => "bleepmidi",
            Self::Video => "bleepmidi-video",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Note {
    start: usize,
    end: usize,
    pitch: u8,
    velocity: u8,
}

struct Song {
    notes: Vec<Note>,
    seconds_per_tick: f64,
    duration: f64,
}

struct Voice {
    amp0: f64,
    exp_val: f64,
    sign: f64,
    start: usize,
    end: usize,
    next_edge: f64,
    high: f64,
    low: f64,
}

#[derive(Clone, Copy, Debug)]
struct Segment {
    start: f64,
    end: f64,
    pitch: u8,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EdgeKind {
    End,
    Start,
}

struct Options {
    input: Option<String>,
    video: Option<String>,
    output: Option<String>,
    video_size: String,
    fps: u32,
    no_banner: bool,
    no_progress: bool,
    demo: bool,
    help: bool,
    version: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            input: None,
            video: None,
            output: None,
            video_size: "1280x720".to_string(),
            fps: 30,
            no_banner: false,
            no_progress: false,
            demo: false,
            help: false,
            version: false,
        }
    }
}

struct TempPath(PathBuf);

impl TempPath {
    fn new(extension: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let serial = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!(
            "bleepmidi-{}-{}-{}.{}",
            std::process::id(),
            nanos,
            serial,
            extension
        );
        Self(env::temp_dir().join(name))
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Run the selected command, printing any error and exiting non-zero.
pub fn main(invocation: Invocation) {
    if let Err(error) = run(invocation) {
        eprintln!("{}: {error}", invocation.name());
        std::process::exit(1);
    }
}

fn run(invocation: Invocation) -> io::Result<()> {
    let options = parse_args(invocation, env::args().skip(1))
        .map_err(|message| io::Error::new(io::ErrorKind::InvalidInput, message))?;

    if options.help {
        print_usage();
        return Ok(());
    }
    if options.version {
        println!("{} {}", invocation.name(), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if options.video.is_some() && options.output.is_some() {
        return Err(invalid("--video and --output are mutually exclusive"));
    }
    if options.input.is_some() && options.demo {
        return Err(invalid("use either a MIDI file or --demo, not both"));
    }
    if options.input.is_none() && !options.demo {
        return Err(invalid(
            "give me a MIDI file or --demo (run with --help for usage)",
        ));
    }
    if !(1..=60).contains(&options.fps) {
        return Err(invalid("--fps must be between 1 and 60"));
    }
    if let Some(video) = &options.video {
        if video.is_empty() {
            return Err(invalid("--video needs a non-empty output file"));
        }
    }
    if let Some(output) = &options.output {
        if output.is_empty() {
            return Err(invalid("--output needs a non-empty file"));
        }
    }

    let label = if options.demo {
        "demo".to_string()
    } else {
        options.input.clone().unwrap()
    };

    let data = if options.demo {
        DEMO_MIDI.to_vec()
    } else {
        let mut file = File::open(&label)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)?;
        data
    };

    let song = load_song(&data)?;

    if let Some(output) = options.output.as_deref() {
        let path = ensure_extension(output, "wav");
        render_to_wav(&path, &song)?;
        let size = fs::metadata(&path)?.len();
        println!(
            "Wrote {} ({:.1} MiB, {}, {} Hz mono)",
            path.display(),
            size as f64 / 1_048_576.0,
            fmt_time(song.duration),
            SAMPLE_RATE
        );
        return Ok(());
    }

    if let Some(video) = options.video.as_deref() {
        return make_video(&options, video, &song, &label);
    }

    play(&options, &song)
}

fn parse_args(
    invocation: Invocation,
    args: impl IntoIterator<Item = String>,
) -> Result<Options, String> {
    let mut options = Options::default();
    let mut args = args.into_iter();

    if invocation == Invocation::Video {
        let Some(first) = args.next() else {
            return Err("give me a MIDI file or --demo (see --help)".to_string());
        };
        match first.as_str() {
            "-h" | "--help" => {
                options.help = true;
                return Ok(options);
            }
            "-V" | "--version" => {
                options.version = true;
                return Ok(options);
            }
            _ => options.video = Some(first),
        }
    }

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => options.help = true,
            "-V" | "--version" => options.version = true,
            "--demo" => options.demo = true,
            "--poly" | "--chords" => {}
            "--no-banner" => options.no_banner = true,
            "--no-progress" => options.no_progress = true,
            "--video" => {
                if invocation == Invocation::Video {
                    return Err("the output file comes first; drop the extra --video".to_string());
                }
                options.video = Some(
                    args.next()
                        .ok_or_else(|| "--video needs an output file".to_string())?,
                );
            }
            "--output" | "-o" => {
                options.output = Some(
                    args.next()
                        .ok_or_else(|| "--output needs a file".to_string())?,
                );
            }
            "--video-size" => {
                options.video_size = args
                    .next()
                    .ok_or_else(|| "--video-size needs WxH".to_string())?;
            }
            "--fps" => {
                let value = args
                    .next()
                    .ok_or_else(|| "--fps needs a number".to_string())?;
                options.fps = value
                    .parse()
                    .map_err(|_| "--fps must be a number".to_string())?;
            }
            _ if arg.starts_with("--video=") => {
                if invocation == Invocation::Video {
                    return Err("the output file comes first; drop the extra --video".to_string());
                }
                options.video = Some(arg["--video=".len()..].to_string());
            }
            _ if arg.starts_with("--output=") => {
                options.output = Some(arg["--output=".len()..].to_string());
            }
            _ if arg.starts_with("--video-size=") => {
                options.video_size = arg["--video-size=".len()..].to_string();
            }
            _ if arg.starts_with("--fps=") => {
                options.fps = arg["--fps=".len()..]
                    .parse()
                    .map_err(|_| "--fps must be a number".to_string())?;
            }
            _ if arg.starts_with('-') => {
                return Err(format!("unknown option: {arg}"));
            }
            _ => {
                if options.input.is_some() {
                    return Err("only one MIDI file at a time".to_string());
                }
                options.input = Some(arg);
            }
        }
    }

    Ok(options)
}

fn print_usage() {
    println!(
        "Usage: bleepmidi [options] <file.mid>\n\
         \x20      bleepmidi --demo [options]\n\
         \x20      bleepmidi-video OUTPUT.mp4 [options] <file.mid>\n\
         \x20      bleepmidi-video OUTPUT.mp4 --demo [options]\n\
         \n\
         Play a MIDI file as PC-speaker beeps, or render it to a video.\n\
         \n\
         Options:\n\
         \x20 --video FILE          Render an MP4 instead of playing\n\
         \x20 --video-size WxH      Video size (default 1280x720)\n\
         \x20 --fps N               Video frame rate, 1-60 (default 30)\n\
         \x20 --output FILE, -o     Write a WAV file instead of playing\n\
         \x20 --no-banner           Hide the logo/tagline\n\
         \x20 --no-progress         Hide the console progress meter\n\
         \x20 --poly, --chords      Accepted for compatibility\n\
         \x20 --demo                Use the built-in demo tune\n\
         \x20 -h, --help            Show this help\n\
         \x20 -V, --version         Show the version"
    );
}

fn load_song(data: &[u8]) -> io::Result<Song> {
    let smf = Smf::parse(data)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

    let ppq = match smf.header.timing {
        Timing::Metrical(t) => u16::from(t) as f64,
        _ => 480.0,
    };
    let seconds_per_tick = (500_000.0 / 1_000_000.0) / ppq;

    let mut notes: Vec<Note> = Vec::new();
    for track in smf.tracks {
        let mut tick = 0usize;
        let mut open_notes: HashMap<u8, (usize, u8)> = HashMap::new();

        for event in track {
            tick += u32::from(event.delta) as usize;

            let midly::TrackEventKind::Midi { message, .. } = event.kind else {
                continue;
            };
            match message {
                midly::MidiMessage::NoteOn { key, vel } => {
                    let pitch = u8::from(key);
                    let velocity = u8::from(vel);
                    if velocity > 0 {
                        open_notes.insert(pitch, (tick, velocity));
                    } else if let Some((start, velocity)) = open_notes.remove(&pitch) {
                        notes.push(Note {
                            start,
                            end: tick,
                            pitch,
                            velocity,
                        });
                    }
                }
                midly::MidiMessage::NoteOff { key, .. } => {
                    let pitch = u8::from(key);
                    if let Some((start, velocity)) = open_notes.remove(&pitch) {
                        notes.push(Note {
                            start,
                            end: tick,
                            pitch,
                            velocity,
                        });
                    }
                }
                _ => {}
            }
        }

        for (pitch, (start, velocity)) in open_notes {
            notes.push(Note {
                start,
                end: tick,
                pitch,
                velocity,
            });
        }
    }

    notes.sort_by_key(|note| note.start);

    let duration = notes.iter().map(|note| note.end).max().unwrap_or(0) as f64 * seconds_per_tick;

    Ok(Song {
        notes,
        seconds_per_tick,
        duration,
    })
}

fn note_freq(pitch: u8) -> f64 {
    440.0 * 2.0f64.powf((pitch as f64 - 69.0) / 12.0)
}

fn note_name(pitch: u8) -> String {
    let names = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!("{}{}", names[(pitch % 12) as usize], (pitch / 12) as i8 - 1)
}

fn fmt_time(seconds: f64) -> String {
    let s = seconds.max(0.0) as i64;
    let hours = s / 3600;
    let minutes = (s % 3600) / 60;
    let secs = s % 60;
    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, minutes, secs)
    } else {
        format!("{:02}:{:02}", minutes, secs)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn play(options: &Options, song: &Song) -> io::Result<()> {
    if !options.no_banner {
        eprintln!("{}", LOGO);
        eprintln!("{}\n", DEFAULT_TAGLINE);
    }

    eprintln!(
        "bleepmidi: playing {} notes · {} · {} Hz",
        song.notes.len(),
        fmt_time(song.duration),
        SAMPLE_RATE
    );

    let sample_rate = SAMPLE_RATE.to_string();
    let mut child = Command::new("aplay")
        .args([
            "-q",
            "-t",
            "raw",
            "-f",
            "S16_LE",
            "-r",
            &sample_rate,
            "-c",
            "1",
        ])
        .stdin(Stdio::piped())
        .spawn()
        .map_err(|error| io::Error::new(error.kind(), format!("cannot start aplay: {error}")))?;

    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "aplay stdin unavailable"))?;

    let frames = total_frames(song.duration, SAMPLE_RATE);
    let segments = build_segments(&song.notes, song.seconds_per_tick);
    let show_progress = !options.no_progress;
    let mut last_ui_update = Instant::now();

    let result = render_audio(song, frames, SAMPLE_RATE, |buffer, frame| {
        stdin.write_all(buffer)?;

        if show_progress && frame < frames && last_ui_update.elapsed() >= Duration::from_millis(100)
        {
            let current_audio_sec = frame as f64 / SAMPLE_RATE as f64;
            let current_note = note_at(&segments, current_audio_sec);
            print!(
                "\r\x1b[K{}",
                progress_line(current_audio_sec, song.duration, current_note)
            );
            io::stdout().flush()?;
            last_ui_update = Instant::now();
        }

        Ok(())
    });

    drop(stdin);

    if result.is_err() {
        let _ = child.wait();
    }
    result?;
    child.wait()?;
    eprintln!("\n");
    Ok(())
}

fn total_frames(duration: f64, sample_rate: usize) -> usize {
    (duration.max(0.0) * sample_rate as f64) as usize + sample_rate
}

fn build_segments(notes: &[Note], seconds_per_tick: f64) -> Vec<Segment> {
    let mut events: Vec<(f64, EdgeKind, u8)> = Vec::with_capacity(notes.len() * 2);

    for note in notes {
        let start = note.start as f64 * seconds_per_tick;
        let end = note.end as f64 * seconds_per_tick;
        if end > start {
            events.push((start, EdgeKind::Start, note.pitch));
            events.push((end, EdgeKind::End, note.pitch));
        }
    }

    if events.is_empty() {
        return Vec::new();
    }

    events.sort_by(|left, right| {
        left.0
            .partial_cmp(&right.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.1.cmp(&right.1))
    });

    let mut active: BTreeSet<u8> = BTreeSet::new();
    let mut segments = Vec::new();
    let mut last = events[0].0;
    let mut index = 0usize;

    while index < events.len() {
        let when = events[index].0;

        if let Some(&pitch) = active.last() {
            if when > last + 1e-9 {
                segments.push(Segment {
                    start: last,
                    end: when,
                    pitch,
                });
            }
        }

        while index < events.len() && (events[index].0 - when).abs() < 1e-9 {
            match events[index].1 {
                EdgeKind::Start => {
                    active.insert(events[index].2);
                }
                EdgeKind::End => {
                    active.remove(&events[index].2);
                }
            }
            index += 1;
        }

        last = when;
    }

    segments
}

fn note_at(segments: &[Segment], when: f64) -> Option<u8> {
    let index = segments.partition_point(|segment| segment.end <= when);
    segments
        .get(index)
        .filter(|segment| segment.start <= when + 1e-9)
        .map(|segment| segment.pitch)
}

fn progress_line(when: f64, duration: f64, note: Option<u8>) -> String {
    let width = 24usize;
    let filled = if duration > 0.0 {
        ((width as f64 * (when / duration)) as usize).min(width)
    } else {
        0
    };
    let bar = "#".repeat(filled) + &"-".repeat(width - filled);
    let name = note.map(note_name).unwrap_or_default();
    format!(
        " {} / {} [{}] {:<4}",
        fmt_time(when),
        fmt_time(duration),
        bar,
        name
    )
}

fn render_audio<F>(song: &Song, frames: usize, sample_rate: usize, mut emit: F) -> io::Result<()>
where
    F: FnMut(&[u8], usize) -> io::Result<()>,
{
    let mut active_voices: Vec<Voice> = Vec::new();
    let mut note_index = 0usize;

    let decay_mult = (-1.0 / (DECAY_TAU * sample_rate as f64)).exp();
    let attack_inv = 1.0 / (ATTACK_S * sample_rate as f64);
    let release_inv = 1.0 / (RELEASE_S * sample_rate as f64);

    let mut buffer = Vec::with_capacity(4096);

    for frame in 0..frames {
        while note_index < song.notes.len() {
            let note = song.notes[note_index];
            let start_frame =
                (note.start as f64 * song.seconds_per_tick * sample_rate as f64) as usize;
            if start_frame > frame {
                break;
            }

            let end_frame = (note.end as f64 * song.seconds_per_tick * sample_rate as f64) as usize;
            let frequency = note_freq(note.pitch);
            let period = sample_rate as f64 / frequency;
            let high = period / 2.0;
            let low = period - high;

            active_voices.push(Voice {
                amp0: BASE_AMP * (0.25 + 0.75 * (note.velocity as f64 / 127.0)),
                exp_val: 1.0,
                sign: 1.0,
                start: start_frame,
                end: end_frame,
                next_edge: start_frame as f64 + high,
                high,
                low,
            });
            note_index += 1;
        }

        let mut total = 0.0;
        let mut signed = 0.0;

        active_voices.retain_mut(|voice| {
            if frame >= voice.end {
                return false;
            }

            let mut amplitude = voice.amp0;
            if frame < voice.start + (ATTACK_S * sample_rate as f64) as usize {
                let t = frame as f64 - voice.start as f64;
                amplitude *= if t > 0.0 { t * attack_inv } else { 0.0 };
            }
            let remaining = voice.end as f64 - frame as f64;
            if remaining < (RELEASE_S * sample_rate as f64) {
                amplitude *= if remaining > 0.0 {
                    remaining * release_inv
                } else {
                    0.0
                };
            }

            if amplitude > 0.0 {
                amplitude *= 0.78 + 0.22 * voice.exp_val;
                total += amplitude;
                signed += if voice.sign > 0.0 {
                    amplitude
                } else {
                    -amplitude
                };
            }

            voice.exp_val *= decay_mult;

            if frame as f64 >= voice.next_edge {
                voice.sign *= -1.0;
                voice.next_edge += if voice.sign > 0.0 {
                    voice.high
                } else {
                    voice.low
                };
            }
            true
        });

        if total > BASE_AMP {
            let ratio = BASE_AMP / total;
            signed *= ratio;
        }

        let sample = signed.clamp(-32768.0, 32767.0) as i16;
        buffer.extend_from_slice(&sample.to_le_bytes());

        if buffer.len() >= 4096 {
            emit(&buffer, frame)?;
            buffer.clear();
        }
    }

    if !buffer.is_empty() {
        emit(&buffer, frames)?;
    }

    Ok(())
}

fn render_to_wav(path: &Path, song: &Song) -> io::Result<()> {
    let frames = total_frames(song.duration, SAMPLE_RATE);
    let data_len = frames
        .checked_mul(2)
        .and_then(|bytes| u32::try_from(bytes).ok())
        .ok_or_else(|| invalid("song is too long for a WAV file"))?;

    let mut file = File::create(path)?;
    write_wav_header(&mut file, data_len, SAMPLE_RATE as u32)?;
    render_audio(song, frames, SAMPLE_RATE, |buffer, _| {
        file.write_all(buffer)
    })
}

fn write_wav_header(file: &mut File, data_len: u32, sample_rate: u32) -> io::Result<()> {
    let riff_len = 36u32
        .checked_add(data_len)
        .ok_or_else(|| invalid("song is too long for a WAV file"))?;
    let byte_rate = sample_rate
        .checked_mul(2)
        .ok_or_else(|| invalid("sample rate is too high for a WAV file"))?;

    file.write_all(b"RIFF")?;
    file.write_all(&riff_len.to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&1u16.to_le_bytes())?;
    file.write_all(&sample_rate.to_le_bytes())?;
    file.write_all(&byte_rate.to_le_bytes())?;
    file.write_all(&2u16.to_le_bytes())?;
    file.write_all(&16u16.to_le_bytes())?;
    file.write_all(b"data")?;
    file.write_all(&data_len.to_le_bytes())?;
    Ok(())
}

fn make_video(options: &Options, video: &str, song: &Song, label: &str) -> io::Result<()> {
    let output = ensure_extension(video, "mp4");
    let (width, height) = parse_video_size(&options.video_size)?;
    let ffmpeg = find_ffmpeg()?;

    let frames = total_frames(song.duration, SAMPLE_RATE);
    let audio_duration = frames as f64 / SAMPLE_RATE as f64;
    let segments = build_segments(&song.notes, song.seconds_per_tick);

    let wav_path = TempPath::new("wav");
    let ass_path = TempPath::new("ass");
    let log_path = TempPath::new("log");

    let interactive = io::stderr().is_terminal() && !options.no_progress;
    if interactive {
        eprintln!("Rendering audio...");
    }

    render_to_wav(&wav_path.0, song)?;

    let ass = build_terminal_ass(
        width,
        height,
        song.duration,
        audio_duration,
        song.notes.len(),
        &segments,
        !options.no_banner,
    );
    fs::write(&ass_path.0, ass)?;

    if interactive {
        eprintln!("Encoding video...");
    }

    run_ffmpeg(
        &ffmpeg,
        &output,
        &wav_path.0,
        &ass_path.0,
        &log_path.0,
        width,
        height,
        options.fps,
        audio_duration,
        label,
        interactive,
    )?;

    let size = fs::metadata(&output)?.len();
    println!(
        "Wrote {} ({:.1} MiB, {}, {}x{}, {} fps)",
        output.display(),
        size as f64 / 1_048_576.0,
        fmt_time(song.duration),
        width,
        height,
        options.fps
    );
    Ok(())
}

fn find_ffmpeg() -> io::Result<String> {
    let version = Command::new("ffmpeg").arg("-version").output();
    match version {
        Ok(result) if result.status.success() => {}
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "ffmpeg is required for --video; install ffmpeg with libass and libx264",
            ));
        }
    }

    let filters = Command::new("ffmpeg")
        .args(["-hide_banner", "-filters"])
        .output()
        .map_err(|error| io::Error::new(error.kind(), format!("cannot run ffmpeg: {error}")))?;
    let text = String::from_utf8_lossy(&filters.stdout);
    if !text
        .lines()
        .any(|line| line.split_whitespace().any(|word| word == "ass"))
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "this ffmpeg has no 'ass' filter; install a build with libass",
        ));
    }

    Ok("ffmpeg".to_string())
}

#[allow(clippy::too_many_arguments)]
fn run_ffmpeg(
    ffmpeg: &str,
    output: &Path,
    wav_path: &Path,
    ass_path: &Path,
    log_path: &Path,
    width: u32,
    height: u32,
    fps: u32,
    audio_duration: f64,
    label: &str,
    interactive: bool,
) -> io::Result<()> {
    let log = File::create(log_path)?;
    let color = format!(
        "color=c={}:s={}x{}:r={}:d={:.3}",
        VIDEO_BG, width, height, fps, audio_duration
    );
    let ass_filter = format!("ass={}", escape_filter_path(ass_path));
    let title = format!(
        "title=bleepmidi - {}",
        Path::new(label)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
    );

    let mut command = Command::new(ffmpeg);
    command
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i"])
        .arg(&color)
        .arg("-i")
        .arg(wav_path)
        .arg("-vf")
        .arg(&ass_filter)
        .args(["-map", "0:v:0", "-map", "1:a:0"])
        .arg("-metadata")
        .arg(&title)
        .args([
            "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-pix_fmt", "yuv420p",
        ])
        .args(["-c:a", "aac", "-b:a", "192k"])
        .args(["-shortest", "-movflags", "+faststart"])
        .args(["-progress", "pipe:1", "-nostats"])
        .arg(output);

    command.stdout(Stdio::piped()).stderr(Stdio::from(log));

    let mut child = command
        .spawn()
        .map_err(|error| io::Error::new(error.kind(), format!("cannot start ffmpeg: {error}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "ffmpeg stdout unavailable"))?;
    let reader = BufReader::new(stdout);
    let mut last_draw = Instant::now();

    for line in reader.lines() {
        let line = line?;
        if interactive {
            if let Some(when) = parse_ffmpeg_time(&line) {
                if last_draw.elapsed() >= Duration::from_millis(100) {
                    draw_encode_progress(when, audio_duration);
                    last_draw = Instant::now();
                }
            }
        }
    }

    let status = child.wait()?;
    if interactive {
        eprintln!();
    }

    if !status.success() {
        let text = fs::read_to_string(log_path).unwrap_or_default();
        let mut lines: Vec<&str> = text.lines().rev().take(12).collect();
        lines.reverse();
        if !lines.is_empty() {
            eprintln!("{}", lines.join("\n"));
        }
        return Err(io::Error::other("ffmpeg failed"));
    }

    Ok(())
}

fn parse_ffmpeg_time(line: &str) -> Option<f64> {
    if let Some(value) = line.strip_prefix("out_time_us=") {
        return value
            .trim()
            .parse::<f64>()
            .ok()
            .map(|micros| (micros / 1_000_000.0).max(0.0));
    }
    if let Some(value) = line.strip_prefix("out_time_ms=") {
        return value
            .trim()
            .parse::<f64>()
            .ok()
            .map(|micros| (micros / 1_000_000.0).max(0.0));
    }
    if let Some(value) = line.strip_prefix("out_time=") {
        let parts: Vec<&str> = value.trim().split(':').collect();
        if parts.len() == 3 {
            let hours = parts[0].parse::<f64>().ok()?;
            let minutes = parts[1].parse::<f64>().ok()?;
            let seconds = parts[2].parse::<f64>().ok()?;
            return Some((hours * 3600.0 + minutes * 60.0 + seconds).max(0.0));
        }
    }
    None
}

fn draw_encode_progress(when: f64, duration: f64) {
    let width = 28usize;
    let fraction = if duration > 0.0 {
        (when / duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let filled = (fraction * width as f64).round() as usize;
    let bar = "#".repeat(filled) + &"-".repeat(width - filled);
    eprint!(
        "\r\x1b[K encoding [{}] {:>3}%",
        bar,
        (fraction * 100.0).round() as u32
    );
    let _ = io::stderr().flush();
}

fn escape_filter_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace('\\', "\\\\")
        .replace(':', "\\:")
        .replace('\'', "\\'")
}

fn ensure_extension(path: &str, extension: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.extension().is_some() {
        path
    } else {
        path.with_extension(extension)
    }
}

fn parse_video_size(value: &str) -> io::Result<(u32, u32)> {
    let (width, height) = value
        .split_once('x')
        .or_else(|| value.split_once('X'))
        .ok_or_else(|| invalid("--video-size must look like 1280x720"))?;

    let mut width: u32 = width
        .parse()
        .map_err(|_| invalid("--video-size width must be a number"))?;
    let mut height: u32 = height
        .parse()
        .map_err(|_| invalid("--video-size height must be a number"))?;

    if width == 0 || height == 0 {
        return Err(invalid("--video-size values must be greater than zero"));
    }

    if width % 2 == 1 {
        width = width
            .checked_add(1)
            .ok_or_else(|| invalid("--video-size width is too large"))?;
    }
    if height % 2 == 1 {
        height = height
            .checked_add(1)
            .ok_or_else(|| invalid("--video-size height is too large"))?;
    }

    Ok((width, height))
}

fn terminal_metrics(width: u32, height: u32, rows: usize) -> (u32, i32, i32, f64) {
    let width_f = width as f64;
    let height_f = height as f64;
    let margin_x = (width_f * 0.02).round().max(4.0);
    let margin_y = (height_f * 0.02).round().max(4.0);
    let available_width = (width_f - 2.0 * margin_x).max(1.0);
    let available_height = (height_f - 2.0 * margin_y).max(1.0);

    let font = (available_width / (TERM_COLS as f64 * TERM_CELL_RATIO))
        .min(available_height / (rows as f64 * TERM_LINE_RATIO));
    let font = font.max(8.0).floor() as u32;

    let line_height = font as f64 * TERM_LINE_RATIO;
    let block_width = TERM_COLS as f64 * font as f64 * TERM_CELL_RATIO;
    let block_height = rows as f64 * line_height;
    let left = ((width_f - block_width) / 2.0).round().max(0.0) as i32;
    let top = ((height_f - block_height) / 2.0).round().max(0.0) as i32;

    (font, left, top, line_height)
}

fn ass_timestamp(seconds: f64) -> String {
    let centis = (seconds.max(0.0) * 100.0).round() as u64;
    let hours = centis / 360_000;
    let rest = centis % 360_000;
    let minutes = rest / 6_000;
    let rest = rest % 6_000;
    let secs = rest / 100;
    let centis = rest % 100;
    format!("{hours}:{minutes:02}:{secs:02}.{centis:02}")
}

fn ass_plain(text: &str) -> String {
    text.chars()
        .map(|character| match character {
            '\\' => '/',
            '{' => '(',
            '}' => ')',
            '\r' | '\n' => ' ',
            other => other,
        })
        .collect()
}

fn ass_terminal_text(text: &str) -> String {
    ass_plain(text)
        .chars()
        .map(|character| {
            if character == ' ' {
                '\u{00a0}'
            } else {
                character
            }
        })
        .collect()
}

fn ass_logo_line(line: &str) -> String {
    line.chars()
        .map(|character| match character {
            ' ' => '\u{00a0}',
            '{' => '(',
            '}' => ')',
            other => other,
        })
        .collect()
}

fn hard_wrap(text: &str, width: usize) -> Vec<String> {
    let characters: Vec<char> = text.chars().collect();
    if characters.is_empty() {
        return vec![String::new()];
    }
    characters
        .chunks(width)
        .map(|chunk| chunk.iter().collect())
        .collect()
}

fn build_terminal_ass(
    width: u32,
    height: u32,
    duration: f64,
    audio_duration: f64,
    note_count: usize,
    segments: &[Segment],
    show_banner: bool,
) -> String {
    let status = format!(
        "bleepmidi: playing {} notes · {} · {} Hz",
        note_count,
        fmt_time(duration),
        SAMPLE_RATE
    );
    let status_lines = hard_wrap(&status, TERM_COLS);
    let logo_lines: Vec<&str> = LOGO.lines().collect();
    let tagline_lines: Vec<&str> = if show_banner {
        vec![DEFAULT_TAGLINE]
    } else {
        Vec::new()
    };

    let mut row = 0usize;
    if show_banner {
        row += logo_lines.len();
        row += tagline_lines.len();
        row += 1;
    }
    let status_row = row;
    let progress_row = status_row + status_lines.len();
    let total_rows = TERM_ROWS.max(progress_row + 1);
    let (font, left, top, line_height) = terminal_metrics(width, height, total_rows);

    let mut events: Vec<(f64, f64, String)> = Vec::new();

    {
        let mut add = |start: f64, end: f64, at_row: usize, text: &str, logo: bool| {
            let end = end.min(audio_duration.max(0.01));
            if end <= start {
                return;
            }

            let escaped = if logo {
                ass_logo_line(text)
            } else {
                ass_terminal_text(text)
            };
            if escaped.is_empty() {
                return;
            }

            let y = (top as f64 + at_row as f64 * line_height).round() as i32;
            let style = format!(
                "{{\\an7\\pos({left},{y})\\fnDejaVu Sans Mono\\fs{font}\\b0\\bord0\\shad0\\c{ASS_TEXT}}}"
            );
            events.push((start, end, format!("{style}{escaped}")));
        };

        if show_banner {
            let mut banner_row = 0usize;
            for &line in &logo_lines {
                add(0.0, audio_duration, banner_row, line, true);
                banner_row += 1;
            }
            for &line in &tagline_lines {
                add(0.0, audio_duration, banner_row, line, false);
                banner_row += 1;
            }
        }

        for (status_index, line) in (status_row..).zip(status_lines.iter()) {
            add(0.0, audio_duration, status_index, line, false);
        }

        let mut when = 0.0;
        while when < audio_duration - 1e-9 {
            let end = (when + VIDEO_STEP).min(audio_duration);
            let note = note_at(segments, when);
            let line = progress_line(when, duration, note);
            add(when, end, progress_row, &line, false);
            when = end;
        }
    }

    let mut output = String::new();
    output.push_str("[Script Info]\n");
    output.push_str("Title: bleepmidi\n");
    output.push_str("ScriptType: v4.00+\n");
    output.push_str(&format!("PlayResX: {width}\n"));
    output.push_str(&format!("PlayResY: {height}\n"));
    output.push_str("WrapStyle: 2\n");
    output.push_str("ScaledBorderAndShadow: yes\n");
    output.push_str("YCbCr Matrix: TV.709\n\n");
    output.push_str("[V4+ Styles]\n");
    output.push_str(
        "Format: Name, Fontname, Fontsize, PrimaryColour, SecondaryColour, OutlineColour, BackColour, Bold, Italic, Underline, StrikeOut, ScaleX, ScaleY, Spacing, Angle, BorderStyle, Outline, Shadow, Alignment, MarginL, MarginR, MarginV, Encoding\n",
    );
    output.push_str(&format!(
        "Style: Mono,DejaVu Sans Mono,{font},&H00D9D1C9,&H00D9D1C9,&H00000000,&H00000000,0,0,0,0,100,100,0,0,1,0,0,7,0,0,0,1\n\n"
    ));
    output.push_str("[Events]\n");
    output.push_str(
        "Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n",
    );

    for (start, end, text) in events {
        output.push_str(&format!(
            "Dialogue: 0,{},{},Mono,,0,0,0,,{}\n",
            ass_timestamp(start),
            ass_timestamp(end),
            text
        ));
    }

    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo_song() -> Song {
        load_song(DEMO_MIDI).expect("demo parses")
    }

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn timestamps_match_ass() {
        assert_eq!(ass_timestamp(0.0), "0:00:00.00");
        assert_eq!(ass_timestamp(3661.23), "1:01:01.23");
    }

    #[test]
    fn video_size_parser() {
        assert_eq!(parse_video_size("1280x720").unwrap(), (1280, 720));
        assert_eq!(parse_video_size("641X359").unwrap(), (642, 360));
        assert!(parse_video_size("1280").is_err());
        assert!(parse_video_size("0x720").is_err());
    }

    #[test]
    fn demo_song_is_stable() {
        let song = demo_song();
        assert_eq!(song.notes.len(), 33);
        assert_eq!(fmt_time(song.duration), "00:16");
    }

    #[test]
    fn segments_pick_highest_note() {
        let notes = vec![
            Note {
                start: 0,
                end: 480,
                pitch: 60,
                velocity: 100,
            },
            Note {
                start: 0,
                end: 480,
                pitch: 64,
                velocity: 100,
            },
            Note {
                start: 480,
                end: 960,
                pitch: 67,
                velocity: 100,
            },
        ];
        let segments = build_segments(&notes, 0.5 / 480.0);
        assert_eq!(note_at(&segments, 0.1), Some(64));
        assert_eq!(note_at(&segments, 0.6), Some(67));
        assert_eq!(note_at(&segments, 2.0), None);
    }

    #[test]
    fn video_invocation_takes_output_first() {
        let options = parse_args(Invocation::Video, args(&["out.mp4", "--demo"])).unwrap();
        assert_eq!(options.video.as_deref(), Some("out.mp4"));
        assert!(options.demo);
    }

    #[test]
    fn video_invocation_rejects_second_video() {
        let result = parse_args(
            Invocation::Video,
            args(&["out.mp4", "--video", "other.mp4", "--demo"]),
        );
        assert!(result.is_err());
    }

    #[test]
    fn plain_invocation_parses_file_and_fps() {
        let options =
            parse_args(Invocation::Bleepmidi, args(&["song.mid", "--fps", "60"])).unwrap();
        assert_eq!(options.input.as_deref(), Some("song.mid"));
        assert_eq!(options.fps, 60);
    }

    #[test]
    fn terminal_ass_matches_current_ui() {
        let song = demo_song();
        let segments = build_segments(&song.notes, song.seconds_per_tick);
        let ass = build_terminal_ass(
            1280,
            720,
            song.duration,
            song.duration + 1.0,
            song.notes.len(),
            &segments,
            true,
        );
        let plain = ass.replace('\u{00a0}', " ");
        assert!(ass.contains("PlayResX: 1280"));
        assert!(ass.contains("DejaVu Sans Mono"));
        assert!(plain.contains("beeps & boops for your MIDI"));
        assert!(plain.contains(&format!("bleepmidi: playing {} notes", song.notes.len())));
        assert!(plain.contains("[------------------------]"));
        assert!(!plain.contains("NOW PLAYING"));
        assert!(!ass.contains("drawbox"));
    }
}
