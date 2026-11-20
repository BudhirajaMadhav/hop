//! Hop: a tiny link router for macOS. Make it the default browser; each link
//! either follows a rule from `~/.config/hop/rules` or shows a picker at the cursor.

use std::cell::{Cell, RefCell};
use std::fs::{self, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;
use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use block2::RcBlock;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, ProtocolObject};
use objc2::{define_class, msg_send, sel, AnyThread, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSApplicationDelegate, NSAutoresizingMaskOptions,
    NSBackingStoreType, NSBezierPath, NSColor, NSEvent, NSEventModifierFlags, NSFont,
    NSFontAttributeName, NSFontWeight, NSFontWeightMedium, NSFontWeightRegular,
    NSFontWeightSemibold, NSForegroundColorAttributeName, NSGlassEffectView, NSGraphicsContext,
    NSImage, NSLineBreakMode, NSMenu, NSMenuItem, NSMutableParagraphStyle, NSPanel,
    NSParagraphStyleAttributeName,
    NSPasteboard, NSPasteboardTypeString, NSPopUpMenuWindowLevel, NSResponder, NSRunningApplication,
    NSScreen,
    NSStringDrawing, NSTrackingArea, NSTrackingAreaOptions, NSView, NSWindow,
    NSWindowCollectionBehavior, NSWindowDelegate, NSWindowStyleMask, NSWorkspace,
};
use objc2_foundation::{
    ns_string, NSArray, NSAttributedStringKey, NSBundle, NSCopying, NSDate, NSDictionary, NSError,
    NSFileManager, NSNotification, NSObject, NSObjectProtocol, NSPoint, NSRect, NSRunLoop, NSSize,
    NSString, NSURL,
};

const BUNDLE_ID: &str = "local.hop";
const W: f64 = 300.0;
const HEAD: f64 = 58.0;
const ROW: f64 = 34.0;
const CHECK: f64 = 28.0; // the remember row
const FOOT: f64 = 6.0 + CHECK + 32.0;

const TRACKERS: &[&str] = &[
    "fbclid", "gclid", "dclid", "gbraid", "wbraid", "msclkid", "yclid", "twclid", "ttclid",
    "igshid", "igsh", "mc_cid", "mc_eid", "_hsenc", "_hsmi", "mkt_tok", "li_fat_id",
    "oly_anon_id", "oly_enc_id", "rb_clickid", "vero_id",
];

const RULES_TEMPLATE: &str = "\
# Hop rules: <domain[/path]> = <picker label | app name>
# First match wins. Subdomains match. Tick Remember (or hold ⌥) while picking to add one.
# `+<label>` marks a favorite: a top choice, in order (⌘1, ⌘2, …). `hop favorites` sets them.
# Everything that is not a favorite goes under More (0).
# `!<label>` hides a row. An app name in a `+`/`!` line covers all of its profiles.
# `hop list` prints the labels. `hop rules` opens this file.
#
# +Google Chrome (Work)
# +Safari
# !T3 Code (Beta)
# github.com/acme = Google Chrome (Work)
# youtube.com = Safari
# figma.com = Figma
";

struct Target {
    name: String,
    detail: String, // the browser name, for profile rows
    app: String,
    profile: Option<String>, // Chromium profile directory
    icon: Retained<NSImage>,
    avatar: Option<Retained<NSImage>>, // the profile picture, when the browser keeps one
}

impl Target {
    fn label(&self) -> String {
        match self.profile {
            Some(_) => format!("{} ({})", self.detail, self.name),
            None => self.name.clone(),
        }
    }

    /// `name` is a lowercased `+`/`!` entry: a label, or an app name that covers its profiles.
    fn is(&self, name: &str) -> bool {
        name == self.label().to_lowercase() || (self.profile.is_some() && name == self.detail.to_lowercase())
    }
}

struct Session {
    url: String,
    host: String,
    path: String,
    trackers: usize,
    targets: Vec<Target>, // the top choices first, then the More menu
    n_top: usize,
}

impl Session {
    fn has_more(&self) -> bool {
        self.targets.len() > self.n_top
    }

    /// The top choices plus the More row.
    fn rows(&self) -> usize {
        self.n_top + self.has_more() as usize
    }
}

struct State {
    panel: Retained<Panel>,
    session: RefCell<Option<Session>>,
    sel: Cell<usize>,
    alt: Cell<bool>,
    remember: Cell<bool>,
}

define_class!(
    // A borderless panel that can still take keyboard focus.
    #[unsafe(super(NSPanel, NSWindow, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct Panel;

    impl Panel {
        #[unsafe(method(canBecomeKeyWindow))]
        fn can_become_key(&self) -> bool {
            true
        }
    }
);

