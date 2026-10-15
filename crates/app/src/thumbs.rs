//! One still frame per source, fetched off the UI thread while the picker is open.
//!
//! A live thumbnail of every window would start a capture per card. One frame, at most three
//! at a time, is enough to tell the windows apart and then the capture stops.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};
use peeroxide_capture::Source;

enum Thumb {
    Pending,
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

    /// Drops whatever was cached and starts again for `sources`.
    pub fn begin(&mut self, sources: &[Source]) {
        self.generation = self.generation.wrapping_add(1);
        self.cache.clear();
        self.textures.clear();
        self.queue = sources.to_vec();
        // Jobs from the previous open still finish; their generation no longer matches, and a
        // few extra captures at the boundary are cheaper than cancelling a Graphics Capture session.
        self.inflight = 0;
        self.pump();
    }

    pub fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.cache.clear();
        self.textures.clear();
        self.queue.clear();
    }

    pub fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(message) = self.rx.try_recv() {
            self.inflight = self.inflight.saturating_sub(1);
            if message.generation == self.generation {
                self.cache.insert(
                    message.id,
                    match message.image {
                        Ok(image) => Thumb::Ready(image),
                        Err(error) => {
                            tracing::debug!(%error, "thumbnail unavailable");
                            Thumb::Missing
                        }
                    },
                );
                ctx.request_repaint();
            }
        }
        self.pump();
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
            self.cache.insert(id.clone(), Thumb::Pending);
            self.inflight += 1;
            let tx = self.tx.clone();
            let generation = self.generation;
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

fn to_image(frame: peeroxide_capture::Frame) -> ColorImage {
    let mut rgba = frame.data;
    for pixel in rgba.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    ColorImage::from_rgba_unmultiplied([frame.width as usize, frame.height as usize], &rgba)
}
