//! Simulated display driver
//!
//! This is an early attempt at a "frame buffer" style display driver. It uses the
//! embedded-graphics simulator crate to act as a display in simulated environments.
//!
//! This implementation is sort of a work in progress, it isn't really a *great*
//! long-term solution, but rather "okay for now".
//!
//! A framebuffer of pixels is allocated for the entire display on registration.
//! This could be, for example, 400x240 pixels.
//!
//! The driver will then allow for a certain number of "sub frames" to be requested.
//!
//! These sub frames could be for the entire display (400x240), or a portion of it,
//! for example 200x120 pixels.
//!
//! Clients of the driver can draw into the sub-frames that they receive, then send
//! them back to be rendered into the total frame. Any data in the client's sub-frame
//! will replace the current contents of the whole frame buffer.

use std::time::Duration;

use embedded_graphics::{
    image::{Image, ImageRaw},
    pixelcolor::Gray8,
    prelude::*,
};
use embedded_graphics_simulator::{
    BinaryColorTheme, OutputSettingsBuilder, SimulatorDisplay, SimulatorEvent, Window,
};
use maitake::sync::Mutex;
use mnemos_alloc::containers::{Arc, FixedVec};
use mnemos_kernel::{
    comms::kchannel::{KChannel, KConsumer},
    registry::Message,
    services::emb_display::{EmbDisplayService, FrameChunk, FrameError, Request, Response},
    Kernel,
};

/// Implements the [`EmbDisplayService`] driver using the `embedded-graphics`
/// simulator.
pub struct SimDisplay;

impl SimDisplay {
    /// Register the driver instance
    ///
    /// Registration will also start the simulated display, meaning that the display
    /// window will appear.
    #[tracing::instrument(skip(kernel))]
    pub async fn register(
        kernel: &'static Kernel,
        max_frames: usize,
        width: u32,
        height: u32,
    ) -> Result<(), FrameError> {
        tracing::debug!("initializing SimDisplay server ({width}x{height})...");
        let frames = FixedVec::new(max_frames).await;

        let (cmd_prod, cmd_cons) = KChannel::new_async(1).await.split();
        let commander = CommanderTask {
            kernel,
            cmd: cmd_cons,
            display_info: DisplayInfo {
                frames,
                frame_idx: 0,
            },
        };

        kernel.spawn(commander.run(width, height)).await;

        kernel
            .with_registry(|reg| reg.register_konly::<EmbDisplayService>(&cmd_prod))
            .await
            .map_err(|_| FrameError::DisplayAlreadyExists)?;

        tracing::info!("SimDisplayServer initialized!");

        Ok(())
    }
}

//////////////////////////////////////////////////////////////////////////////
// CommanderTask - This is the "driver server"
//////////////////////////////////////////////////////////////////////////////

/// This task is spawned by the call to [`SimDisplay::register`]. It is a single
/// async function that will process requests, and periodically redraw the
/// framebuffer.
struct CommanderTask {
    kernel: &'static Kernel,
    cmd: KConsumer<Message<EmbDisplayService>>,
    display_info: DisplayInfo,
}

struct Context {
    sdisp: SimulatorDisplay<Gray8>,
    window: Window,
    dirty: bool,
}