define_class!(
    // The picker view. It is also the app delegate and the panel delegate.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    #[ivars = State]
    struct Hop;

    impl Hop {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(acceptsFirstMouse:))]
        fn accepts_first_mouse(&self, _e: Option<&NSEvent>) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            self.draw();
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, e: &NSEvent) {
            self.key(e);
        }

        #[unsafe(method(flagsChanged:))]
        fn flags_changed(&self, e: &NSEvent) {
            self.ivars().alt.set(e.modifierFlags().contains(NSEventModifierFlags::Option));
            self.setNeedsDisplay(true);
        }

        #[unsafe(method(mouseMoved:))]
        fn mouse_moved(&self, e: &NSEvent) {
            if let Some(i) = self.row_at(e) {
                self.select(i);
            }
        }

        #[unsafe(method(mouseDown:))]
        fn mouse_down(&self, e: &NSEvent) {
            let y = self.convertPoint_fromView(e.locationInWindow(), None).y;
            if let Some(i) = self.row_at(e) {
                self.activate(i);
            } else if (self.footer_y()..self.footer_y() + CHECK).contains(&y) {
                self.toggle_remember();
            }
        }

        #[unsafe(method(pickMore:))]
        fn pick_more(&self, item: &NSMenuItem) {
            self.pick(item.tag() as usize);
        }
    }

    unsafe impl NSObjectProtocol for Hop {}

    unsafe impl NSApplicationDelegate for Hop {
        #[unsafe(method(applicationDidFinishLaunching:))]
        fn did_finish_launching(&self, _n: &NSNotification) {
            // Dev only: `HOP_SNAPSHOT=out.png target/debug/hop <url>` routes the URL in this process.
            std::env::args().skip(1).filter(|a| a.starts_with("http")).for_each(|u| self.route(&u));
        }

        #[unsafe(method(application:openURLs:))]
        fn open_urls(&self, _app: &NSApplication, urls: &NSArray<NSURL>) {
            for url in urls.iter() {
                if let Some(s) = url.absoluteString() {
                    self.route(&s.to_string());
                }
            }
        }

        #[unsafe(method(applicationShouldHandleReopen:hasVisibleWindows:))]
        fn reopen(&self, _app: &NSApplication, _visible: bool) -> bool {
            edit_rules();
            false
        }
    }

    unsafe impl NSWindowDelegate for Hop {
        #[unsafe(method(windowDidResignKey:))]
        fn did_resign_key(&self, _n: &NSNotification) {
            self.dismiss();
        }
    }
);

