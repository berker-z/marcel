//! A byte-budgeted image cache for the `img(path)` elements.
//!
//! Without a cache element above it, `img(path)` falls back to GPUI's global
//! asset table, which only ever grows: every decoded image stays in RAM for
//! the life of the process and its atlas tiles are never released. That is
//! fine for a handful of icons and ruinous for PDF pages and thumbnails. This
//! cache is GPUI's `RetainAllImageCache` with a budget: images are accounted
//! by decoded size, the least recently drawn is evicted first, and eviction
//! frees both the pixels and the atlas tiles.
//!
//! `img(Arc<RenderImage>)` never consults a cache; those images are the
//! preview's own and `PreviewState` releases them itself.

use std::{collections::HashMap, hash::Hash, sync::Arc};

use gpui::{
    App, AppContext as _, Asset as _, AssetLogger, Entity, ImageAssetLoader, ImageCache,
    ImageCacheError, RenderImage, Resource, Task, WeakEntity, Window, hash,
};

/// Byte accounting and recency for a set of keys. Everything the cache
/// decides is decided here, and nothing here needs a window.
struct Ledger<K> {
    budget: usize,
    used: usize,
    /// Bytes charged and the tick of the last touch, per key.
    entries: HashMap<K, (usize, u64)>,
    clock: u64,
}

impl<K: Hash + Eq + Clone> Ledger<K> {
    fn new(budget: usize) -> Self {
        Self { budget, used: 0, entries: HashMap::new(), clock: 0 }
    }

    fn tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// Mark `key` as just used.
    fn touch(&mut self, key: &K) {
        let tick = self.tick();
        if let Some((_, last_used)) = self.entries.get_mut(key) {
            *last_used = tick;
        }
    }

    /// Charge `bytes` to `key` and return whichever other keys have to go to
    /// stay within budget, least recently used first. The key just charged is
    /// never among them: an image bigger than the whole budget is still the
    /// one being drawn.
    fn charge(&mut self, key: K, bytes: usize) -> Vec<K> {
        let tick = self.tick();
        if let Some((previous, _)) = self.entries.insert(key.clone(), (bytes, tick)) {
            self.used -= previous;
        }
        self.used += bytes;

        let mut evicted = Vec::new();
        while self.used > self.budget {
            let Some(victim) = self
                .entries
                .iter()
                .filter(|(candidate, _)| **candidate != key)
                .min_by_key(|(_, (_, last_used))| *last_used)
                .map(|(candidate, _)| candidate.clone())
            else {
                break;
            };
            self.release(&victim);
            evicted.push(victim);
        }
        evicted
    }

    fn release(&mut self, key: &K) -> Option<usize> {
        let (bytes, _) = self.entries.remove(key)?;
        self.used -= bytes;
        Some(bytes)
    }
}

type Decoded = Result<Arc<RenderImage>, ImageCacheError>;

enum Item {
    /// Dropping the task abandons the decode, so an image evicted or
    /// invalidated while still loading costs nothing further.
    Loading {
        _decode: Task<()>,
    },
    Loaded(Decoded),
}

/// The cache proper: one `Item` per resource under a `Ledger`.
pub struct BoundedImageCache {
    this: WeakEntity<Self>,
    items: HashMap<u64, Item>,
    ledger: Ledger<u64>,
}

impl BoundedImageCache {
    /// A cache that keeps at most about `budget` bytes of decoded pixels.
    pub fn new(budget: usize, cx: &mut App) -> Entity<Self> {
        let cache = cx.new(|cx| Self {
            this: cx.weak_entity(),
            items: HashMap::new(),
            ledger: Ledger::new(budget),
        });
        // The window is gone by the time its view releases this, so there is
        // no current window to name; every other window's atlas is cleared.
        cx.observe_release(&cache, |cache, cx| {
            for (_, item) in std::mem::take(&mut cache.items) {
                if let Item::Loaded(Ok(image)) = item {
                    cx.drop_image(image, None);
                }
            }
        })
        .detach();
        cache
    }

    /// Forget `source`, freeing its pixels and atlas tiles. The next `img`
    /// for it decodes afresh, which is the point when the file behind the
    /// path has changed.
    pub fn remove(&mut self, source: &Resource, window: &mut Window, cx: &mut App) {
        self.evict(hash(source), window, cx);
    }

    fn evict(&mut self, key: u64, window: &mut Window, cx: &mut App) {
        self.ledger.release(&key);
        if let Some(Item::Loaded(Ok(image))) = self.items.remove(&key) {
            cx.drop_image(image, Some(window));
        }
    }

