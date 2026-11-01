// SPDX-License-Identifier: AGPL-3.0-or-later

//! `bleepmidi-video OUTPUT.mp4 [options] <file.mid>`
//!
//! A thin alias for `bleepmidi --video OUTPUT.mp4 ...`.

fn main() {
    bleepmidi::main(bleepmidi::Invocation::Video);
}