impl Hop {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(W, 200.0));
        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let panel: Retained<Panel> = unsafe {
            msg_send![Panel::alloc(mtm), initWithContentRect: frame, styleMask: style, backing: NSBackingStoreType::Buffered, defer: false]
        };
        panel.setLevel(NSPopUpMenuWindowLevel);
        panel.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Transient,
        );
        panel.setHidesOnDeactivate(false);
        unsafe { panel.setReleasedWhenClosed(false) };
        panel.setOpaque(false);
        panel.setBackgroundColor(Some(&NSColor::clearColor()));
        panel.setHasShadow(true);

        let state = State {
            panel: panel.clone(),
            session: RefCell::new(None),
            sel: Cell::new(0),
            alt: Cell::new(false),
            remember: Cell::new(false),
        };
        let hop: Retained<Self> = unsafe { msg_send![super(Self::alloc(mtm).set_ivars(state)), initWithFrame: frame] };
        hop.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable);
        let opts = NSTrackingAreaOptions::MouseMoved | NSTrackingAreaOptions::ActiveAlways | NSTrackingAreaOptions::InVisibleRect;
        let area = unsafe { NSTrackingArea::initWithRect_options_owner_userInfo(NSTrackingArea::alloc(), frame, opts, Some(&hop), None) };
        hop.addTrackingArea(&area);

        let glass = NSGlassEffectView::new(mtm);
        glass.setCornerRadius(18.0);
        glass.setContentView(Some(&hop));
        panel.setContentView(Some(&glass));
        panel.setDelegate(Some(ProtocolObject::from_ref(&*hop)));
        hop
    }

    fn route(&self, raw: &str) {
        let (url, trackers) = clean(raw);
        let parsed = NSURL::URLWithString(&NSString::from_str(&url));
        let host = parsed.as_ref().and_then(|u| u.host()).map(|h| h.to_string().to_lowercase()).unwrap_or_default();
        let path = parsed.as_ref().and_then(|u| u.path()).map(|p| p.to_string()).unwrap_or_default();
        let rules = read_rules();
        let (targets, n_top) = targets(&rules);
        if let Some(want) = find_rule(&rules, &host, &path) {
            if let Some(t) = targets.iter().find(|t| t.label().eq_ignore_ascii_case(&want)) {
                return launch(&t.app, t.profile.as_deref(), &url);
            }
            #[allow(deprecated)] // the replacement needs a bundle id; rules name apps
            if let Some(app) = NSWorkspace::sharedWorkspace().fullPathForApplication(&NSString::from_str(&want)) {
                return launch(&app.to_string(), None, &url);
            }
        }
        self.show(Session { url, host, path, trackers, targets, n_top });
    }

    fn show(&self, s: Session) {
        let st = self.ivars();
        let h = HEAD + ROW * s.rows() as f64 + FOOT;
        let m = NSEvent::mouseLocation();
        let screens = NSScreen::screens(self.mtm());
        let inside = |r: NSRect| m.x >= r.origin.x && m.x <= r.origin.x + r.size.width && m.y >= r.origin.y && m.y <= r.origin.y + r.size.height;
        let vf = screens.iter().find(|sc| inside(sc.frame())).or_else(|| screens.firstObject()).map(|sc| sc.visibleFrame());
        let Some(vf) = vf else { return };
        // Put the first row under the cursor, so a click or ↩ opens it at once.
        let x = (m.x - 40.0).min(vf.origin.x + vf.size.width - W - 8.0).max(vf.origin.x + 8.0);
        let y = (m.y + HEAD + ROW / 2.0 - h).min(vf.origin.y + vf.size.height - h - 8.0).max(vf.origin.y + 8.0);
        st.panel.setFrame_display(NSRect::new(NSPoint::new(x, y), NSSize::new(W, h)), false);
        self.setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(W, h)));

        st.session.replace(Some(s));
        st.sel.set(0);
        st.alt.set(NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Option));
        st.remember.set(false);
        self.setNeedsDisplay(true);
        #[cfg(debug_assertions)] // dev only: `HOP_SNAPSHOT=out.png target/debug/hop <url>` renders the picker without showing it
        if let Some(out) = std::env::var("HOP_SNAPSHOT").ok() {
            let rep = self.bitmapImageRepForCachingDisplayInRect(self.bounds()).unwrap();
            self.cacheDisplayInRect_toBitmapImageRep(self.bounds(), &rep);
            let png = unsafe { rep.representationUsingType_properties(objc2_app_kit::NSBitmapImageFileType::PNG, &NSDictionary::new()) };
            png.unwrap().writeToFile_atomically(&NSString::from_str(&out), true);
            std::process::exit(0);
        }
        NSApplication::sharedApplication(self.mtm()).unhideWithoutActivation();
        st.panel.makeKeyAndOrderFront(None);
        st.panel.makeFirstResponder(Some(self));
    }

    fn select(&self, i: usize) {
        if i != self.ivars().sel.get() {
            self.ivars().sel.set(i);
            self.setNeedsDisplay(true);
        }
    }

    fn row_at(&self, e: &NSEvent) -> Option<usize> {
        let n = self.ivars().session.borrow().as_ref()?.rows();
        let p = self.convertPoint_fromView(e.locationInWindow(), None);
        let i = ((p.y - HEAD) / ROW).floor();
        (i >= 0.0 && (i as usize) < n).then_some(i as usize)
    }

    fn footer_y(&self) -> f64 {
        HEAD + ROW * self.ivars().session.borrow().as_ref().map_or(0, |s| s.rows()) as f64 + 6.0
    }

    fn toggle_remember(&self) {
        self.ivars().remember.set(!self.ivars().remember.get());
        self.setNeedsDisplay(true);
    }

    /// A row is a top choice, or the More row.
    fn activate(&self, row: usize) {
        let more = self.ivars().session.borrow().as_ref().is_some_and(|s| s.has_more() && row == s.n_top);
        if more {
            self.open_more();
        } else {
            self.pick(row);
        }
    }

    /// The rest of the targets in a native menu. Profiles go in a submenu per browser.
    fn open_more(&self) {
        let mtm = self.mtm();
        let menu = NSMenu::new(mtm);
        let y = {
            let session = self.ivars().session.borrow();
            let Some(s) = session.as_ref().filter(|s| s.has_more()) else { return };
            let mut groups: Vec<(&str, Retained<NSMenu>)> = Vec::new();
            for (i, t) in s.targets.iter().enumerate().skip(s.n_top) {
                let item = self.menu_item(&t.name, i as isize, t.avatar.as_deref().unwrap_or(&t.icon));
                if t.profile.is_none() {
                    menu.addItem(&item);
                    continue;
                }
                let sub = match groups.iter().find(|(name, _)| *name == t.detail) {
                    Some((_, m)) => m.clone(),
                    None => {
                        let m = NSMenu::new(mtm);
                        let parent = self.menu_item(&t.detail, -1, &t.icon);
                        parent.setSubmenu(Some(&m));
                        menu.addItem(&parent);
                        groups.push((&t.detail, m.clone()));
                        m
                    }
                };
                sub.addItem(&item);
            }
            HEAD + ROW * (s.n_top + 1) as f64
        }; // the borrow ends here: a pick from the menu takes the session
        menu.popUpMenuPositioningItem_atLocation_inView(None, NSPoint::new(40.0, y), Some(self));
    }

    /// `tag` is the target index. A negative tag makes a submenu parent.
    fn menu_item(&self, title: &str, tag: isize, icon: &NSImage) -> Retained<NSMenuItem> {
        let action = (tag >= 0).then_some(sel!(pickMore:));
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(self.mtm()), &NSString::from_str(title), action, ns_string!(""))
        };
        let target: &AnyObject = self;
        unsafe { item.setTarget(Some(target)) };
        item.setTag(tag);
        let img = icon.copy();
        img.setSize(NSSize::new(16.0, 16.0));
        item.setImage(Some(&img));
        item
    }

    fn pick(&self, i: usize) {
        let st = self.ivars();
        if st.session.borrow().as_ref().is_none_or(|s| i >= s.targets.len()) {
            return;
        }
        let remember = st.remember.get() || NSEvent::modifierFlags_class().contains(NSEventModifierFlags::Option);
        let s = st.session.take().unwrap(); // taken first, so the resign-key from orderOut is a no-op
        let t = &s.targets[i];
        if remember {
            save_rule(&s.host, &t.label());
        }
        launch(&t.app, t.profile.as_deref(), &s.url);
        st.panel.orderOut(None);
    }

    fn dismiss(&self) {
        if self.ivars().session.take().is_some() {
            self.ivars().panel.orderOut(None);
            NSApplication::sharedApplication(self.mtm()).hide(None); // hand focus back to the source app
        }
    }

    fn key(&self, e: &NSEvent) {
        let flags = e.modifierFlags();
        let (cmd, shift) = (flags.contains(NSEventModifierFlags::Command), flags.contains(NSEventModifierFlags::Shift));
        let ch = e.charactersIgnoringModifiers().map(|s| s.to_string()).unwrap_or_default();
        let Some((n, n_top)) = self.ivars().session.borrow().as_ref().map(|s| (s.rows(), s.n_top)) else { return };
        let sel = self.ivars().sel.get();
        let digit = ch.parse::<usize>().ok().filter(|d| (1..=9).contains(d));
        match e.keyCode() {
            53 => self.dismiss(),
            36 | 76 => self.activate(sel),
            49 => self.toggle_remember(),
            125 => self.select((sel + 1) % n.max(1)),
            126 => self.select((sel + n.max(1) - 1) % n.max(1)),
            48 if shift => self.select((sel + n.max(1) - 1) % n.max(1)),
            48 => self.select((sel + 1) % n.max(1)),
            _ if cmd && ch == "c" => {
                if let Some(s) = self.ivars().session.borrow().as_ref() {
                    let pb = NSPasteboard::generalPasteboard();
                    pb.clearContents();
                    pb.setString_forType(&NSString::from_str(&s.url), unsafe { NSPasteboardTypeString });
                }
                self.dismiss();
            }
            _ if cmd && ch == "," => {
                self.dismiss();
                edit_rules();
            }
            _ if cmd && ch == "q" => NSApplication::sharedApplication(self.mtm()).terminate(None),
            _ if ch == "0" => self.open_more(),
            _ if digit.is_some_and(|d| d <= n_top) => self.pick(digit.unwrap() - 1),
            _ => {}
        }
    }

    fn draw(&self) {
        let st = self.ivars();
        let session = st.session.borrow();
        let Some(s) = session.as_ref() else { return };
        let size = self.bounds().size;
        let (w, h) = (size.width, size.height);
        let (label, dim, faint) = (NSColor::labelColor(), NSColor::secondaryLabelColor(), NSColor::tertiaryLabelColor());
        let white = NSColor::whiteColor();
        let white_dim = white.colorWithAlphaComponent(0.72);
        let (regular, medium, semibold) = unsafe { (NSFontWeightRegular, NSFontWeightMedium, NSFontWeightSemibold) };

        let remember = st.remember.get() || st.alt.get();
        let accent = NSColor::controlAccentColor();

        // Header: the host, then the path (or the rule hint while Remember is on).
        let mut host_w = w - 36.0;
        if s.trackers > 0 {
            let note = format!("−{} tracker{}", s.trackers, if s.trackers == 1 { "" } else { "s" });
            let nw = text(&note, 0.0, -100.0, w, 11.0, regular, &faint); // measure only
            text(&note, w - 18.0 - nw, 17.0, nw, 11.0, regular, &faint);
            host_w -= nw + 8.0;
        }
        text(if s.host.is_empty() { &s.url } else { rule_host(&s.host) }, 18.0, 13.0, host_w, 14.0, semibold, &label);
        if remember {
            let hint = format!("Always open {} in…", rule_host(&s.host));
            text(&hint, 18.0, 33.0, w - 36.0, 11.5, medium, &accent);
        } else {
            text(if s.path.is_empty() { "/" } else { &s.path }, 18.0, 33.0, w - 36.0, 11.5, regular, &dim);
        }

        for row in 0..s.rows() {
            let y = HEAD + ROW * row as f64;
            let on = row == st.sel.get();
            if on {
                accent.setFill();
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(NSRect::new(NSPoint::new(8.0, y), NSSize::new(w - 16.0, ROW)), 10.0, 10.0).fill();
            }
            let (fg, sub) = if on { (&white, &white_dim) } else { (&label, &dim) };
            let icon = NSRect::new(NSPoint::new(16.0, y + 6.0), NSSize::new(22.0, 22.0));
            let (name, detail, key) = match s.targets.get(row).filter(|_| row < s.n_top) {
                Some(t) => {
                    draw_icon(t, icon);
                    (t.name.as_str(), t.detail.clone(), (row < 9).then(|| (row + 1).to_string()))
                }
                None => {
                    text("•••", 17.0, y + 8.0, 24.0, 12.0, semibold, sub);
                    ("More", format!("{} more", s.targets.len() - s.n_top), Some("0".into()))
                }
            };
            let name_w = text(name, 48.0, y + 8.5, w - 100.0, 13.0, if on { medium } else { regular }, fg);
            if !detail.is_empty() {
                let x = 48.0 + name_w + 6.0;
                text(&detail, x, y + 8.5, (w - 44.0 - x).max(0.0), 13.0, regular, sub);
            }
            if let Some(key) = key {
                text(&key, w - 32.0, y + 9.0, 14.0, 12.0, medium, if on { &white_dim } else { &faint });
            }
        }

        // Footer: the Remember checkbox, then the key hints.
        let y = h - FOOT + 6.0;
        let check = NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(NSRect::new(NSPoint::new(18.5, y + 7.5), NSSize::new(14.0, 14.0)), 4.0, 4.0);
        if remember {
            accent.setFill();
            check.fill();
            text("✓", 21.0, y + 6.5, 12.0, 11.0, semibold, &white);
        } else {
            dim.setStroke();
            check.setLineWidth(1.0);
            check.stroke();
        }
        text(&format!("Remember for {}", rule_host(&s.host)), 41.0, y + 6.0, w - 59.0, 12.5, regular, &label);
        text("↩ open    space remember    ⌘C copy    ⌘, rules", 18.0, y + CHECK + 8.0, w - 36.0, 11.0, regular, &faint);
    }
}