impl CommanderTask {
    /// The entrypoint for the driver execution
    async fn run(mut self, width: u32, height: u32) {
        let output_settings = OutputSettingsBuilder::new()
            .theme(BinaryColorTheme::OledBlue)
            .build();

        // Create a mutex for the embedded graphics simulator objects.
        //
        // We do this because if we don't call "update" regularly, the window just
        // sort of freezes. We also make the update loop check for "quit" events,
        // because otherwise the gui window just swallows all the control-c events,
        // which means you have to send a sigkill to actually get the simulator to
        // fully stop.
        //
        // The update loop *needs* to drop the egsim items, otherwise they just exist
        // in the mutex until the next time a frame is displayed, which right now is
        // only whenever line characters actually arrive.
        let sdisp = SimulatorDisplay::<Gray8>::new(Size::new(width, height));
        let window = Window::new("mnemOS", &output_settings);
        let mutex = Arc::new(Mutex::new(Some(Context {
            sdisp,
            window,
            dirty: true,
        })))
        .await;

        // Spawn a task that draws the framebuffer at a regular rate of 15Hz.
        self.kernel
            .spawn({
                let mutex = mutex.clone();
                async move {
                    let mut idle_ticks = 0;
                    loop {
                        self.kernel
                            .sleep(Duration::from_micros(1_000_000 / 15))
                            .await;
                        let mut guard = mutex.lock().await;
                        let mut done = false;
                        if let Some(Context {
                            sdisp,
                            window,
                            dirty,
                        }) = (&mut *guard).as_mut()
                        {
                            // If nothing has been drawn, only update the frame at 1Hz to save
                            // CPU usage
                            if *dirty || idle_ticks >= 15 {
                                idle_ticks = 0;
                                *dirty = false;
                                window.update(&sdisp);
                            } else {
                                idle_ticks += 1;
                            }

                            if window.events().any(|e| e == SimulatorEvent::Quit) {
                                done = true;
                            }
                        } else {
                            done = true;
                        }
                        if done {
                            let _ = guard.take();
                            break;
                        }
                    }
                }
            })
            .await;

        // This loop services incoming client requests.
        //
        // Generally, don't handle errors when replying to clients, this indicates that they
        // sent us a message and "hung up" without waiting for a response.
        loop {
            let msg = self.cmd.dequeue_async().await.map_err(drop).unwrap();
            let Message {
                msg: mut req,
                reply,
            } = msg;
            match &mut req.body {
                Request::NewFrameChunk {
                    start_x,
                    start_y,
                    width,
                    height,
                } => {
                    let res = self
                        .display_info
                        .new_frame(*start_x, *start_y, *width, *height)
                        .await
                        .map(Response::FrameChunkAllocated);

                    let resp = req.reply_with(res);

                    let _ = reply.reply_konly(resp).await;
                }
                Request::Draw(fc) => match self.display_info.remove_frame(fc.frame_id) {
                    Ok(_) => {
                        let (x, y) = (fc.start_x, fc.start_y);
                        let raw_img = frame_display(fc).unwrap();
                        let image = Image::new(&raw_img, Point::new(x, y));

                        let mut guard = mutex.lock().await;
                        if let Some(Context { sdisp, dirty, .. }) = (&mut *guard).as_mut() {
                            image.draw(sdisp).unwrap();
                            *dirty = true;

                            // Drop the guard before we reply so we don't hold it too long.
                            drop(guard);

                            let _ = reply
                                .reply_konly(req.reply_with(Ok(Response::FrameDrawn)))
                                .await;
                        } else {
                            break;
                        }
                    }
                    Err(e) => {
                        let _ = reply.reply_konly(req.reply_with(Err(e))).await;
                    }
                },
                Request::Drop(fc) => {
                    let _ = match self.display_info.remove_frame(fc.frame_id) {
                        Ok(_) => {
                            reply
                                .reply_konly(req.reply_with(Ok(Response::FrameDropped)))
                                .await
                        }
                        Err(e) => reply.reply_konly(req.reply_with(Err(e))).await,
                    };
                }
            }
        }
    }
}

/// Create and return a Simulator display object from raw pixel data.
///
/// Pixel data is turned into a raw image, and then drawn onto a SimulatorDisplay object
/// This is necessary as a e-g Window only accepts SimulatorDisplay object
/// On a physical display, the raw pixel data can be sent over to the display directly
/// Using the display's device interface
fn frame_display(fc: &mut FrameChunk) -> Result<ImageRaw<Gray8>, ()> {
    let raw_image: ImageRaw<Gray8>;
    raw_image = ImageRaw::<Gray8>::new(fc.bytes.as_slice(), fc.width);
    Ok(raw_image)
}

struct FrameInfo {
    frame: u16,
}

struct DisplayInfo {
    frame_idx: u16,
    frames: FixedVec<FrameInfo>,
}

impl DisplayInfo {
    // Returns a new frame chunk
    async fn new_frame(
        &mut self,
        start_x: i32,
        start_y: i32,
        width: u32,
        height: u32,
    ) -> Result<FrameChunk, FrameError> {
        let fidx = self.frame_idx;
        self.frame_idx = self.frame_idx.wrapping_add(1);

        self.frames
            .try_push(FrameInfo { frame: fidx })
            .map_err(|_| FrameError::NoFrameAvailable)?;

        let size = (width * height) as usize;

        // TODO: So, in the future, we might not want to ACTUALLY allocate here. Instead,
        // we might want to allocate ALL potential frame chunks at registration time and
        // hand those out, rather than requiring an allocation here.
        //
        // TODO: We might want to do ANY input checking here:
        //
        // * Making sure the request is smaller than the actual display
        // * Making sure the request exists entirely within the actual display
        let mut bytes = FixedVec::new(size).await;
        for _ in 0..size {
            let _ = bytes.try_push(0);
        }
        let fc = FrameChunk {
            frame_id: fidx,
            bytes,
            start_x,
            start_y,
            width,
            height,
        };

        Ok(fc)
    }

    fn remove_frame(&mut self, frame_id: u16) -> Result<(), FrameError> {
        let mut found = false;
        unsafe {
            // safety: This only removes items, and will not cause a realloc
            self.frames.as_vec_mut().retain(|fr| {
                let matches = fr.frame == frame_id;
                found |= matches;
                !matches
            });
        }
        if found {
            Ok(())
        } else {
            Err(FrameError::NoSuchFrame)
        }
    }
}
