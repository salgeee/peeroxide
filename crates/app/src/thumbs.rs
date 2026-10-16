//! One still frame per source, the same capture the picker used to start on open.
//!
//! It starts when the app opens, off the UI thread. At most three at a time. A window
//! that has since closed is no longer in the source list; one that opened later is fetched
//! the same way.

use std::collections::{HashMap, HashSet};
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use peeroxide_capture::Source;

enum Thumb {
    /// `u64` is the generation that spawned this fetch. A newer pass ignores the old result.
    Pending(u64),
    Ready(ColorImage),
    Missing,
}

pub struct Thumbnails {
    generation: u64,
    cache: HashMap<String, Thumb>,
    textures: HashMap<String, TextureHandle>,
    queue: Vec<Source>,
    inflight: usize,
    tx: Sender<Message>,
    rx: Receiver<Message>,
}

struct Message {
    generation: u64,
    id: String,
    image: Result<ColorImage, String>,
}

impl Thumbnails {
    pub fn new() -> Self {
        let (tx, rx) = channel();
        Self {
            generation: 0,
            cache: HashMap::new(),
            textures: HashMap::new(),
            queue: Vec::new(),
            inflight: 0,
            tx,
            rx,
        }
    }

    /// Same fetch the picker used to run when it opened.
    pub fn begin(&mut self, sources: &[Source]) {
        self.generation = self.generation.wrapping_add(1);
        self.cache.clear();
        self.textures.clear();
        self.queue = sources.to_vec();
        // Jobs from a previous pass still finish; their generation no longer matches.
        self.inflight = 0;
        self.pump();
    }

    /// Keeps stills already taken and fetches only sources that do not have one yet.
    /// A failed capture is tried again. Sources no longer listed are dropped.
    pub fn ensure(&mut self, sources: &[Source]) {
        let wanted: HashSet<String> = sources.iter().map(|source| source.id()).collect();
        self.cache.retain(|id, _| wanted.contains(id));
        self.textures.retain(|id, _| wanted.contains(id));
        self.queue.retain(|source| wanted.contains(&source.id()));
        let queued: HashSet<String> = self.queue.iter().map(|source| source.id()).collect();
        for source in sources {
            let id = source.id();
            if queued.contains(&id)
                || matches!(
                    self.cache.get(&id),
                    Some(Thumb::Ready(_) | Thumb::Pending(_))
                )
            {
                continue;
            }
            self.cache.remove(&id);
            self.queue.push(source.clone());
        }
        self.pump();
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.rx.try_recv() {
            self.inflight = self.inflight.saturating_sub(1);
            let accept = matches!(
                self.cache.get(&message.id),
                Some(Thumb::Pending(expected)) if *expected == message.generation
            );
            if !accept {
                continue;
            }
            if let Err(error) = &message.image {
                tracing::debug!(%error, id = %message.id, "thumbnail unavailable");
            }
            let thumb = match message.image {
                Ok(image) if frame_visible(&image) => Thumb::Ready(image),
                _ => Thumb::Missing,
            };
            self.cache.insert(message.id, thumb);
            ctx.request_repaint();
        }
        self.pump();
        // The stills arrive on other threads. The channel is only read on a frame, and a
        // delayed wake does not keep arriving, so ask for the next frame while work remains.
        if self.inflight > 0 || !self.queue.is_empty() {
            ctx.request_repaint();
        }
    }

    /// The still for `id`, once it has arrived. `None` while it is loading or it failed.
    pub fn texture(&mut self, ctx: &egui::Context, id: &str) -> Option<TextureHandle> {
        if let Some(texture) = self.textures.get(id) {
            return Some(texture.clone());
        }
        let Thumb::Ready(image) = self.cache.get(id)? else {
            return None;
        };
        let texture =
            ctx.load_texture(format!("thumb-{id}"), image.clone(), TextureOptions::LINEAR);
        self.textures.insert(id.to_owned(), texture.clone());
        Some(texture)
    }

    fn pump(&mut self) {
        while self.inflight < 3 {
            let Some(source) = self.queue.pop() else {
                return;
            };
            let id = source.id();
            if self.cache.contains_key(&id) {
                continue;
            }
            let generation = self.generation;
            self.cache.insert(id.clone(), Thumb::Pending(generation));
            self.inflight += 1;
            let tx = self.tx.clone();
            let message_id = id.clone();
            if std::thread::Builder::new()
                .name("thumb".into())
                .spawn(move || {
                    let image = peeroxide_capture::snapshot(&source)
                        .map(to_image)
                        .map_err(|e| e.to_string());
                    let _ = tx.send(Message {
                        generation,
                        id: message_id,
                        image,
                    });
                })
                .is_err()
            {
                self.inflight = self.inflight.saturating_sub(1);
                self.cache.insert(id, Thumb::Missing);
            }
        }
    }
}

/// A fully transparent frame composites as nothing, so the card stays the empty well.
fn frame_visible(image: &ColorImage) -> bool {
    image.pixels.iter().any(|pixel| pixel.a() > 0)
}

fn to_image(frame: peeroxide_capture::Frame) -> ColorImage {
    let mut rgba = frame.data;
    for pixel in rgba.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &rgba)
}