/// Draws one line of truncated text with its top-left at (x, y). Returns the drawn width.
fn text(s: &str, x: f64, y: f64, max_w: f64, size: f64, weight: NSFontWeight, color: &NSColor) -> f64 {
    let font = NSFont::systemFontOfSize_weight(size, weight);
    let para = NSMutableParagraphStyle::new();
    para.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
    let values: [&AnyObject; 3] = [&font, color, &para];
    let ns = NSString::from_str(s);
    unsafe {
        let keys = [NSFontAttributeName, NSForegroundColorAttributeName, NSParagraphStyleAttributeName];
        let attrs: Retained<NSDictionary<NSAttributedStringKey, AnyObject>> = NSDictionary::from_slices(&keys, &values);
        let sz = ns.sizeWithAttributes(Some(&attrs));
        ns.drawInRect_withAttributes(NSRect::new(NSPoint::new(x, y), NSSize::new(max_w, sz.height)), Some(&attrs));
        sz.width.min(max_w)
    }
}

/// The profile picture in a circle with a small browser badge, else the app icon.
fn draw_icon(t: &Target, r: NSRect) {
    let Some(avatar) = &t.avatar else { return t.icon.drawInRect(r) };
    NSGraphicsContext::saveGraphicsState_class();
    NSBezierPath::bezierPathWithOvalInRect(r).addClip();
    avatar.drawInRect(r);
    NSGraphicsContext::restoreGraphicsState_class();
    let b = 13.0;
    t.icon.drawInRect(NSRect::new(NSPoint::new(r.origin.x + r.size.width - b + 4.0, r.origin.y + r.size.height - b + 4.0), NSSize::new(b, b)));
}

