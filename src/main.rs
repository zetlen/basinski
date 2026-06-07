// SPDX-License-Identifier: GPL-3.0-or-later
//! basinski — rescues media files from their own disintegration.
//!
//! Named for William Basinski's *Disintegration Loops*: music made from tape
//! loops that crumbled as they played. This tool is for when your files do
//! the same thing, except you'd rather have them back.

mod aac;
mod divine;
mod ffx;
mod forensics;
mod gestalt;
mod h264;
mod mp4;
mod rescue;
mod transplant;

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand, ValueHint};

#[derive(Parser)]
#[command(
    name = "basinski",
    version,
    about = "Rescues media files from their own disintegration.",
    long_about = "basinski — rescues media files from their own disintegration.\n\n\
        Named for William Basinski's Disintegration Loops. Identifies audio and\n\
        video forensically even when headers are gone, regrows the missing\n\
        structure of head-truncated MP4s from the surviving index, clips video to\n\
        clean keyframes, and converts anything to The Correct Format."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Forensically identify a media file, headers or no headers.
    Identify {
        /// File to examine.
        #[arg(value_hint = ValueHint::FilePath)]
        file: PathBuf,
        /// Emit findings as JSON.
        #[arg(long)]
        json: bool,
    },

    /// Rescue a damaged media file (e.g. an MP4 whose header was cut off).
    Rescue {
        /// The casualty.
        #[arg(value_hint = ValueHint::FilePath)]
        file: PathBuf,
        /// Where to write the rescued file (default: <name>.rescued.<ext>).
        #[arg(short, long, value_hint = ValueHint::FilePath)]
        output: Option<PathBuf>,
        /// Also convert the result to The Correct Format.
        #[arg(long = "the-correct-format")]
        the_correct_format: bool,
        /// Keep damaged leading frames instead of clipping to a clean keyframe.
        #[arg(long)]
        no_clip: bool,
        /// Keep intermediate reconstruction files for inspection.
        #[arg(long)]
        keep_temp: bool,
        /// An intact file from the same device/encoder — the organ donor.
        /// When the casualty has no index at all (tail cut off, faststart
        /// front destroyed), basinski borrows this file's codec parameters
        /// and rebuilds the index from the raw media: a moov transplant.
        #[arg(long, value_name = "FILE", value_hint = ValueHint::FilePath)]
        reference: Option<PathBuf>,
        /// Override the video frame rate of a transplanted rescue. A stream
        /// with no parameter sets carries no timing, so the donor's rate is
        /// only a guess; set the real one here (e.g. 24, 25, 30) if playback
        /// comes out fast or slow. No re-divining needed.
        #[arg(long)]
        fps: Option<f64>,
        /// Skip raw-AAC audio salvage from the interleave gaps.
        #[arg(long)]
        no_audio: bool,
        /// Sample rate to assume for salvaged AAC audio (default 44100).
        #[arg(long, default_value_t = 44100)]
        audio_rate: u32,
    },

    /// Identify keyframes and clip video to valid ones — de-artifacting it.
    Clip {
        /// Artifacted but playable video.
        #[arg(value_hint = ValueHint::FilePath)]
        file: PathBuf,
        /// Where to write the clipped file (default: <name>.clipped.mp4).
        #[arg(short, long, value_hint = ValueHint::FilePath)]
        output: Option<PathBuf>,
        /// Clip from the first keyframe at or after this time (seconds).
        /// Without this, basinski searches for the first cleanly-decoding keyframe.
        #[arg(long)]
        from: Option<f64>,
        /// Just list the keyframes and exit.
        #[arg(long)]
        list: bool,
    },

    /// Divine the codec parameters of a headless stream and manufacture an
    /// organ donor for it — no intact sibling required. Candidate parameter
    /// sets are synthesized, the stream's own keyframe is decoded under each,
    /// and a picture-coherence score (plus an optional tiny image model)
    /// judges which guess produced actual borders and shapes.
    Divine {
        /// The headless casualty (an mdat payload with no index, no SPS/PPS).
        #[arg(value_hint = ValueHint::FilePath)]
        file: PathBuf,
        /// Where to write the donor (default: <name>.donor.mp4).
        #[arg(short, long, value_hint = ValueHint::FilePath)]
        output: Option<PathBuf>,
        /// Frame rate for the donor's timing tables. Decoding doesn't care;
        /// playback speed does.
        #[arg(long, default_value_t = 30)]
        fps: u32,
        /// ONNX image-classification model for the neural second opinion
        /// (also: $BASINSKI_MODEL, or mobilenetv2.onnx in ~/.cache/basinski/).
        #[arg(long, value_hint = ValueHint::FilePath)]
        model: Option<PathBuf>,
        /// Keep the candidate workshop directory for inspection.
        #[arg(long)]
        keep_temp: bool,
    },

    /// Convert any media to The Correct Format. There is only one.
    Convert {
        /// File to convert.
        #[arg(value_hint = ValueHint::FilePath)]
        file: PathBuf,
        /// Where to write it (default: <name>.mp4 or <name>.mp3).
        #[arg(short, long, value_hint = ValueHint::FilePath)]
        output: Option<PathBuf>,
        /// Consent to The Correct Format (mp4 for video, mp3 for audio).
        /// There are no other formats. This flag is the entire format menu.
        #[arg(long = "the-correct-format")]
        the_correct_format: bool,
    },

    /// Generate a shell completion script and print it to stdout.
    /// e.g. `basinski completions zsh > ~/.zfunc/_basinski`.
    Completions {
        /// Shell to generate for (zsh, bash, fish, powershell, elvish).
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitCode> {
    match cli.command {
        Command::Identify { file, json } => {
            let data = fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            let findings = forensics::identify(&data);
            if json {
                println!("{}", serde_json::to_string_pretty(&findings)?);
            } else if findings.is_empty() {
                println!("no recognizable media structure in {}", file.display());
                println!("(not even a frame sync — this may simply not be media)");
                return Ok(ExitCode::FAILURE);
            } else {
                println!("{} — {} bytes", file.display(), data.len());
                for f in &findings {
                    println!(
                        "  {:>3.0}%  {:<45} offset {:>8}  {}",
                        f.confidence * 100.0,
                        f.kind,
                        f.offset,
                        f.evidence
                    );
                }
                match ffx::probe(&file) {
                    Ok(Some(p)) => println!("  ffprobe corroborates: {}", p.summary()),
                    Ok(None) => {
                        println!("  ffprobe cannot read this file (forensics above is all we have)")
                    }
                    Err(e) => println!("  (ffprobe unavailable: {e})"),
                }
            }
            Ok(ExitCode::SUCCESS)
        }

        Command::Rescue {
            file,
            output,
            the_correct_format,
            no_clip,
            keep_temp,
            reference,
            fps,
            no_audio,
            audio_rate,
        } => {
            let opts = rescue::Options {
                correct_format: the_correct_format,
                no_clip,
                keep_temp,
                reference,
                fps,
                audio: !no_audio,
                audio_rate,
            };
            rescue::rescue(&file, output, &opts)?;
            Ok(ExitCode::SUCCESS)
        }

        Command::Clip {
            file,
            output,
            from,
            list,
        } => {
            let kfs = ffx::keyframes(&file)?;
            if kfs.is_empty() {
                anyhow::bail!("no video keyframes found in {}", file.display());
            }
            if list {
                println!("{} keyframes:", kfs.len());
                for (i, t) in kfs.iter().enumerate() {
                    println!("  [{i:>4}] {t:.3}s");
                }
                return Ok(ExitCode::SUCCESS);
            }
            let output = output.unwrap_or_else(|| {
                let stem = file.file_stem().unwrap_or_default().to_string_lossy();
                file.with_file_name(format!("{stem}.clipped.mp4"))
            });
            let t = match from {
                Some(want) => *kfs
                    .iter()
                    .find(|&&k| k >= want)
                    .with_context(|| format!("no keyframe at or after {want}s"))?,
                None => rescue::first_clean_keyframe(&file, &output)?,
            };
            println!("✂ clipping {} from keyframe at {t:.3}s", file.display());
            ffx::clip_from_keyframe(&file, &output, t)?;
            println!("✔ {}", output.display());
            Ok(ExitCode::SUCCESS)
        }

        Command::Divine {
            file,
            output,
            fps,
            model,
            keep_temp,
        } => {
            let opts = divine::Options {
                fps,
                keep_temp,
                model,
                output,
            };
            divine::divine(&file, &opts)?;
            Ok(ExitCode::SUCCESS)
        }

        Command::Convert {
            file,
            output,
            the_correct_format,
        } => {
            if !the_correct_format {
                eprintln!("basinski converts media to The Correct Format and nothing else.");
                eprintln!("The Correct Format is mp4 (H.264 + AAC) for video and mp3 for audio.");
                eprintln!("This was decided long ago and is no longer open for discussion.");
                eprintln!();
                eprintln!("Pass --the-correct-format to proceed.");
                return Ok(ExitCode::from(2));
            }
            let probe = ffx::probe(&file)?.with_context(|| {
                format!(
                    "{} is not probeable media; try `rescue` first",
                    file.display()
                )
            })?;
            let video = probe.has_video();
            if !video && !probe.has_audio() {
                anyhow::bail!("no audio or video streams found; nothing to convert");
            }
            let ext = if video { "mp4" } else { "mp3" };
            let output = output.unwrap_or_else(|| file.with_extension(ext));
            if output == file {
                anyhow::bail!(
                    "{} is already named like The Correct Format; refusing to overwrite the input",
                    file.display()
                );
            }
            println!(
                "♻ {} → The Correct Format ({})",
                file.display(),
                if video { "mp4: H.264 + AAC" } else { "mp3" }
            );
            ffx::to_correct_format(&file, &output, video)?;
            let errors = ffx::decode_errors(&output)?;
            println!("✔ {} ({} decode errors)", output.display(), errors);
            Ok(ExitCode::SUCCESS)
        }

        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
            Ok(ExitCode::SUCCESS)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// clap's own validator: catches malformed args, conflicting value hints,
    /// duplicate flags, etc. at test time rather than at first run.
    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    /// The zsh completion must generate and carry the structure that makes it
    /// a real `#compdef` script with our subcommands in it.
    #[test]
    fn zsh_completions_generate() {
        let mut cmd = Cli::command();
        let mut out = Vec::new();
        clap_complete::generate(clap_complete::Shell::Zsh, &mut cmd, "basinski", &mut out);
        let script = String::from_utf8(out).expect("zsh completion is valid UTF-8");
        assert!(
            script.contains("#compdef basinski"),
            "missing compdef header"
        );
        for sub in ["identify", "rescue", "clip", "divine", "convert"] {
            assert!(
                script.contains(sub),
                "completion missing subcommand `{sub}`"
            );
        }
        // Richness: path args advertise file completion via _files.
        assert!(script.contains("_files"), "no file-path completion emitted");
    }
}