    /// Store a finished decode and evict whatever no longer fits.
    fn settle(&mut self, key: u64, decoded: Decoded, window: &mut Window, cx: &mut App) {
        // Removed while decoding: the task was dropped, and a result that
        // still arrived belongs to nobody.
        if !matches!(self.items.get(&key), Some(Item::Loading { .. })) {
            return;
        }
        // A failure holds nothing worth counting, but it does hold its place
        // so the failing path is not decoded again every frame.
        let bytes = decoded.as_ref().map(|image| decoded_bytes(image)).unwrap_or(0);
        self.items.insert(key, Item::Loaded(decoded));
        for evicted in self.ledger.charge(key, bytes) {
            self.evict(evicted, window, cx);
        }
    }
}

impl ImageCache for BoundedImageCache {
    fn load(&mut self, resource: &Resource, window: &mut Window, cx: &mut App) -> Option<Decoded> {
        let key = hash(resource);
        if let Some(item) = self.items.get(&key) {
            self.ledger.touch(&key);
            return match item {
                Item::Loaded(decoded) => Some(decoded.clone()),
                Item::Loading { .. } => None,
            };
        }

        // Like `RetainAllImageCache`, redraw the view once the image is in.
        // Unlike it, account the image the moment it lands rather than the
        // next time something asks for it: a page the user scrolled past
        // before it decoded would otherwise sit outside the budget for good.
        let load = AssetLogger::<ImageAssetLoader>::load(resource.clone(), cx);
        let decode = cx.background_executor().spawn(load);
        let view = window.current_view();
        let cache = self.this.clone();
        let task = window.spawn(cx, async move |cx| {
            let decoded = decode.await;
            let _ = cx.update(|window, cx| {
                let _ = cache.update(cx, |cache, cx| cache.settle(key, decoded, window, cx));
                cx.notify(view);
            });
        });
        self.items.insert(key, Item::Loading { _decode: task });
        None
    }
}

/// What `image` costs in memory: BGRA, every frame.
fn decoded_bytes(image: &RenderImage) -> usize {
    (0..image.frame_count())
        .map(|frame| {
            let size = image.size(frame);
            size.width.0.max(0) as usize * size.height.0.max(0) as usize * 4
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys_by_recency(ledger: &Ledger<u64>) -> Vec<u64> {
        let mut keys =
            ledger.entries.iter().map(|(key, (_, tick))| (*tick, *key)).collect::<Vec<_>>();
        keys.sort_unstable();
        keys.into_iter().map(|(_, key)| key).collect()
    }

    #[test]
    fn charging_past_the_budget_evicts_the_least_recently_used_first() {
        let mut ledger = Ledger::new(100);
        assert!(ledger.charge(1, 40).is_empty());
        assert!(ledger.charge(2, 40).is_empty());
        ledger.touch(&1);

        // 2 is now the oldest; 3 pushes usage to 120, so 2 alone goes.
        assert_eq!(ledger.charge(3, 40), vec![2]);
        assert_eq!(ledger.used, 80);
        assert_eq!(keys_by_recency(&ledger), vec![1, 3]);
    }

    #[test]
    fn eviction_keeps_going_until_the_budget_is_met() {
        let mut ledger = Ledger::new(100);
        ledger.charge(1, 30);
        ledger.charge(2, 30);
        ledger.charge(3, 30);
        assert_eq!(ledger.charge(4, 90), vec![1, 2, 3]);
        assert_eq!(ledger.used, 90);
    }

    #[test]
    fn an_image_over_the_whole_budget_is_kept_and_everything_else_goes() {
        let mut ledger = Ledger::new(100);
        ledger.charge(1, 10);
        assert_eq!(ledger.charge(2, 500), vec![1]);
        assert_eq!(ledger.used, 500);
        assert!(ledger.entries.contains_key(&2));
    }

    #[test]
    fn recharging_a_key_replaces_its_bytes_rather_than_adding() {
        let mut ledger = Ledger::new(100);
        ledger.charge(1, 60);
        assert!(ledger.charge(1, 30).is_empty());
        assert_eq!(ledger.used, 30);
    }

    #[test]
    fn releasing_refunds_the_bytes_and_unknown_keys_are_ignored() {
        let mut ledger = Ledger::new(100);
        ledger.charge(1, 60);
        assert_eq!(ledger.release(&1), Some(60));
        assert_eq!(ledger.release(&1), None);
        assert_eq!(ledger.used, 0);
    }

    #[test]
    fn decoded_size_counts_every_frame_in_bgra() {
        let frames = vec![
            image::Frame::new(image::RgbaImage::new(4, 2)),
            image::Frame::new(image::RgbaImage::new(3, 3)),
        ];
        assert_eq!(decoded_bytes(&RenderImage::new(frames)), (4 * 2 + 3 * 3) * 4);
    }
}