fn probe() -> Retained<NSURL> {
    NSURL::URLWithString(ns_string!("https://example.com")).unwrap()
}

/// Every app that opens https links, minus Hop and `!hidden` rows, with the `+pinned` ones first.
/// Chromium browsers with 2+ profiles get one row per profile. Returns the targets and the top-choice count.
fn targets(rules: &str) -> (Vec<Target>, usize) {
    let ws = NSWorkspace::sharedWorkspace();
    let fm = NSFileManager::defaultManager();
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for app in ws.URLsForApplicationsToOpenURL(&probe()).iter() {
        let Some(path) = app.path() else { continue };
        let id = NSBundle::bundleWithURL(&app).and_then(|b| b.bundleIdentifier()).map_or_else(|| path.to_string(), |s| s.to_string());
        if id == BUNDLE_ID || seen.contains(&id) {
            continue;
        }
        let name = fm.displayNameAtPath(&path).to_string().trim_end_matches(".app").to_string();
        let icon = ws.iconForFile(&path);
        let profiles = chromium_profiles(&id);
        if profiles.len() > 1 {
            for (dir, pname, picture) in profiles {
                let avatar = picture.exists().then(|| NSImage::initWithContentsOfFile(NSImage::alloc(), &NSString::from_str(&picture.to_string_lossy()))).flatten();
                out.push(Target { name: pname, detail: name.clone(), app: path.to_string(), profile: Some(dir), icon: icon.clone(), avatar });
            }
        } else {
            out.push(Target { name, detail: String::new(), app: path.to_string(), profile: None, icon, avatar: None });
        }
        seen.push(id);
    }
    let hidden = directives(rules, '!');
    out.retain(|t| !hidden.iter().any(|h| t.is(h)));
    // A stable order keeps the shortcuts the same between clicks.
    out.sort_by_key(|t| if t.profile.is_some() { &t.detail } else { &t.name }.to_lowercase());
    let mut top = Vec::new();
    for pin in directives(rules, '+') {
        while let Some(i) = out.iter().position(|t| t.is(&pin)) {
            top.push(out.remove(i));
        }
    }
    // No pins, or none that match: show everything as top choices.
    let n_top = if top.is_empty() { out.len() } else { top.len() };
    top.append(&mut out);
    (top, n_top)
}

/// The lowercased entries of the rules lines that start with `c`.
fn directives(rules: &str, c: char) -> Vec<String> {
    rules.lines().filter_map(|l| Some(l.trim().strip_prefix(c)?.trim().to_lowercase())).collect()
}

/// (profile directory, display name, picture path) in the browser's own order.
fn chromium_profiles(bundle_id: &str) -> Vec<(String, String, PathBuf)> {
    let dir = match bundle_id {
        "com.google.Chrome" => "Google/Chrome",
        "com.google.Chrome.beta" => "Google/Chrome Beta",
        "com.google.Chrome.dev" => "Google/Chrome Dev",
        "com.google.Chrome.canary" => "Google/Chrome Canary",
        "com.brave.Browser" => "BraveSoftware/Brave-Browser",
        "com.microsoft.edgemac" => "Microsoft Edge",
        "com.vivaldi.Vivaldi" => "Vivaldi",
        "org.chromium.Chromium" => "Chromium",
        _ => return vec![],
    };
    let base = home().join("Library/Application Support").join(dir);
    let Some(v) = fs::read(base.join("Local State")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) else {
        return vec![];
    };
    let p = &v["profile"];
    let Some(cache) = p["info_cache"].as_object() else { return vec![] };
    let order: Vec<String> = match p["profiles_order"].as_array() {
        Some(a) => a.iter().filter_map(|d| d.as_str().map(String::from)).collect(),
        None => cache.keys().cloned().collect(),
    };
    order
        .into_iter()
        .filter_map(|d| {
            let name = cache.get(&d)?["name"].as_str()?.to_string();
            let picture = base.join(&d).join("Google Profile Picture.png");
            Some((d, name, picture))
        })
        .collect()
}

fn launch(app: &str, profile: Option<&str>, url: &str) {
    let mut cmd = Command::new("/usr/bin/open");
    match profile {
        // -n starts a short-lived instance. Chromium hands the URL to the running browser in that profile.
        Some(dir) => cmd.args(["-na", app, "--args", &format!("--profile-directory={dir}"), url]),
        None => cmd.args(["-a", app, url]),
    };
    spawn(cmd);
}

