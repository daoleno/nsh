//! AppKit drops of files, routed by the same hit testing as pointer input.
//!
//! A drag offers local files (Finder: their URLs), files another app
//! promises to write (an image dragged out of a browser, the screenshot
//! thumbnail, a Mail attachment: `NSFilePromiseReceiver`) or image data. A
//! drop hands each file over with its contents ([`nsh_core::FileContents`]),
//! read on a thread of its own as a pick's are, beside the local files'
//! URLs: the app reads what the user dropped without an `fs` grant.
use std::{
    cell::RefCell,
    path::PathBuf,
    ptr::NonNull,
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use block2::RcBlock;
use nsh_core::{
    EventKind, EventPayload, FileContents,
    picker::{PickedFile, read_dropped_files},
};
use objc2::{
    ClassType,
    rc::Retained,
    runtime::{AnyClass, ProtocolObject},
};
use objc2_app_kit::{NSDragOperation, NSDraggingInfo, NSFilePromiseReceiver, NSPasteboard, NSView};
use objc2_foundation::{NSArray, NSDictionary, NSError, NSOperationQueue, NSString, NSURL};

use crate::{input, surface::Surface, view::NshView};

#[derive(Default)]
pub(crate) struct State {
    target: RefCell<Option<NshView>>,
    offer: RefCell<Offer>,
    destinations: RefCell<Destinations>,
}

/// What a drag offers: local files' URLs, files another app promises, or
/// image data.
#[derive(Default, Clone)]
struct Offer {
    uris: Vec<String>,
    promises: bool,
    image: bool,
}

impl Offer {
    fn any(&self) -> bool {
        !self.uris.is_empty() || self.promises || self.image
    }

    /// Each offered item's MIME type: the local files' by their names; an
    /// image's or a promise's (no URL) as the drag says.
    fn mime_types(&self) -> Vec<String> {
        if !self.uris.is_empty() {
            return self
                .uris
                .iter()
                .map(|uri| nsh_core::platform::file_mime_type(uri).to_owned())
                .collect();
        }
        if self.image {
            return vec!["image/png".into()];
        }
        Vec::new()
    }

    /// What drag entry tells the app: the offer, nothing read.
    fn entered(&self) -> EventPayload {
        EventPayload::FileContents(Box::new(
            FileContents::new(Vec::new(), self.mime_types(), None).with_uris(self.uris.clone()),
        ))
    }
}

/// Image data a drag can carry, in the order taken: the type, the name
/// its file gets, and its MIME type.
const DRAGGED_IMAGES: [(&str, &str, &str); 3] = [
    ("public.png", "image.png", "image/png"),
    ("public.jpeg", "image.jpg", "image/jpeg"),
    ("public.tiff", "image.png", "image/png"),
];

/// How long a drop waits for the files another app promised (a browser may
/// download the image first).
const PROMISE_WAIT: Duration = Duration::from_secs(60);

#[derive(Default)]
struct Destinations {
    /// The views that take drops, by id (a table that follows the views
    /// alive, not every id made).
    enabled: nsh_core::id_table::IdTable<()>,
}
impl Destinations {
    fn set(&mut self, id: nsh_core::NodeId, enabled: bool) -> Option<bool> {
        let index = id as usize;
        if self.enabled.contains(index) == enabled {
            return None;
        }
        let before = self.count() != 0;
        if enabled {
            self.enabled.insert(index, ());
        } else {
            self.enabled.remove(index);
        }
        let after = self.count() != 0;
        (before != after).then_some(after)
    }

    fn count(&self) -> usize {
        self.enabled.len()
    }
}

pub(crate) fn events_changed(surface: &Surface, view: &NshView) {
    let enabled = view.state().events.contains(EventKind::FileDrop);
    destination_changed(surface, view.id(), enabled);
}

pub(crate) fn deleted(surface: &Surface, id: nsh_core::NodeId) {
    destination_changed(surface, id, false);
}

fn destination_changed(surface: &Surface, id: nsh_core::NodeId, enabled: bool) {
    let registration = surface.file_drop.destinations.borrow_mut().set(id, enabled);
    match registration {
        Some(true) => {
            register(surface.root.ns());
            for root in crate::window::roots(surface) {
                register(root.ns());
            }
        }
        Some(false) => {
            surface.root.ns().unregisterDraggedTypes();
            for root in crate::window::roots(surface) {
                root.ns().unregisterDraggedTypes();
            }
        }
        None => {}
    }
    if !enabled {
        invalidate(surface, id);
    }
}

pub(crate) fn reset(surface: &Surface) {
    clear(surface);
    if surface.file_drop.destinations.borrow().count() > 0 {
        surface.root.ns().unregisterDraggedTypes();
    }
    *surface.file_drop.destinations.borrow_mut() = Destinations::default();
}

/// Whether a view of the surface takes file drops: a window's root view
/// made later registers then ([`crate::window`]).
pub(crate) fn accepting(surface: &Surface) -> bool {
    surface.file_drop.destinations.borrow().count() > 0
}

pub(crate) fn register(view: &NSView) {
    let mut kinds: Vec<Retained<NSString>> = std::iter::once("public.file-url")
        .chain(DRAGGED_IMAGES.iter().map(|(kind, ..)| *kind))
        .map(NSString::from_str)
        .collect();
    kinds.extend(NSFilePromiseReceiver::readableDraggedTypes().to_vec());
    view.registerForDraggedTypes(&NSArray::from_retained_slice(&kinds));
}

/// The operation a drop answers the drag's source with: a copy, or the
/// source's own choice where it offers no copy (`Generic`); none for a
/// source that only moves (the drop would take its file away).
fn operation(sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
    let mask = sender.draggingSourceOperationMask();
    if mask.contains(NSDragOperation::Copy) {
        NSDragOperation::Copy
    } else if mask.contains(NSDragOperation::Generic) {
        NSDragOperation::Generic
    } else {
        NSDragOperation::None
    }
}

fn target(surface: &Surface, sender: &ProtocolObject<dyn NSDraggingInfo>) -> Option<NshView> {
    if operation(sender) == NSDragOperation::None {
        return None;
    }
    let point = input::event_root(surface)
        .ns()
        .convertPoint_fromView(sender.draggingLocation(), None);
    input::file_drop_target(surface, point.x as f32, point.y as f32)
}

fn set_target(surface: &Surface, target: Option<NshView>) {
    let mut current = surface.file_drop.target.borrow_mut();
    if current.as_ref().map(NshView::id) == target.as_ref().map(NshView::id) {
        return;
    }
    if let Some(previous) = current.take()
        && previous.state().events.contains(EventKind::FileDragLeave)
    {
        surface.event(previous.id(), EventKind::FileDragLeave, EventPayload::None);
    }
    if let Some(next) = &target
        && next.state().events.contains(EventKind::FileDragEnter)
    {
        let entered = surface.file_drop.offer.borrow().entered();
        surface.event(next.id(), EventKind::FileDragEnter, entered);
    }
    *current = target;
}

pub(crate) fn clear(surface: &Surface) {
    *surface.file_drop.offer.borrow_mut() = Offer::default();
    set_target(surface, None);
}

pub(crate) fn invalidate(surface: &Surface, id: nsh_core::NodeId) {
    let matches = surface
        .file_drop
        .target
        .borrow()
        .as_ref()
        .is_some_and(|view| view.id() == id);
    if matches {
        set_target(surface, None);
    }
}

pub(crate) fn begin(
    surface: &Surface,
    sender: &ProtocolObject<dyn NSDraggingInfo>,
) -> NSDragOperation {
    *surface.file_drop.offer.borrow_mut() = read_offer(&sender.draggingPasteboard());
    update(surface, sender)
}

pub(crate) fn update(
    surface: &Surface,
    sender: &ProtocolObject<dyn NSDraggingInfo>,
) -> NSDragOperation {
    let next = if surface.file_drop.offer.borrow().any() {
        target(surface, sender)
    } else {
        None
    };
    let answer = if next.is_some() {
        operation(sender)
    } else {
        NSDragOperation::None
    };
    set_target(surface, next);
    answer
}

pub(crate) fn perform(surface: &Surface, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
    if operation(sender) == NSDragOperation::None {
        clear(surface);
        return false;
    }
    let point = input::event_root(surface)
        .ns()
        .convertPoint_fromView(sender.draggingLocation(), None);
    let board = sender.draggingPasteboard();
    // A file that went (or that the system stopped this app from reading)
    // since the drag entered is left off the pasteboard now: the drop is
    // what entered, so the app hears of the file and can say it could not
    // be read.
    let offer = Some(read_offer(&board))
        .filter(Offer::any)
        .unwrap_or_else(|| surface.file_drop.offer.borrow().clone());
    deliver(surface, point.x as f32, point.y as f32, offer, Some(&board))
}

/// Drops `uris` at `x`, `y` (the event root's coordinates) on the view
/// there that listens: whether one took them. The test agent's `dropFiles`
/// comes here too.
#[cfg(feature = "dev")]
pub(crate) fn drop_uris(surface: &Surface, x: f32, y: f32, uris: Vec<String>) -> bool {
    let offer = Offer {
        uris,
        ..Offer::default()
    };
    deliver(surface, x, y, offer, None)
}

/// Drops what `offer` holds at `x`, `y` on the view there that listens:
/// whether one took it. The drag ends now (`onFileDragLeave`); the drop
/// (`onFileDrop`) follows from a thread once the files are read: the local
/// files at once, promised ones once their app has written them (a
/// temporary folder, removed after), else the image data `board` holds.
/// The drop comes even when nothing could be read, with the URLs, so the
/// app can say so.
fn deliver(surface: &Surface, x: f32, y: f32, offer: Offer, board: Option<&NSPasteboard>) -> bool {
    let Some(destination) = input::file_drop_target(surface, x, y) else {
        clear(surface);
        return false;
    };
    let Some(control) = surface.event_control().filter(|_| offer.any()) else {
        clear(surface);
        return false;
    };
    *surface.file_drop.offer.borrow_mut() = offer.clone();
    set_target(surface, Some(destination.clone()));
    clear(surface);
    let paths: Vec<PathBuf> = offer.uris.iter().filter_map(|uri| file_path(uri)).collect();
    let (promised, image) = match board {
        Some(board) if paths.is_empty() => (
            offer.promises.then(|| receive_promises(board)).flatten(),
            offer.image.then(|| image_data(board)).flatten(),
        ),
        _ => (None, None),
    };
    let id = destination.id();
    let mime_types = offer.mime_types();
    let uris = offer.uris;
    let spawned = std::thread::Builder::new()
        .name("nsh-file-drop".into())
        .spawn(move || {
            let mut files = read_dropped_files(&paths);
            if let Some(promised) = promised {
                files.extend(promised.wait());
            }
            if files.is_empty()
                && let Some(image) = image
            {
                files.push(image);
            }
            let drop = FileContents::new(files, mime_types, None).with_uris(uris);
            control.dispatch_event(
                id,
                EventKind::FileDrop,
                EventPayload::FileContents(Box::new(drop)),
            );
        });
    if let Err(e) = spawned {
        log::warn!("cannot read the dropped files: {e}");
    }
    true
}

/// A drag entering an input's text view (a text area, a field's editor).
/// AppKit offers a drag to the deepest view registered for one of its
/// types, and a text view registers for files and text: files, promised
/// files and images over an input inside a file drop target go to that
/// target, as anywhere else in it (`Some`), even when the drag also
/// carries text (a browser's image carries its address); any other drag
/// stays the text view's (`None`), text drags included.
pub(crate) fn text_entered(
    surface: &Surface,
    sender: &ProtocolObject<dyn NSDraggingInfo>,
) -> Option<NSDragOperation> {
    Some(begin(surface, sender)).filter(|operation| *operation != NSDragOperation::None)
}

/// A drag moving over an input's text view: [`text_entered`].
pub(crate) fn text_updated(
    surface: &Surface,
    sender: &ProtocolObject<dyn NSDraggingInfo>,
) -> Option<NSDragOperation> {
    if !surface.file_drop.offer.borrow().any() {
        return None;
    }
    Some(update(surface, sender)).filter(|operation| *operation != NSDragOperation::None)
}

/// A drop on an input's text view: whether the target around it took the
/// files (`Some`), or `None` for the text view to handle ([`text_entered`]).
pub(crate) fn text_performed(
    surface: &Surface,
    sender: &ProtocolObject<dyn NSDraggingInfo>,
) -> Option<bool> {
    let offered = surface.file_drop.offer.borrow().any();
    (offered && target(surface, sender).is_some()).then(|| perform(surface, sender))
}

/// What the drag pasteboard `board` offers. Only types are read for
/// promises and images (no data until the drop).
fn read_offer(board: &NSPasteboard) -> Offer {
    let types = board.types();
    let has = |kind: &NSString| types.as_ref().is_some_and(|t| t.containsObject(kind));
    Offer {
        uris: read_uris(board),
        promises: NSFilePromiseReceiver::readableDraggedTypes()
            .iter()
            .any(|kind| has(&kind)),
        image: DRAGGED_IMAGES
            .iter()
            .any(|(kind, ..)| has(&NSString::from_str(kind))),
    }
}

fn read_uris(board: &NSPasteboard) -> Vec<String> {
    let kind = NSString::from_str("public.file-url");
    board
        .pasteboardItems()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.stringForType(&kind))
                .filter_map(|uri| item_file_uri(&uri))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
}

