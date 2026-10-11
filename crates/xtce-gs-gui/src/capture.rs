//! A picture of the window, taken by the window.
//!
//! # Why not a screen-capture utility
//!
//! The README shows what this station looks like with a real downlink in it, and a picture in
//! a README rots unless there is a command that produces it again. A screen grabber is not
//! that command: it needs the display server's permission — macOS refuses it outright to a
//! process that is not an application bundle, and returns the desktop picture with every
//! window missing rather than an error — it photographs whatever happens to be in front, and
//! it cannot be run by anyone who has not arranged their desktop the same way.
//!
//! Asking the window for its own framebuffer needs none of that. `egui` renders the frame it
//! was going to render anyway and hands the pixels back, so the file is the same on any
//! machine that can open the window at all, and regenerating it is one flag rather than a
//! procedure.
//!
//! # Why it waits
//!
//! Every plot is empty until packets have arrived, so a picture taken on the first frame shows
//! an empty station truthfully and uselessly. The delay is the caller's: it is how long the
//! source needs to fill the history that is worth looking at, which only the caller knows.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Where a picture of the window goes, and how long the station is given to fill first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Screenshot {
    /// The PNG to write. An existing file is replaced.
    pub path: PathBuf,
    /// How long to let the session run before the frame is taken.
    pub after: Duration,
}

/// How often to ask for a frame while waiting, when nothing else is driving repaints.
///
/// An operator moving a mouse produces frames; nobody is here. Without this the window would
/// repaint on the idle timer and the delay would be rounded up to it.
const WAITING_REPAINT: Duration = Duration::from_millis(100);

/// What the capture wants the caller to do next.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Next {
    /// The clock has not run out, or the frame has not come back yet.
    Waiting,
    /// The file is written, or writing it failed and was reported. Close the window.
    Done,
}

/// One pending screenshot.
pub(crate) struct Capture {
    request: Screenshot,
    opened: Instant,
    asked: bool,
}

impl Capture {
    /// Starts the clock.
    pub(crate) fn new(request: Screenshot) -> Self {
        Self {
            request,
            opened: Instant::now(),
            asked: false,
        }
    }

    /// Asks for the frame once the window has had its time, then writes what comes back.
    ///
    /// The request and the reply are a frame apart at best: `egui` reads the framebuffer after
    /// the frame it was asked during has been painted, and delivers it as an event on a later
    /// one. So this returns [`Next::Waiting`] at least twice.
    pub(crate) fn poll(&mut self, ctx: &egui::Context) -> Next {
        if !self.asked {
            // Wherever the pointer happens to be resting is a widget that will raise a
            // tooltip, and a tooltip in a README picture looks like part of the interface.
            // Nobody is here to hover deliberately, so nothing should be hovered at all.
            ctx.all_styles_mut(|style| style.interaction.tooltip_delay = f32::MAX);
            if self.opened.elapsed() < self.request.after {
                ctx.request_repaint_after(WAITING_REPAINT);
                return Next::Waiting;
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            self.asked = true;
            ctx.request_repaint();
            return Next::Waiting;
        }

        let frame = ctx.input(|input| {
            input.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(Arc::clone(image)),
                _ => None,
            })
        });
        let Some(frame) = frame else {
            ctx.request_repaint();
            return Next::Waiting;
        };

        // Reported on the terminal the flag was typed into, both ways round: this runs with
        // nobody watching the window, and a silent failure here is a stale README.
        match write_png(&self.request.path, &frame) {
            Ok(()) => println!(
                "screenshot: {} ({}x{})",
                self.request.path.display(),
                frame.size[0],
                frame.size[1]
            ),
            Err(reason) => eprintln!(
                "screenshot: {} was not written: {reason}",
                self.request.path.display()
            ),
        }
        Next::Done
    }
}

/// Writes one `egui` frame out as a PNG.
///
/// `egui`'s colours are premultiplied by their alpha. A window's own framebuffer is opaque, so
/// the multiplication was by one and the bytes go straight out; a translucent pixel would come
/// out darker than it looks, which is a trade this does not have to make.
fn write_png(path: &Path, frame: &egui::ColorImage) -> Result<(), String> {
    let [width, height] = frame.size;
    let width = u32::try_from(width).map_err(|_| format!("{width} pixels is too wide"))?;
    let height = u32::try_from(height).map_err(|_| format!("{height} pixels is too tall"))?;

    let mut bytes = Vec::with_capacity(frame.pixels.len() * 4);
    for pixel in &frame.pixels {
        bytes.extend_from_slice(&pixel.to_array());
    }

    let buffer = image::RgbaImage::from_raw(width, height, bytes).ok_or_else(|| {
        format!(
            "{width}x{height} does not match {} pixels",
            frame.pixels.len()
        )
    })?;
    buffer
        .save_with_format(path, image::ImageFormat::Png)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: usize, height: usize) -> egui::ColorImage {
        let pixels = vec![egui::Color32::from_rgb(0x1D, 0x5B, 0xA6); width * height];
        egui::ColorImage::new([width, height], pixels)
    }

    #[test]
    fn a_frame_is_written_as_a_png_of_the_same_size() {
        let path = std::env::temp_dir().join("xtce-gs-capture-size.png");
        write_png(&path, &frame(7, 3)).expect("a 7x3 frame is writable");

        let written = std::fs::read(&path).expect("the file is there");
        // The eight-byte PNG signature, then IHDR's width and height, big-endian.
        assert_eq!(&written[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&written[16..24], &[0, 0, 0, 7, 0, 0, 0, 3]);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_path_that_cannot_be_written_is_reported_and_not_a_panic() {
        // A directory that does not exist, which is what a mistyped flag looks like.
        let path = std::env::temp_dir().join("xtce-gs-no-such-directory/shot.png");
        let error = write_png(&path, &frame(2, 2)).expect_err("the directory is not there");
        assert!(!error.is_empty(), "the failure has to say something");
    }
}