fn spawn(mut cmd: Command) {
    // ponytail: one reaper thread per launch. That is fine at human click rates.
    if let Ok(mut child) = cmd.spawn() {
        std::thread::spawn(move || child.wait());
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn rules_path() -> PathBuf {
    home().join(".config/hop/rules")
}

fn read_rules() -> String {
    fs::read_to_string(rules_path()).unwrap_or_else(|_| RULES_TEMPLATE.into())
}

/// Replaces the `+` lines of `rules` with `labels`, in order. Keeps every other line.
fn set_favorites(rules: &str, labels: &[String]) -> String {
    let mut out: String = rules.lines().filter(|l| !l.trim_start().starts_with('+')).map(|l| format!("{l}\n")).collect();
    out.extend(labels.iter().map(|l| format!("+{l}\n")));
    out
}

fn find_rule(rules: &str, host: &str, path: &str) -> Option<String> {
    let path = path.to_lowercase();
    rules.lines().find_map(|line| {
        let (pat, target) = line.split('#').next()?.split_once('=')?;
        let (pat, target) = (pat.trim().to_lowercase(), target.trim());
        (!pat.is_empty() && !target.is_empty() && matches(&pat, host, &path)).then(|| target.to_string())
    })
}

/// `pat` is `domain` or `domain/path-prefix`. The domain also matches its subdomains.
fn matches(pat: &str, host: &str, path: &str) -> bool {
    let (domain, prefix) = pat.split_once('/').unwrap_or((pat, ""));
    let host_ok = host == domain || host.strip_suffix(domain).is_some_and(|h| h.ends_with('.'));
    let rest = path.trim_start_matches('/').strip_prefix(prefix.trim_end_matches('/'));
    host_ok && rest.is_some_and(|r| prefix.is_empty() || r.is_empty() || r.starts_with('/'))
}

fn rule_host(host: &str) -> &str {
    host.strip_prefix("www.").unwrap_or(host)
}

fn save_rule(host: &str, label: &str) {
    if host.is_empty() {
        return;
    }
    let p = rules_path();
    let _ = fs::create_dir_all(p.parent().unwrap());
    let existing = fs::read_to_string(&p).unwrap_or_else(|_| RULES_TEMPLATE.into());
    let sep = if existing.is_empty() || existing.ends_with('\n') { "" } else { "\n" };
    let line = format!("{sep}{} = {label}\n", rule_host(host));
    let fresh = !p.exists();
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&p) {
        let _ = f.write_all(if fresh { format!("{RULES_TEMPLATE}{line}") } else { line }.as_bytes());
    }
}

fn edit_rules() {
    let p = rules_path();
    if !p.exists() {
        let _ = fs::create_dir_all(p.parent().unwrap());
        let _ = fs::write(&p, RULES_TEMPLATE);
    }
    let mut cmd = Command::new("/usr/bin/open");
    cmd.arg("-t").arg(&p);
    spawn(cmd);
}

/// Removes known tracking parameters. Returns the URL and the count it removed.
fn clean(url: &str) -> (String, usize) {
    let (base, frag) = url.split_at(url.find('#').unwrap_or(url.len()));
    let Some((head, query)) = base.split_once('?') else { return (url.into(), 0) };
    let pairs: Vec<&str> = query.split('&').filter(|kv| !kv.is_empty()).collect();
    let kept: Vec<&str> = pairs
        .iter()
        .copied()
        .filter(|kv| {
            let key = kv.split('=').next().unwrap_or("").to_ascii_lowercase();
            !(key.starts_with("utm_") || TRACKERS.contains(&key.as_str()))
        })
        .collect();
    let removed = pairs.len() - kept.len();
    if removed == 0 {
        return (url.into(), 0);
    }
    let q = if kept.is_empty() { String::new() } else { format!("?{}", kept.join("&")) };
    (format!("{head}{q}{frag}"), removed)
}

// The `hop` command. `./bundle.sh install` links it into ~/.local/bin.

const APP_PATH: &str = "/Applications/Hop.app";

const HELP: &str = "\
hop: a link router for macOS

  hop <url>        open a link through Hop
  hop favorites    pick the browsers and profiles at the top of the picker
  hop default      make Hop the default browser
  hop rules        open the rules file (~/.config/hop/rules)
  hop list         print the picker labels (+ marks a favorite)
  hop uninstall    give your links back to another browser, then remove Hop
";

fn cli(args: &[String]) {
    match args.first().map(String::as_str) {
        Some("favorites" | "favourites") => favorites(),
        Some("default") => make_default(),
        Some("rules") => {
            println!("{}", rules_path().display());
            edit_rules();
        }
        Some("list") => list(),
        Some("uninstall") => uninstall(),
        Some(url) if url.starts_with("http://") || url.starts_with("https://") => {
            installed_or_exit();
            let _ = Command::new("/usr/bin/open").args(["-a", APP_PATH, url]).status();
        }
        Some("help" | "-h" | "--help") | None => print!("{HELP}"),
        Some(other) => {
            eprint!("hop: unknown command `{other}`\n\n{HELP}");
            std::process::exit(2);
        }
    }
}

fn installed_or_exit() -> Retained<NSURL> {
    if !PathBuf::from(APP_PATH).exists() {
        eprintln!("Hop is not installed. Run: ./bundle.sh install");
        std::process::exit(1);
    }
    NSURL::fileURLWithPath(&NSString::from_str(APP_PATH))
}

fn list() {
    let (targets, n_top) = targets(&read_rules());
    for (i, t) in targets.iter().enumerate() {
        println!("{}{}", if i < n_top && n_top < targets.len() { "+" } else { "" }, t.label());
    }
}

/// Puts the terminal in key-at-a-time mode until dropped. `None` when stdin is not a terminal.
struct RawTerminal(String);

impl RawTerminal {
    fn on() -> Option<Self> {
        if !io::stdin().is_terminal() {
            return None;
        }
        let saved = stty(&["-g"])?;
        // -isig: Ctrl-C arrives as a key, so the terminal is always restored. time 1: reads time out after 0.1 s.
        stty(&["-icanon", "-echo", "-isig", "min", "0", "time", "1"])?;
        Some(Self(saved))
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        stty(&[&self.0]);
        print!("\x1b[?25h");
        let _ = io::stdout().flush();
    }
}

fn stty(args: &[&str]) -> Option<String> {
    let out = Command::new("stty").args(args).stdin(std::process::Stdio::inherit()).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[derive(PartialEq)]
enum Key {
    Up,
    Down,
    MoveUp,   // Shift+↑ or K
    MoveDown, // Shift+↓ or J
    Space,
    Enter,
    Quit,
    Other,
}

fn read_byte(stdin: &mut impl Read) -> Option<u8> {
    let mut b = [0u8; 1];
    (stdin.read(&mut b).ok()? == 1).then_some(b[0])
}

fn read_key(stdin: &mut impl Read) -> Key {
    let first = loop {
        if let Some(b) = read_byte(stdin) {
            break b;
        }
    };
    match first {
        b'\r' | b'\n' => Key::Enter,
        b' ' => Key::Space,
        b'k' => Key::Up,
        b'j' => Key::Down,
        b'K' => Key::MoveUp,
        b'J' => Key::MoveDown,
        b'q' | 3 | 4 => Key::Quit, // q, Ctrl-C, Ctrl-D
        27 => {
            // An escape sequence ends with a letter or `~`. Nothing within 0.1 s means Esc on its own.
            let mut seq = Vec::new();
            while let Some(b) = read_byte(stdin) {
                seq.push(b);
                if b.is_ascii_alphabetic() || b == b'~' || seq.len() > 6 {
                    break;
                }
            }
            match seq.as_slice() {
                [] => Key::Quit,
                b"[A" | b"OA" => Key::Up,
                b"[B" | b"OB" => Key::Down,
                [b'[', b'1', b';', _, b'A'] => Key::MoveUp, // ↑ with Shift (or another modifier)
                [b'[', b'1', b';', _, b'B'] => Key::MoveDown,
                _ => Key::Other,
            }
        }
        _ => Key::Other,
    }
}

/// A terminal list picker. With `multi`, space adds or removes a row, Shift+↑↓ reorders the added rows,
/// and the result is in that order; `picked` is the starting selection, and the added rows show first.
/// Without `multi`, Enter picks the row under the cursor, which starts on `picked[0]`.
/// `None` means the user cancelled, or there is no terminal.
fn choose(title: &str, items: &[String], mut picked: Vec<usize>, multi: bool) -> Option<Vec<usize>> {
    let n = items.len();
    if n == 0 {
        return None;
    }
    let _raw = RawTerminal::on()?;
    let mut stdin = io::stdin().lock();
    let mut out = io::stdout();
    let mut cur = picked.first().copied().unwrap_or(0).min(n - 1); // an item index: the cursor follows its row
    let help = if multi {
        "↑↓ move · space add/remove · shift+↑↓ reorder · enter save · q cancel"
    } else {
        "↑↓ move · enter choose · q cancel"
    };
    print!("\x1b[?25l{title}\n\x1b[2m{help}\x1b[0m\n");
    for first in std::iter::once(true).chain(std::iter::repeat(false)) {
        let view: Vec<usize> = match multi {
            true => picked.iter().copied().chain((0..n).filter(|i| !picked.contains(i))).collect(),
            false => (0..n).collect(),
        };
        let pos = view.iter().position(|&i| i == cur).unwrap_or(0);
        if !first {
            print!("\x1b[{n}A"); // back to the top of the list, then redraw it
        }
        for &i in &view {
            let mark = match picked.iter().position(|&p| p == i) {
                _ if !multi => String::new(),
                Some(rank) => format!("\x1b[36m{:>2}\x1b[0m ", rank + 1),
                None => "\x1b[2m ·\x1b[0m ".into(),
            };
            let (arrow, bold) = if i == cur { ("\x1b[36m›\x1b[0m", "\x1b[1m") } else { (" ", "") };
            print!("\r\x1b[2K {arrow} {mark}{bold}{}\x1b[0m\n", items[i]);
        }
        let _ = out.flush();
        let rank = picked.iter().position(|&p| p == cur);
        match read_key(&mut stdin) {
            Key::Up => cur = view[(pos + n - 1) % n],
            Key::Down => cur = view[(pos + 1) % n],
            Key::Space if multi => match rank {
                Some(r) => drop(picked.remove(r)),
                None => picked.push(cur),
            },
            Key::MoveUp if multi => {
                if let Some(r) = rank.filter(|&r| r > 0) {
                    picked.swap(r, r - 1);
                }
            }
            Key::MoveDown if multi => {
                if let Some(r) = rank.filter(|&r| r + 1 < picked.len()) {
                    picked.swap(r, r + 1);
                }
            }
            Key::Enter => return Some(if multi { picked } else { vec![cur] }),
            Key::Quit => return None,
            _ => {}
        }
    }
    unreachable!()
}

fn favorites() {
    let rules = read_rules();
    let (targets, n_top) = targets(&rules);
    let items: Vec<String> = targets.iter().map(|t| t.label()).collect();
    let current = if n_top < targets.len() { (0..n_top).collect() } else { Vec::new() };
    let title = "Pick your favorites. They go at the top of the picker, in this order.";
    let Some(picked) = choose(title, &items, current, true) else {
        return println!("Kept the current favorites.");
    };
    let labels: Vec<String> = picked.iter().map(|&i| items[i].clone()).collect();
    let p = rules_path();
    let _ = fs::create_dir_all(p.parent().unwrap());
    match fs::write(&p, set_favorites(&rules, &labels)) {
        Ok(()) if labels.is_empty() => println!("No favorites: the picker shows every browser."),
        Ok(()) => println!("Favorites: {}", labels.join(", ")),
        Err(e) => eprintln!("Could not write {}: {e}", p.display()),
    }
}

/// The bundle id and name of the app that opens `scheme` links now.
fn handler(scheme: &str) -> Option<(String, String)> {
    let url = NSURL::URLWithString(&NSString::from_str(&format!("{scheme}://example.com")))?;
    let app = NSWorkspace::sharedWorkspace().URLForApplicationToOpenURL(&url)?;
    let id = NSBundle::bundleWithURL(&app)?.bundleIdentifier()?.to_string();
    let name = NSFileManager::defaultManager().displayNameAtPath(&*app.path()?).to_string();
    Some((id, name.trim_end_matches(".app").to_string()))
}

fn is_default(id: &str) -> bool {
    ["http", "https"].iter().all(|s| handler(s).is_some_and(|(h, _)| h == id))
}

/// Asks macOS to make `app` the default browser. macOS shows its own dialog; this waits for the answer.
fn set_default(app: &NSURL, id: &str) -> bool {
    for scheme in ["http", "https"] {
        if handler(scheme).is_some_and(|(h, _)| h == id) {
            continue; // the http change usually moves https too
        }
        let (tx, rx) = mpsc::channel();
        let done = RcBlock::new(move |_err: *mut NSError| {
            let _ = tx.send(());
        });
        NSWorkspace::sharedWorkspace().setDefaultApplicationAtURL_toOpenURLsWithScheme_completionHandler(app, &NSString::from_str(scheme), Some(&done));
        let deadline = Instant::now() + Duration::from_secs(300);
        while rx.try_recv().is_err() && Instant::now() < deadline {
            NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.1));
        }
    }
    is_default(id)
}