/// The local path of `file:` URL `uri`.
pub(crate) fn file_path(uri: &str) -> Option<PathBuf> {
    let url = NSURL::URLWithString(&NSString::from_str(uri))?;
    url.isFileURL()
        .then(|| url.path())
        .flatten()
        .map(|path| PathBuf::from(path.to_string()))
}

/// The first image `board` holds, as a dropped file (a TIFF as PNG).
fn image_data(board: &NSPasteboard) -> Option<PickedFile> {
    DRAGGED_IMAGES.iter().find_map(|(kind, name, mime)| {
        let bytes = board.dataForType(&NSString::from_str(kind))?.to_vec();
        let bytes = if *kind == "public.tiff" {
            crate::system::to_png(&bytes)?
        } else {
            bytes
        };
        Some(PickedFile::new(
            *name,
            *mime,
            nsh_core::ImageData::new(bytes),
        ))
    })
}

/// Files another app promised, being written into a temporary folder.
struct Promised {
    written: mpsc::Receiver<Option<PathBuf>>,
    expected: usize,
    dir: PathBuf,
    _alive: Receiving,
}

/// The promises and the queue their files are written on, kept until the
/// drop is read.
struct Receiving {
    _receivers: Vec<Retained<NSFilePromiseReceiver>>,
    _queue: Retained<NSOperationQueue>,
}