fn make_default() {
    let app = installed_or_exit();
    if is_default(BUNDLE_ID) {
        return println!("Hop is already the default browser.");
    }
    println!("macOS asks you to confirm. Choose “Use “Hop””.");
    if set_default(&app, BUNDLE_ID) {
        println!("Hop is now the default browser.");
    } else {
        let now = handler("https").map_or("another app".into(), |(_, name)| name);
        println!("{now} is still the default browser. Run `hop default` to ask again.");
    }
}

/// The user picks the browser that gets the links back. Returns false if Hop is still the default.
fn restore_default() -> bool {
    if !["http", "https"].iter().any(|s| handler(s).is_some_and(|(h, _)| h == BUNDLE_ID)) {
        return true;
    }
    let (targets, _) = targets(&read_rules());
    let mut apps: Vec<(String, String)> = Vec::new(); // (name, path), one per app
    for t in &targets {
        if !apps.iter().any(|(_, path)| *path == t.app) {
            apps.push((if t.profile.is_some() { &t.detail } else { &t.name }.clone(), t.app.clone()));
        }
    }
    apps.sort_by_key(|(name, _)| name.to_lowercase());
    let safari = apps.iter().position(|(name, _)| name == "Safari").unwrap_or(0);
    let names: Vec<String> = apps.iter().map(|(name, _)| name.clone()).collect();
    let pick = if io::stdin().is_terminal() {
        match choose("Which browser gets your links after Hop is gone?", &names, vec![safari], false) {
            Some(p) => p[0],
            None => return false,
        }
    } else {
        safari
    };
    let Some((name, path)) = apps.get(pick) else {
        eprintln!("Hop found no other browser. Pick one in System Settings → Desktop & Dock.");
        return false;
    };
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    let id = NSBundle::bundleWithURL(&url).and_then(|b| b.bundleIdentifier()).map(|s| s.to_string()).unwrap_or_default();
    println!("macOS asks you to confirm. Choose “Use “{name}””.");
    set_default(&url, &id)
}