// SAFETY: the thread that reads the drop only releases these (an
// `objc_release`, safe on any thread); they are messaged on the main
// thread and AppKit's operation queue only.
unsafe impl Send for Receiving {}

/// Asks `board`'s file promises to write their files into a temporary
/// folder of NSH's (`receivePromisedFilesAtDestination:`).
fn receive_promises(board: &NSPasteboard) -> Option<Promised> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let class: &AnyClass = NSFilePromiseReceiver::class();
    // SAFETY: an array of a class that adopts NSPasteboardReading, no options.
    let objects =
        unsafe { board.readObjectsForClasses_options(&NSArray::from_slice(&[class]), None) }?;
    let receivers: Vec<Retained<NSFilePromiseReceiver>> = objects
        .to_vec()
        .into_iter()
        .filter_map(|object| object.downcast::<NSFilePromiseReceiver>().ok())
        .collect();
    if receivers.is_empty() {
        return None;
    }
    let dir = std::env::temp_dir().join(format!(
        "nsh-drop-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir)
        .inspect_err(|e| log::warn!("cannot make {}: {e}", dir.display()))
        .ok()?;
    let destination = NSURL::fileURLWithPath(&NSString::from_str(&dir.to_string_lossy()));
    let queue = NSOperationQueue::new();
    let (tx, written) = mpsc::channel();
    let reader = RcBlock::new(move |url: NonNull<NSURL>, error: *mut NSError| {
        // SAFETY: AppKit passes a valid URL, and an error or null.
        let (url, error) = unsafe { (url.as_ref(), error.as_ref()) };
        if let Some(error) = error {
            log::warn!("a promised file was not written: {error:?}");
            let _ = tx.send(None);
            return;
        }
        let _ = tx.send(url.path().map(|path| PathBuf::from(path.to_string())));
    });
    let mut expected = 0;
    for receiver in &receivers {
        expected += receiver.fileTypes().count().max(1);
        // SAFETY: AppKit's documented API: an empty options dictionary, a
        // queue of ours, and a reader that only sends on a channel.
        unsafe {
            receiver.receivePromisedFilesAtDestination_options_operationQueue_reader(
                &destination,
                &NSDictionary::new(),
                &queue,
                &reader,
            );
        }
    }
    Some(Promised {
        written,
        expected,
        dir,
        _alive: Receiving {
            _receivers: receivers,
            _queue: queue,
        },
    })
}

impl Promised {
    /// The promised files once written (up to [`PROMISE_WAIT`]), read; the
    /// temporary folder is removed.
    fn wait(self) -> Vec<PickedFile> {
        let until = Instant::now() + PROMISE_WAIT;
        let mut paths = Vec::new();
        for _ in 0..self.expected {
            match self
                .written
                .recv_timeout(until.saturating_duration_since(Instant::now()))
            {
                Ok(Some(path)) => paths.push(path),
                Ok(None) => {}
                Err(_) => {
                    log::warn!("a promised file was not written in {PROMISE_WAIT:?}");
                    break;
                }
            }
        }
        let files = read_dropped_files(&paths);
        if let Err(e) = std::fs::remove_dir_all(&self.dir) {
            log::warn!("cannot remove {}: {e}", self.dir.display());
        }
        files
    }
}

/// The `file:` URL a dragged or copied item names: its path's
/// ([`local_file_uri`]), or, when that does not resolve (a file reference
/// to a file that is gone, a file the system refuses this app), the local
/// `file:` URL as the item gave it, so the app still hears of the file and
/// can say it could not be read.
pub(crate) fn item_file_uri(uri: &NSString) -> Option<String> {
    local_file_uri(uri).or_else(|| {
        let url = NSURL::URLWithString(uri)?;
        let local = url.isFileURL()
            && url
                .path()
                .is_some_and(|path| path.to_string().starts_with('/'))
            && url.host().is_none_or(|host| {
                let host = host.to_string();
                host.is_empty() || host.eq_ignore_ascii_case("localhost")
            });
        local.then(|| uri.to_string())
    })
}

/// The `file:` URL with the path of the local file or directory `uri`
/// names, if it names one. Finder puts file reference URLs
/// (`file:///.file/id=…`) on drag and copy pasteboards: they become the
/// path's URL, which apps can turn into a path.
pub(crate) fn local_file_uri(uri: &NSString) -> Option<String> {
    let url = NSURL::URLWithString(uri)?.filePathURL()?;
    let local = url.isFileURL()
        && url
            .path()
            .is_some_and(|path| path.to_string().starts_with('/'))
        && url.host().is_none_or(|host| {
            let host = host.to_string();
            host.is_empty() || host.eq_ignore_ascii_case("localhost")
        });
    local
        .then(|| url.absoluteString())
        .flatten()
        .map(|uri| uri.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finder_references_expose_the_filename_before_drop() {
        let path = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("nsh-drag-{}-a b.png", std::process::id()));
        std::fs::write(&path, b"metadata only").unwrap();
        let url = NSURL::fileURLWithPath(&NSString::from_str(path.to_str().unwrap()));
        let reference = url.fileReferenceURL().unwrap().absoluteString().unwrap();
        assert!(reference.to_string().contains("/.file/id="));
        assert_eq!(
            local_file_uri(&reference),
            url.absoluteString().map(|s| s.to_string())
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn file_urls_become_their_paths() {
        assert_eq!(
            file_path("file:///tmp/a%20b%E4%B8%AD.png"),
            Some(PathBuf::from("/tmp/a b中.png"))
        );
        assert_eq!(file_path("https://example.com/a.png"), None);
    }

    #[test]
    fn registration_lasts_until_the_final_destination_is_removed() {
        let mut destinations = Destinations::default();
        assert_eq!(destinations.set(1, false), None);
        assert_eq!(destinations.set(2, true), Some(true));
        assert_eq!(destinations.set(5, true), None);
        assert_eq!(destinations.set(2, false), None);
        assert_eq!(destinations.set(2, false), None);
        assert_eq!(destinations.set(5, false), Some(false));
        assert_eq!(destinations.set(5, true), Some(true));
    }

    #[test]
    fn file_urls_are_local_and_absolute() {
        for uri in [
            "file:///tmp/a%20b",
            "file:///tmp/%E4%B8%AD/",
            "file://localhost/tmp/a",
        ] {
            assert!(local_file_uri(&NSString::from_str(uri)).is_some(), "{uri}");
        }
        for uri in [
            "https://example.com/file",
            "file://server/tmp/a",
            "file:relative",
            "plain text",
        ] {
            assert_eq!(local_file_uri(&NSString::from_str(uri)), None, "{uri}");
        }
    }

    #[test]
    fn file_reference_urls_become_the_paths_urls() {
        let dir = std::env::temp_dir().join("nsh file drop 中");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a b.txt");
        std::fs::write(&file, "x").unwrap();
        let path = NSString::from_str(&file.to_string_lossy());
        let path_url = NSURL::fileURLWithPath(&path);
        let reference = path_url
            .fileReferenceURL()
            .unwrap()
            .absoluteString()
            .unwrap();
        assert!(
            reference.to_string().starts_with("file:///.file/id="),
            "{reference}"
        );
        // The reference resolves to the file's real path (`/var` is `/private/var`).
        let real = std::fs::canonicalize(&file).unwrap();
        let expected = NSURL::fileURLWithPath(&NSString::from_str(&real.to_string_lossy()))
            .absoluteString()
            .unwrap()
            .to_string();
        assert_eq!(
            local_file_uri(&reference).as_deref(),
            Some(expected.as_str())
        );
        // A reference to nothing (the file is gone) is no file, but the
        // item still names one, which could not be read.
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(local_file_uri(&reference), None);
        assert_eq!(item_file_uri(&reference), Some(reference.to_string()));
        assert_eq!(
            item_file_uri(&NSString::from_str("https://example.com/a")),
            None
        );
    }
}