fn uninstall() {
    installed_or_exit();
    if !restore_default() {
        return eprintln!("Hop is still the default browser, so Hop stays installed.");
    }
    for app in NSRunningApplication::runningApplicationsWithBundleIdentifier(&NSString::from_str(BUNDLE_ID)).iter() {
        if app.processIdentifier() as u32 != std::process::id() {
            app.terminate();
        }
    }
    let lsregister = "/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister";
    let _ = Command::new(lsregister).args(["-u", APP_PATH]).status();
    if let Err(e) = fs::remove_dir_all(APP_PATH) {
        return eprintln!("Could not remove {APP_PATH}: {e}");
    }
    let link = home().join(".local/bin/hop");
    if fs::read_link(&link).is_ok_and(|target| target.starts_with(APP_PATH)) {
        let _ = fs::remove_file(&link);
    }
    println!("Removed Hop. Your rules are still in {}.", rules_path().display());
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // LaunchServices starts the app with no arguments and no terminal. Anything else is the `hop` command.
    let snapshot = cfg!(debug_assertions) && std::env::var_os("HOP_SNAPSHOT").is_some();
    if !snapshot && (!args.is_empty() || io::stdin().is_terminal()) {
        return cli(&args);
    }
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let hop = Hop::new(mtm);
    app.setDelegate(Some(ProtocolObject::from_ref(&*hop)));
    app.run();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_trackers_only() {
        assert_eq!(clean("https://a.com/x"), ("https://a.com/x".into(), 0));
        assert_eq!(clean("https://a.com/x?id=1&q=a+b"), ("https://a.com/x?id=1&q=a+b".into(), 0));
        assert_eq!(clean("https://a.com/x?utm_source=t&id=1&fbclid=z#top"), ("https://a.com/x?id=1#top".into(), 2));
        assert_eq!(clean("https://a.com/?UTM_Medium=x&gclid=1"), ("https://a.com/".into(), 2));
        assert_eq!(clean("https://a.com/#/r?utm_source=t"), ("https://a.com/#/r?utm_source=t".into(), 0));
    }

    #[test]
    fn favorites_replace_only_the_pin_lines() {
        let rules = "# +Safari (an example)\n+Old\nyoutube.com = Safari\n  +Older\n!Spotify";
        let want = "# +Safari (an example)\nyoutube.com = Safari\n!Spotify\n+A\n+B (C)\n";
        assert_eq!(set_favorites(rules, &["A".into(), "B (C)".into()]), want);
    }

    #[test]
    fn rules_match_domains_and_paths() {
        let rules = "# c\nyoutube.com = Safari\ngithub.com/acme = Google Chrome (Work)  # work\ngithub.com = Firefox\n";
        assert_eq!(find_rule(rules, "www.youtube.com", "/watch").as_deref(), Some("Safari"));
        assert_eq!(find_rule(rules, "notyoutube.com", "/"), None);
        assert_eq!(find_rule(rules, "github.com", "/Acme/repo").as_deref(), Some("Google Chrome (Work)"));
        assert_eq!(find_rule(rules, "github.com", "/acme").as_deref(), Some("Google Chrome (Work)"));
        assert_eq!(find_rule(rules, "github.com", "/acmex/repo").as_deref(), Some("Firefox"));
        assert_eq!(find_rule("bad line\n= Safari\nx.com =\n!Safari\n", "x.com", "/"), None);
    }
}
