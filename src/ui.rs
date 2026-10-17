//! GTK4 + wlr-layer-shell user interface for Trandex.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};

use async_channel::Receiver;
use gtk::gdk;
use gtk::gio;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};

use crate::config::{Config, PreviewPosition, Provider, WindowPosition};
use crate::indexer::{self, DesktopEntry, Item, Source};

fn tick(t0: std::time::Instant, label: &str) {
    eprintln!("[{:>5} ms] {}", t0.elapsed().as_millis(), label);
}

fn starts_with_ci(haystack: &str, needle: &str) -> bool {
    let mut h = haystack.chars();
    for n in needle.chars() {
        let Some(value) = h.next() else { return false };
        if !value.eq_ignore_ascii_case(&n) {
            return false;
        }

    }
    true
}

fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }

    fn calculator_answer_legacy(query: &str) -> Option<String> {
        let query = query.trim();
        if query.is_empty() || query.chars().all(|c| c.is_ascii_digit() || c == '.') { return None; }
        let mut x = None;
        let mut answer = None;
        for part in query.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if let Some((name, value)) = part.split_once('=') {
                if name.trim() != "x" { return None; }
                x = Some(eval_math(value.trim(), x)?);
                answer = x;
            } else {
                answer = Some(eval_math(part, x)?);
            }
        }
        answer.map(|value| if value.fract().abs() < 1e-10 {
            (value.round() as i64).to_string()
        } else {
            format!("{value:.10}").trim_end_matches('0').trim_end_matches('.').to_string()
        })
    }

    fn eval_math(input: &str, x: Option<f64>) -> Option<f64> {
        let mut value = 0.0;
        let mut op = '+';
        let mut number = String::new();
        let mut variable = false;
        for c in input.chars().chain(std::iter::once('+')) {
            if c.is_ascii_digit() || c == '.' {
                number.push(c);
            } else if c == 'x' {
                variable = true;
            } else if "+-*/".contains(c) {
                let rhs = if variable { x? } else { number.parse().ok()? };
                match op { '+' => value += rhs, '-' => value -= rhs, '*' => value *= rhs, '/' if rhs != 0.0 => value /= rhs, _ => return None }
                number.clear();
                variable = false;
                op = c;
            } else if !c.is_whitespace() {
                return None;
            }
        }
        Some(value)
    }

    fn calculator_answer_legacy2(query: &str) -> Option<String> {
        let query = query.trim();
        if query.is_empty() || query.chars().all(|c| c.is_ascii_digit() || c == '.') {
            return None;
        }
        let mut x = None;
        let mut answer = None;
        for part in query.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if let Some((name, value)) = part.split_once('=') {
                if name.trim() != "x" { return None; }
                x = Some(parse_math(value.trim(), x)?);
                answer = x;
            } else {
                answer = Some(parse_math(part, x)?);
            }
        }
        answer.map(|value| {
            if value.fract().abs() < 1e-10 {
                (value.round() as i64).to_string()
            } else {
                format!("{value:.10}").trim_end_matches('0').trim_end_matches('.').to_string()
            }
        })
    }

    fn parse_math(input: &str, x: Option<f64>) -> Option<f64> {
        struct P { chars: Vec<char>, pos: usize, x: Option<f64> }
        impl P {
            fn ws(&mut self) { while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) { self.pos += 1; } }
            fn expr(&mut self) -> Option<f64> {
                let mut v = self.term()?;
                loop {
                    self.ws();
                    match self.chars.get(self.pos).copied() {
                        Some('+') => { self.pos += 1; v += self.term()?; }
                        Some('-') => { self.pos += 1; v -= self.term()?; }
                        _ => return Some(v),
                    }
                }
            }
            fn term(&mut self) -> Option<f64> {
                let mut v = self.factor()?;
                loop {
                    self.ws();
                    match self.chars.get(self.pos).copied() {
                        Some('*') => { self.pos += 1; v *= self.factor()?; }
                        Some('/') => { self.pos += 1; let n = self.factor()?; if n == 0.0 { return None; } v /= n; }
                        Some('x') => { self.pos += 1; v *= self.x?; }
                        _ => return Some(v),
                    }
                }
            }
            fn factor(&mut self) -> Option<f64> {
                self.ws();
                if self.chars.get(self.pos) == Some(&'-') { self.pos += 1; return Some(-self.factor()?); }
                if self.chars.get(self.pos) == Some(&'(') {
                    self.pos += 1;
                    let v = self.expr()?;
                    self.ws();
                    if self.chars.get(self.pos) != Some(&')') { return None; }
                    self.pos += 1;
                    return Some(v);
                }
                if self.chars.get(self.pos) == Some(&'x') { self.pos += 1; return self.x; }
                let start = self.pos;
                while self.chars.get(self.pos).is_some_and(|c| c.is_ascii_digit() || *c == '.') { self.pos += 1; }
                (self.pos > start).then(|| self.chars[start..self.pos].iter().collect::<String>().parse().ok()).flatten()
            }
        }
        let mut p = P { chars: input.chars().collect(), pos: 0, x };
        let value = p.expr()?;
        p.ws();
        (p.pos == p.chars.len()).then_some(value)
    }
    haystack.char_indices().any(|(start, _)| {
        let mut candidate = haystack[start..].chars();
        needle.chars().all(|wanted| {
            candidate
                .next()
                .is_some_and(|actual| actual.eq_ignore_ascii_case(&wanted))
        })
    })
}

fn calculator_answer(query: &str) -> Option<String> {
    let query = query.trim();
    if query.is_empty() || query.chars().all(|c| c.is_ascii_digit() || c == '.') { return None; }
    let (expression, variable) = if let Some((left, right)) = query.split_once(',') {
        let (name, value) = left.split_once('=')?;
        if name.trim() != "x" || right.contains('=') { return None; }
        (right.trim(), Some(eval_math(value.trim(), None)?))
    } else {
        (query, None)
    };
    let answer = eval_math(expression, variable)?;
    Some(if answer.fract().abs() < 1e-10 {
        (answer.round() as i64).to_string()
    } else {
        format!("{answer:.10}").trim_end_matches('0').trim_end_matches('.').to_string()
    })
}

fn eval_math(input: &str, x: Option<f64>) -> Option<f64> {
    struct Parser {
        chars: Vec<char>,
        pos: usize,
        x: Option<f64>,
    }
    impl Parser {
        fn ws(&mut self) {
            while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
                self.pos += 1;
            }
        }
        fn expression(&mut self) -> Option<f64> {
            let mut value = self.term()?;
            loop {
                self.ws();
                match self.chars.get(self.pos).copied() {
                    Some('+') => { self.pos += 1; value += self.term()?; }
                    Some('-') => { self.pos += 1; value -= self.term()?; }
                    _ => return Some(value),
                }
            }
        }
        fn term(&mut self) -> Option<f64> {
            let mut value = self.power()?;
            loop {
                self.ws();
                match self.chars.get(self.pos).copied() {
                    Some('*') => { self.pos += 1; value *= self.power()?; }
                    Some('/') => {
                        self.pos += 1;
                        let divisor = self.power()?;
                        if divisor == 0.0 { return None; }
                        value /= divisor;
                    }
                    Some('x') if self.x.is_some() => value *= self.factor()?,
                    _ => return Some(value),
                }
            }
        }
        fn power(&mut self) -> Option<f64> {
            let base = self.factor()?;
            self.ws();
            if self.chars.get(self.pos) == Some(&'^') {
                self.pos += 1;
                return Some(base.powf(self.power()?));
            }
            Some(base)
        }
        fn factor(&mut self) -> Option<f64> {
            self.ws();
            if self.chars.get(self.pos) == Some(&'-') {
                self.pos += 1;
                return Some(-self.factor()?);
            }
            if self.chars.get(self.pos) == Some(&'(') {
                self.pos += 1;
                let value = self.expression()?;
                self.ws();
                if self.chars.get(self.pos) != Some(&')') { return None; }
                self.pos += 1;
                return Some(value);
            }
            if self.chars.get(self.pos) == Some(&'x') {
                self.pos += 1;
                return self.x;
            }
            let start = self.pos;
            while self.chars.get(self.pos).is_some_and(|c| c.is_ascii_digit() || *c == '.') {
                self.pos += 1;
            }
            if self.pos == start { return None; }
            self.chars[start..self.pos].iter().collect::<String>().parse().ok()
        }
    }
    let mut parser = Parser { chars: input.chars().collect(), pos: 0, x };
    let value = parser.expression()?;
    parser.ws();
    (parser.pos == parser.chars.len()).then_some(value)
}

/// Distance from the top edge of the screen, in pixels.
const TOP_MARGIN: i32 = 120;
/// Maximum number of results rendered at once.
const MAX_RESULTS: usize = 100;
/// Maximum index batches folded into `State` per main-loop wakeup.
const MAX_BATCHES_PER_POLL: usize = 32;
/// Minimum gap between refreshes while batches are still streaming in and the
/// user has typed something (see `drain_index`).
const REFRESH_THROTTLE_MS: u64 = 150;
/// Labels for Alt + digit shortcuts on the first ten result rows.
const SHORTCUTS: [&str; 10] = [
    "Alt + 1", "Alt + 2", "Alt + 3", "Alt + 4", "Alt + 5",
    "Alt + 6", "Alt + 7", "Alt + 8", "Alt + 9", "Alt + 0",
];

/// Fallback stylesheet. User-configured stylesheets are loaded from disk first.
const BASIC_STYLE: &str = include_str!("themes/basic.css");

/// A single searchable result: either an indexed file or a launchable app.
///
/// Defined in `indexer.rs` (together with `Source`) because indexing threads
/// both construct and lower-case items; the UI only consumes them. See
/// `indexer::Item` and `indexer::Source`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemRef {
    App(usize),
    Command(usize),
    Math,
    File(usize),
}

/// A message produced by one of the indexing worker threads.
enum IndexMessage {
    /// The parsed desktop applications, as items.
    Apps(Vec<Item>),
    /// The application scan has finished.
    AppsDone,
}

struct FilterRequest {
    generation: u64,
    query: String,
    index_revision: u64,
}

struct FilterReply {
    generation: u64,
    shown: Vec<ItemRef>,
    elapsed: Duration,
    index_revision: u64,
}

struct FileReply {
    generation: u64,
    paths: Vec<PathBuf>,
    done: bool,
    elapsed: Duration,
    total_elapsed: Duration,
}

struct FileQueryRequest {
    generation: u64,
    query: String,
    sent_at: Instant,
}

/// In-memory index and the subset currently displayed.
struct State {
    /// Desktop applications, indexed on the main thread.
    apps: Arc<RwLock<Vec<Item>>>,
    /// The current query's bounded file results, never the full file index.
    file_results: Vec<PathBuf>,
    /// Items currently shown, in row order.
    shown: Vec<ItemRef>,
    /// Incremented whenever an index batch changes the searchable sources.
    index_revision: u64,
    /// Current search text.
    query: String,
    math_answer: Option<String>,
    math_expression: Option<String>,
    providers: Vec<Provider>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            apps: Arc::new(RwLock::new(Vec::new())),
            file_results: Vec::new(),
            shown: Vec::new(),
            query: String::new(),
            math_answer: None,
            math_expression: None,
            index_revision: 0,
            providers: vec![Provider::AppList, Provider::Math, Provider::Index],
        }
    }
}

/// The widgets that make up the results area.
#[derive(Clone)]
struct ResultsView {
    /// The rows themselves.
    list: gtk::ListBox,
    /// The card that wraps the rows; always allocated to keep the window stable.
    scroller: gtk::ScrolledWindow,
    /// Where (if anywhere) the placeholder preview column is drawn.
    preview: PreviewPosition,
    /// Preview panel container
    preview_panel: gtk::Box,
    /// Preview scroll
    preview_scroll: gtk::ScrolledWindow,
    /// Preview label for text content
    preview_label: gtk::Label,
    /// Preview picture for images
    preview_picture: gtk::Picture,
}

/// A reusable result row. Rows are created once and then re-bound to new
/// items, so keystrokes never rebuild the widget tree.
struct ResultRow {
    row: gtk::ListBoxRow,
    icon: gtk::Image,
    name: gtk::Label,
    path: gtk::Label,
    /// Shortcut pill ("Alt + 1" ... "Alt + 0") shown on the first ten rows.
    shortcut: gtk::Label,
}

impl ResultRow {
    fn new(_preview: PreviewPosition) -> Self {
        let row = gtk::ListBoxRow::new();
        row.add_css_class("app-row");

        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        content.add_css_class("app-content");
        content.set_valign(gtk::Align::Center);

        let icon = gtk::Image::new();
        icon.set_pixel_size(24);
        icon.add_css_class("app-icon");
        content.append(&icon);

        let labels = gtk::Box::new(gtk::Orientation::Vertical, 2);
        labels.set_valign(gtk::Align::Center);
        labels.set_hexpand(true);

        let name = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Middle)
            .build();
        name.add_css_class("app-title");

        let path = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(pango::EllipsizeMode::Middle)
            .build();
        path.add_css_class("app-sub");

        labels.append(&name);
        labels.append(&path);
        content.append(&labels);

        let shortcut = gtk::Label::new(None);
        shortcut.add_css_class("app-pill");
        content.append(&shortcut);

        row.set_child(Some(&content));

        Self {
            row,
            icon,
            name,
            path,
            shortcut,
        }
    }

    /// Re-bind this row to `item`, showing the shortcut pill on the first ten.
    fn bind(&self, item: &Item, index: usize) {
        let name = item.name();
        let subtitle = item.subtitle();
        self.name.set_label(&name);
        self.path.set_label(&subtitle);
        self.icon.set_from_gicon(&item_icon(item));

        if let Some(label) = SHORTCUTS.get(index) {
            self.shortcut.set_label(label);
            self.shortcut.set_visible(true);
        } else {
            self.shortcut.set_label("");
            self.shortcut.set_visible(false);
        }

    }

    fn bind_math(&self, answer: &str, expression: &str, index: usize) {
        self.name.set_label(answer);
        self.path.set_label(expression);
        self.icon.clear();
        if let Some(label) = SHORTCUTS.get(index) {
            self.shortcut.set_label(label);
            self.shortcut.set_visible(true);
        }
    }
}



/// Build and present the Trandex overlay for `app`.
///
/// The path to the first frame is deliberately small: the window, its
/// layer-shell placement, the embedded CSS, the search row and an *empty,
/// hidden* results container — then `present()` and focus. Everything else
/// (the menu model, keyboard shortcuts, the debounced filter, and indexing)
/// runs in an idle source that is only dispatched after the compositor has
/// painted that first frame.
pub fn build(
    config: Rc<Config>,
    config_path: PathBuf,
    socket_path: PathBuf,
    origin: Instant,
    main_loop: glib::MainLoop,
) {
    let t0 = std::time::Instant::now();
    tick(t0, "build start");
    let window = gtk::Window::builder()
        .title("Trandex")
        .decorated(false)
        .resizable(false)
        .build();
    tick(t0, "ApplicationWindow built");
    let container = gtk::Box::new(gtk::Orientation::Vertical, 10);
    let white_surface = gtk::DrawingArea::new();
    white_surface.set_draw_func(|_, context, width, height| {
        context.set_source_rgb(1.0, 1.0, 1.0);
        context.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
        let _ = context.fill();
    });
    white_surface.set_content_width(1);
    white_surface.set_content_height(1);
    white_surface.set_hexpand(true);
    white_surface.set_vexpand(true);
    container.append(&white_surface);
    window.set_child(Some(&container));
    tick(t0, "window.set_child");
    // Layer-shell must be initialized before GTK realizes the window.
    window.init_layer_shell();
    window.connect_close_request(move |window| {
        window.hide();
        glib::Propagation::Stop
    });
    eprintln!(
        "Constructed window in {}ms",
        origin.elapsed().as_millis()
    );
    setup_full(window, config, config_path, socket_path, origin, t0, main_loop);
}

fn setup_full(
    window: gtk::Window,
    config: Rc<Config>,
    config_path: PathBuf,
    socket_path: PathBuf,
    origin: Instant,
    t0: std::time::Instant,
    main_loop: glib::MainLoop,
) {
        apply_layer_shell(&window, config.window_position());
        tick(t0, "apply_layer_shell");
        install_css(config.styles.as_deref(), config_path.parent());
        tick(t0, "install_css");
        window.add_css_class("app-screen");
        hook_first_frame(&window, origin);
        let presented = Instant::now();
        let state = Rc::new(RefCell::new(State::default()));
        state.borrow_mut().providers = config.providers();
        let pool: Rc<RefCell<Vec<ResultRow>>> = Rc::new(RefCell::new(Vec::new()));
        let generation = Rc::new(Cell::new(0u64));
        let preview_cache = Rc::new(RefCell::new(PreviewCache::new()));
        let (filter_tx, filter_rx) = start_filter_worker(
            state.borrow().apps.clone(),
            state.borrow().providers.contains(&Provider::AppList),
        );
        let file_cancel = Arc::new(AtomicU64::new(0));
        let (file_tx, file_rx) = start_file_query_worker(config.clone(), file_cancel.clone());
        let container = window.child().and_downcast::<gtk::Box>().unwrap();
        if let Some(white_surface) = container.first_child() {
            container.remove(&white_surface);
        }
        container.add_css_class("app-window");
        container.set_halign(gtk::Align::Center);
        let search_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        search_row.add_css_class("app-search-row");
        let search = gtk::Entry::builder()
            .placeholder_text(config.placeholder())
            .hexpand(true)
            .build();
        search.add_css_class("app-search");
        search_row.append(&search);
        let spinner = gtk::Spinner::builder()
            .width_request(16)
            .height_request(16)
            .visible(false)
            .build();
        spinner.add_css_class("app-spinner");
        search_row.append(&spinner);
        container.append(&search_row);
        let menu_button = gtk::MenuButton::builder().label("⋮").build();
        menu_button.set_always_show_arrow(false);
        menu_button.add_css_class("app-menu");
        search_row.append(&menu_button);
        tick(t0, "container/search/menu_button construction");
        let reload_loop = main_loop.clone();
        let reload_socket = socket_path.clone();
        install_socket(
            socket_path,
            window.clone(),
            search.clone(),
            main_loop,
            origin,
        );

        let results = gtk::ListBox::new();
        results.add_css_class("app-list");
        results.set_selection_mode(gtk::SelectionMode::Single);
        results.set_activate_on_single_click(false);

        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&results)
            .build();
        scroller.add_css_class("app-results-scroll");
        scroller.set_vexpand(true);
        scroller.set_opacity(0.0);

        let preview_label = gtk::Label::builder()
            .xalign(0.0)
            .selectable(false)
            .wrap(false)
            .build();
        preview_label.add_css_class("app-preview-label");
        preview_label.set_visible(false);

        let preview_picture = gtk::Picture::new();
        preview_picture.add_css_class("app-preview-picture");
        preview_picture.set_visible(false);

        let preview_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        preview_box.append(&preview_picture);
        preview_box.append(&preview_label);

        let preview_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .child(&preview_box)
            .build();
        preview_scroll.add_css_class("app-preview-scroll");
        preview_scroll.set_vexpand(true);

        let preview_panel = gtk::Box::new(gtk::Orientation::Vertical, 0);
        preview_panel.add_css_class("app-preview");
        preview_panel.set_vexpand(true);
        preview_panel.append(&preview_scroll);
        let mut view = ResultsView {
            list: results,
            scroller,
            preview: PreviewPosition::None,
            preview_panel: preview_panel.clone(),
            preview_scroll: preview_scroll.clone(),
            preview_label: preview_label.clone(),
            preview_picture: preview_picture.clone(),
        };

        let results_area = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        results_area.add_css_class("app-results-area");
        results_area.set_vexpand(true);
        let window_width = style_width();
        let half_width = window_width / 2;
        results_area.set_width_request(window_width);
        preview_panel.set_width_request(half_width);
        if debug_enabled() {
            debug_log(&format!(
                "computed widths: window={}px, results-area={}px, preview={}px",
                window_width, window_width, half_width
            ));
        }
        let preview_pos = config.preview_position();
        if preview_pos == PreviewPosition::Left {
            results_area.append(&preview_panel);
            results_area.append(&view.scroller);
        } else if preview_pos == PreviewPosition::Right {
            results_area.append(&view.scroller);
            results_area.append(&preview_panel);
        } else {
            results_area.append(&view.scroller);
        }
        view.scroller.set_hexpand(true);
        view.scroller.set_vexpand(true);
        preview_panel.set_hexpand(false);
        container.append(&results_area);

        view.preview = config.preview_position();
        set_preview_visibility(&view, false);

        {
            let state = Rc::clone(&state);
            let pool = Rc::clone(&pool);
            let view = view.clone();
            let preview_cache = Rc::clone(&preview_cache);
            let generation = Rc::clone(&generation);
            glib::spawn_future_local(async move {
                while let Ok(reply) = filter_rx.recv().await {
                    if reply.generation != generation.get()
                        || reply.index_revision != state.borrow().index_revision {
                        continue;
                    }
                    debug_log(&format!(
                        "filter query generation={} elapsed_ms={}",
                        reply.generation,
                        reply.elapsed.as_secs_f64() * 1000.0
                    ));
                    refresh_with_shown(
                        &mut pool.borrow_mut(),
                        &view,
                        &state,
                        reply.generation,
                        preview_cache.clone(),
                        generation.clone(),
                        reply.shown,
                    );
                }
            });
        }
        {
            let state = Rc::clone(&state);
            let pool = Rc::clone(&pool);
            let view = view.clone();
            let preview_cache = Rc::clone(&preview_cache);
            let generation = Rc::clone(&generation);
            let spinner = spinner.clone();
            glib::spawn_future_local(async move {
                while let Ok(reply) = file_rx.recv().await {
                    if reply.generation != generation.get() {
                        continue;
                    }
                    debug_log(&format!(
                        "file rows query generation={} done={} elapsed_ms={} total_since_keystroke_ms={}",
                        reply.generation,
                        reply.done,
                        reply.elapsed.as_secs_f64() * 1000.0,
                        reply.total_elapsed.as_secs_f64() * 1000.0
                    ));
                    if reply.done {
                        spinner.stop();
                        spinner.set_visible(false);
                        debug_log(&format!(
                            "file spinner stopped generation={}",
                            reply.generation
                        ));
                    }
                    let mut state_ref = state.borrow_mut();
                    state_ref.file_results.extend(reply.paths);
                    let shown = state_ref.shown.iter()
                        .copied()
                        .filter(|item| matches!(item, ItemRef::App(_) | ItemRef::Command(_)))
                        .collect::<Vec<_>>();
                    drop(state_ref);
                    refresh_with_shown(
                        &mut pool.borrow_mut(),
                        &view,
                        &state,
                        reply.generation,
                        preview_cache.clone(),
                        generation.clone(),
                        shown,
                    );
                }
            });
        }

        log_ms(origin, "indexing started");
        start_indexing(
            Rc::clone(&state),
            Rc::clone(&pool),
            view.clone(),
            Rc::clone(&config),
            preview_cache.clone(),
            generation.clone(),
            presented,
            origin,
        );

        // --- Triple-dot menu ------------------------------------------------
        let about_action = gio::SimpleAction::new("about", None);
        {
            let about_window = window.clone();
            about_action.connect_activate(move |_, _| {
                let about = gtk::AboutDialog::builder()
                    .program_name("Trandex")
                    .version(env!("CARGO_PKG_VERSION"))
                    .comments("A fast, floating file launcher for Wayland.")
                    .build();
                about.set_transient_for(Some(&about_window));
                about.present();
            });
        }
        let menu = gio::Menu::new();
        let reload_action = gio::SimpleAction::new("reload", None);
        {
            let reload_config = config_path.clone();
            let reload_socket = reload_socket.clone();
            let reload_loop = reload_loop.clone();
            reload_action.connect_activate(move |_, _| {
                match restart_daemon(&reload_config, &reload_socket) {
                    Ok(()) => reload_loop.quit(),
                    Err(error) => eprintln!("trandex: could not reload: {error}"),
                }
            });
        }
        let settings_action = gio::SimpleAction::new("settings", None);
        {
            let settings_path = config_path.clone();
            settings_action.connect_activate(move |_, _| {
                let uri = gio::File::for_path(&settings_path).uri();
                if let Err(error) =
                    gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
                {
                    eprintln!("trandex: could not open settings: {error}");
                }
            });
        }
        let action_group = gio::SimpleActionGroup::new();
        action_group.add_action(&about_action);
        action_group.add_action(&reload_action);
        action_group.add_action(&settings_action);
        window.insert_action_group("win", Some(&action_group));
        menu.append(Some("Reload"), Some("win.reload"));
        menu.append(Some("Settings"), Some("win.settings"));
        menu.append(Some("About"), Some("win.about"));
        menu_button.set_menu_model(Some(&menu));

        // --- Debounce keystrokes --------------------------------------------
        {
            let state = Rc::clone(&state);
            let _pool = Rc::clone(&pool);
            let view = view.clone();
            let generation = Rc::clone(&generation);
            let _preview_cache = Rc::clone(&preview_cache);
            let file_tx = file_tx.clone();
            let spinner = spinner.clone();
            search.connect_changed(move |entry| {
                let text = entry.text().to_string();
                let current = generation.get().wrapping_add(1);
                generation.set(current);
                file_cancel.store(current, Ordering::Relaxed);
                if text.trim().is_empty() {
                    set_preview_visibility(&view, false);
                    spinner.stop();
                    spinner.set_visible(false);
                    debug_log("file spinner stopped: empty query");
                } else {
                    spinner.set_visible(true);
                    spinner.start();
                    debug_log(&format!("file spinner started generation={current}"));
                }
                state.borrow_mut().file_results.clear();
                state.borrow_mut().query = text.clone();
                let index_revision = state.borrow().index_revision;
                let _ = filter_tx.send_blocking(FilterRequest {
                    generation: current,
                    query: text.clone(),
                    index_revision,
                });
                if !text.trim().is_empty()
                    && state.borrow().providers.contains(&Provider::Index) && !text.trim().is_empty() {
                        let _ = file_tx.send_blocking(FileQueryRequest {
                            generation: current,
                            query: text,
                            sent_at: Instant::now(),
                        });
                    }
            });
        }
        {
            let state = Rc::clone(&state);
            let view = view.clone();
            let generation = Rc::clone(&generation);
            let preview_cache = Rc::clone(&preview_cache);
            let preview_view = view.clone();
            view.list.connect_row_selected(move |_, _| {
                let shown = state.borrow().shown.clone();
                update_preview(
                    &preview_view,
                    &state,
                    &shown,
                    generation.get(),
                    preview_cache.clone(),
                    generation.clone(),
                );
            });
        }
        {
            let state = Rc::clone(&state);
            let view = view.clone();
            let window_for_activate = window.clone();
            search.connect_activate(move |_| {
                activate_selected(&view, &window_for_activate, &state);
            });
        }

        // --- Keyboard navigation on the window ------------------------------
        let key_controller = gtk::EventControllerKey::new();
        {
            let state = Rc::clone(&state);
            let view = view.clone();
            let window_for_keys = window.clone();
            let search_for_keys = search.clone();
            key_controller.connect_key_pressed(move |_, keyval, _, modifiers| {
                if keyval == gdk::Key::Escape {
                    window_for_keys.close();
                    glib::Propagation::Stop
                } else if modifiers.contains(gdk::ModifierType::ALT_MASK) {
                    // Alt + 1..9 open the first nine results, Alt + 0 the tenth.
                    // GTK treats Alt as a consumed modifier, so the entry never
                    // inserts the digit itself and the event bubbles to us.
                    if let Some(index) = shortcut_index(keyval) {
                        launch_shown(&window_for_keys, &state, index);
                        glib::Propagation::Stop
                    } else {
                        glib::Propagation::Proceed
                    }
                } else if !search_for_keys.has_focus()
                    && (keyval == gdk::Key::BackSpace || keyval == gdk::Key::Delete)
                {
                    search_for_keys.grab_focus();
                    let position = search_for_keys.position();
                    if keyval == gdk::Key::BackSpace && position > 0 {
                        search_for_keys.delete_text(position - 1, position);
                        search_for_keys.set_position(position - 1);
                    } else if keyval == gdk::Key::Delete {
                        let end = search_for_keys.text().len() as i32;
                        if position < end {
                            search_for_keys.delete_text(position, position + 1);
                            search_for_keys.set_position(position);
                        }
                    }
                    glib::Propagation::Stop
                } else if keyval == gdk::Key::Up {
                    let count = state.borrow().shown.len();
                    move_selection(&view.list, count, -1);
                    glib::Propagation::Stop
                } else if keyval == gdk::Key::Down {
                    let count = state.borrow().shown.len();
                    move_selection(&view.list, count, 1);
                    glib::Propagation::Stop
                } else if keyval == gdk::Key::Return || keyval == gdk::Key::KP_Enter {
                    activate_selected(&view, &window_for_keys, &state);
                    glib::Propagation::Stop
                } else if !search_for_keys.has_focus()
                    && !modifiers.intersects(
                        gdk::ModifierType::CONTROL_MASK
                            | gdk::ModifierType::ALT_MASK
                            | gdk::ModifierType::SUPER_MASK,
                    )
                    && keyval
                        .to_unicode()
                        .filter(|character| character.is_alphanumeric())
                        .is_some()
                {
                    let character = keyval.to_unicode().expect("validated printable key");
                    search_for_keys.grab_focus();
                    let mut text = search_for_keys.text().to_string();
                    text.push(character);
                    search_for_keys.set_text(&text);
                    search_for_keys.set_position(-1);
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
        }
        window.add_controller(key_controller);
        tick(t0, "window.add_controller");
    eprintln!("Setup complete in {}ms", origin.elapsed().as_millis());
}

/// Spawn a fresh daemon process and take the control socket down so the
/// replacement can bind it. The caller quits the current main loop afterwards.
fn restart_daemon(config_path: &Path, socket_path: &Path) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    // Unlink before spawning: the current listener stays alive until the main
    // loop quits, and leaving the file behind would make the child believe a
    // daemon is already running.
    let _ = std::fs::remove_file(socket_path);
    Command::new(exe)
        .arg("--daemon")
        .arg("--config")
        .arg(config_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

fn install_socket(
    path: PathBuf,
    window: gtk::Window,
    search: gtk::Entry,
    main_loop: glib::MainLoop,
    origin: Instant,
) {
    if path.exists() {
        match UnixStream::connect(&path) {
            Ok(_) => {
                eprintln!("trandex: daemon already running at {}", path.display());
                std::process::exit(1);
            }
            Err(_) => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    let listener = match UnixListener::bind(&path) {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("trandex: could not bind {}: {error}", path.display());
            main_loop.quit();
            return;
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        eprintln!("trandex: could not configure socket: {error}");
        let _ = std::fs::remove_file(&path);
        main_loop.quit();
        return;
    }
    let fd = listener.as_raw_fd();
    glib21::source::unix_fd_add_local(
        fd,
        glib21::IOCondition::IN | glib21::IOCondition::HUP | glib21::IOCondition::ERR,
        move |_, condition| {
            if condition.intersects(glib21::IOCondition::HUP | glib21::IOCondition::ERR) {
                return glib21::ControlFlow::Continue;
            }
            loop {
                let Ok((mut stream, _)) = listener.accept() else {
                    break;
                };
                let mut command = String::new();
                let _ = stream.read_to_string(&mut command);
                match command.lines().next().unwrap_or_default() {
                    "toggle" => {
                        debug_log_at(origin, "toggle received");
                        if window.is_visible() {
                            window.hide();
                            search.set_text("");
                        } else {
                            window.present();
                            search.grab_focus();
                            debug_log_at(origin, "window visible");
                        }
                    }
                    "show" => {
                        window.present();
                        search.grab_focus();
                        debug_log_at(origin, "window visible");
                    }
                    "hide" => {
                        window.hide();
                        search.set_text("");
                    }
                    "quit" => {
                        window.hide();
                        let _ = std::fs::remove_file(&path);
                        main_loop.quit();
                        return glib21::ControlFlow::Break;
                    }
                    _ => {}
                }
            }
            glib21::ControlFlow::Continue
        },
    );
}

fn debug_log_at(origin: Instant, message: &str) {
    if debug_enabled() {
        eprintln!("[{:>5} ms] {message}", origin.elapsed().as_millis());
    }
}

/// Log the ms-since-main timestamp of the first painted frame, exactly once.
///
/// `frame_clock()` is only available once the window is mapped, and `present()`
/// maps asynchronously, so `after-paint` is connected from within the window's
/// `map` handler. The `Cell` guard swallows every later frame.
fn hook_first_frame(window: &gtk::Window, origin: Instant) {
    window.connect_map(move |window| {
        if let Some(frame_clock) = window.frame_clock() {
            let logged = Rc::new(Cell::new(false));
            frame_clock.connect_after_paint(move |_| {
                if !logged.get() {
                    logged.set(true);
                    log_ms(origin, "first frame drawn");
                }
            });
        }
    });
}

/// Load the launcher's monochrome stylesheet.
fn install_css(styles: Option<&Path>, config_dir: Option<&Path>) {
    if let Some(display) = gdk::Display::default() {
        let configured = styles.map(|styles| {
            if styles.is_absolute() {
                styles.to_path_buf()
            } else {
                config_dir.map_or_else(|| styles.to_path_buf(), |dir| dir.join(styles))
            }
        });
        let configured = configured.and_then(|path| {
            if path.is_file() {
                Some(path)
            } else if path.file_name().and_then(|name| name.to_str()) == Some("basic.css") {
                let candidate = config_dir
                    .and_then(|dir| dir.parent())
                    .map(|root| root.join("src/themes/basic.css"));
                candidate.filter(|candidate| candidate.is_file()).unwrap_or(path).into()
            } else {
                Some(path)
            }
        });
        if let Some(path) = configured.as_ref().filter(|path| path.is_file()) {
            eprintln!("trandex: using stylesheet {}", path.display());
            let provider = gtk::CssProvider::new();
            provider.load_from_path(path);
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        } else {
            if let Some(path) = configured {
                eprintln!(
                    "trandex: stylesheet {} not found; using themes/basic.css",
                    path.display()
                );
            }
            let provider = gtk::CssProvider::new();
            provider.load_from_data(BASIC_STYLE);
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
        }
        // GTK's built-in theme paints an accent-colored focus outline around
        // the focused entry. Load this above every other provider so it is
        // always suppressed, regardless of the active theme or user CSS.
        let focus_reset = gtk::CssProvider::new();
        focus_reset.load_from_data(
            "entry:focus, entry:focus-within { outline: none; box-shadow: none; }",
        );
        gtk::style_context_add_provider_for_display(
            &display,
            &focus_reset,
            gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
        );
    }
}

/// Configure `window` as a floating layer-shell overlay.
fn apply_layer_shell(window: &gtk::Window, position: WindowPosition) {
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some("trandex"));
    window.set_keyboard_mode(KeyboardMode::Exclusive);
    window.set_anchor(Edge::Top, matches!(position, WindowPosition::Top));
    window.set_anchor(Edge::Bottom, false);
    window.set_anchor(Edge::Left, matches!(position, WindowPosition::Left));
    window.set_anchor(Edge::Right, matches!(position, WindowPosition::Right));
    window.set_exclusive_zone(0);
    match position {
        WindowPosition::Top => window.set_margin(Edge::Top, TOP_MARGIN),
        WindowPosition::Left => window.set_margin(Edge::Left, 16),
        WindowPosition::Right => window.set_margin(Edge::Right, 16),
        WindowPosition::Center => {
            window.set_margin(Edge::Top, 0);
            window.set_margin(Edge::Bottom, 0);
            window.set_margin(Edge::Left, 0);
            window.set_margin(Edge::Right, 0);
        }
    }
}

fn style_width() -> i32 {
    BASIC_STYLE.lines()
        .find_map(|line| {
            let value = line.trim().strip_prefix("min-width:")?.trim().strip_suffix("px")?;
            value.parse().ok()
        })
        .unwrap_or(720)
}

fn start_filter_worker(
    apps: Arc<RwLock<Vec<Item>>>,
    apps_enabled: bool,
) -> (async_channel::Sender<FilterRequest>, async_channel::Receiver<FilterReply>) {
    let (requests, request_rx) = async_channel::unbounded::<FilterRequest>();
    let (reply_tx, replies) = async_channel::unbounded::<FilterReply>();
    std::thread::spawn(move || {
        while let Ok(request) = request_rx.recv_blocking() {
            let started = Instant::now();
            let apps_guard = apps.read().unwrap();
            let shown = if apps_enabled {
                filter(&apps_guard, &[], &request.query, MAX_RESULTS)
            } else {
                Vec::new()
            };
            if shown.is_empty() {
                debug_zero_sources(&request.query, &apps_guard, &[]);
            }
            let _ = reply_tx.send_blocking(FilterReply {
                generation: request.generation,
                shown,
                elapsed: started.elapsed(),
                index_revision: request.index_revision,
            });
            if debug_enabled() && request.query.trim().eq_ignore_ascii_case("main.rs") {
                debug_log(&format!(
                    "query={:?} app filter returned no file index (files are query-local)",
                    request.query
                ));
            }
        }
    });
    (requests, replies)
}

fn start_file_query_worker(
    config: Rc<Config>,
    cancel_generation: Arc<AtomicU64>,
) -> (
    async_channel::Sender<FileQueryRequest>,
    async_channel::Receiver<FileReply>,
) {
    let (tx, rx) = async_channel::unbounded::<FileQueryRequest>();
    let (reply_tx, replies) = async_channel::unbounded::<FileReply>();
    let hidden = config.hidden();
    let ignore = config.ignore_dirs();
    std::thread::spawn(move || {
        while let Ok(request) = rx.recv_blocking() {
            let started = Instant::now();
            let home = std::env::var_os("HOME").map(PathBuf::from);
            let Some(home) = home else { continue };
            let mut args = vec![
                "--type".into(), "f".into(), "--absolute-path".into(),
                "--color=never".into(),
            ];
            if hidden { args.push("--hidden".into()); }
            for entry in &ignore {
                args.push("--exclude".into());
                args.push(entry.clone());
            }
            args.extend([".".into(), home.to_string_lossy().into_owned()]);
            let mut child = match Command::new("fd")
                .args(&args)
                .stdout(std::process::Stdio::piped())
                .spawn()
            {
                Ok(child) => child,
                Err(_) => {
                    let mut find_args = vec![home.to_string_lossy().into_owned()];
                    for entry in &ignore {
                        find_args.extend(["!".into(), "-path".into(), format!("*/{entry}/*")]);
                    }
                    find_args.extend(["-type".into(), "f".into(), "-print".into()]);
                    match Command::new("find").args(&find_args).stdout(std::process::Stdio::piped()).spawn() {
                        Ok(child) => child,
                        Err(_) => continue,
                    }
                }
            };
            let stdout = child.stdout.take();
            let mut name_matches = Vec::new();
            let mut path_matches = Vec::new();
            if let Some(stdout) = stdout {
                use std::io::{BufRead, BufReader};
                for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                    if cancel_generation.load(Ordering::Relaxed) != request.generation {
                        let _ = child.kill();
                        break;
                    }
                    let path = PathBuf::from(line);
                    let name = path.file_name().map(|value| value.to_string_lossy()).unwrap_or_default();
                    if contains_ci(&name, &request.query) {
                        name_matches.push(path);
                    } else if contains_ci(&path.to_string_lossy(), &request.query) {
                        path_matches.push(path);
                    } else {
                        continue;
                    }
                    if name_matches.len() + path_matches.len() >= MAX_RESULTS {
                        let _ = child.kill();
                        break;
                    }
                    if !name_matches.is_empty() {
                        let batch = std::mem::take(&mut name_matches);
                        let _ = reply_tx.send_blocking(FileReply {
                            generation: request.generation,
                            paths: batch,
                            done: false,
                            elapsed: started.elapsed(),
                            total_elapsed: request.sent_at.elapsed(),
                        });
                    }
                }
            }
            let mut paths = name_matches;
            paths.extend(path_matches);
            paths.truncate(MAX_RESULTS);
            let _ = reply_tx.send_blocking(FileReply {
                generation: request.generation,
                paths,
                done: true,
                elapsed: started.elapsed(),
                total_elapsed: request.sent_at.elapsed(),
            });
        }
    });
    (tx, replies)
}

fn rank_paths(paths: Vec<PathBuf>, query: &str) -> Vec<PathBuf> {
    let mut starts = Vec::new();
    let mut names = Vec::new();
    let mut contains = Vec::new();
    for path in paths {
        let name = path.file_name().map(|v| v.to_string_lossy()).unwrap_or_default();
        if starts_with_ci(&name, query) {
            starts.push(path);
        } else if contains_ci(&name, query) {
            names.push(path);
        } else if contains_ci(&path.to_string_lossy(), query) {
            contains.push(path);
        }
    }
    starts.into_iter().chain(names).chain(contains).take(MAX_RESULTS).collect()
}

fn filter_refs(
    apps: &Arc<RwLock<Vec<Item>>>,
    files: &Arc<RwLock<Vec<Item>>>,
    refs: &[ItemRef],
    query: &str,
    limit: usize,
) -> Vec<ItemRef> {
    let apps = apps.read().unwrap();
    let files = files.read().unwrap();
    let mut starts = Vec::new();
    let mut names = Vec::new();
    let mut paths = Vec::new();
    for &item_ref in refs {
        let item = match item_ref {
            ItemRef::App(i) => apps.get(i),
            ItemRef::Command(i) => apps.get(i),
            ItemRef::Math => None,
            ItemRef::File(i) => files.get(i),
        };
        let Some(item) = item else { continue };
        let name = item.name();
        let path = item.subtitle();
        let target = if starts_with_ci(&name, query) {
            &mut starts
        } else if contains_ci(&name, query) {
            &mut names
        } else if contains_ci(&path, query) {
            &mut paths
        } else {
            continue
        };
        target.push(item_ref);
    }
    starts.into_iter().chain(names).chain(paths).take(limit).collect()
}

/// Start the file and application indexers on worker threads and fold their
/// batches in through one channel on the main thread.
///
/// Called from an idle source, so `present()` and `grab_focus()` have already
/// run. `.desktop` parsing and the file walk (including `Item` construction,
/// lowercasing and the final sort) all happen off the GTK thread; GIO types are
/// only touched here and in `launch()`.
fn start_indexing(
    state: Rc<RefCell<State>>,
    pool: Rc<RefCell<Vec<ResultRow>>>,
    view: ResultsView,
    config: Rc<Config>,
    preview_cache: Rc<RefCell<PreviewCache>>,
    generation: Rc<Cell<u64>>,
    presented: Instant,
    origin: Instant,
) {
    let (sender, receiver) = async_channel::unbounded::<IndexMessage>();
    let apps_enabled = config.provider_enabled(Provider::AppList);

    // --- Apps --------------------------------------------------------------
    let apps_sender = sender.clone();
    std::thread::spawn(move || {
        if !apps_enabled {
            return;
        }
        // Timed under TRANDEX_DEBUG for comparison against the .desktop parse.
        // AppInfo::all() measured ~30 ms warm / ~75 ms cold on this machine,
        // which is over the 50 ms budget, so the parse below is the real path.
        if debug_enabled() {
            let started = Instant::now();
            let total = gio::AppInfo::all().len();
            debug_log(&format!(
                "gio::AppInfo::all() listed {total} entries in {:?}",
                started.elapsed()
            ));
        }

        let started = Instant::now();
        let mut apps: Vec<Item> = indexer::desktop_apps()
            .into_iter()
            .filter_map(Item::app)
            .collect();
        let command_start = apps.len();
        apps.extend(indexer::path_commands().into_iter().map(|path| Item {
            source: Source::Command(path),
        }));
        debug_log(&format!(
            "desktop app parse complete: {} apps in {:?}",
            command_start,
            started.elapsed()
        ));
        let _ = apps_sender.send_blocking(IndexMessage::Apps(apps));
        let _ = apps_sender.send_blocking(IndexMessage::AppsDone);
    });

    // Keep only the workers' senders and the drainer's receiver alive.
    drop(sender);

    glib::spawn_future_local(drain_index(
        receiver,
        state,
        pool,
        view,
        preview_cache,
        generation,
        presented,
        origin,
    ));
}

fn start_file_indexing(
    state: Rc<RefCell<State>>,
    pool: Rc<RefCell<Vec<ResultRow>>>,
    view: ResultsView,
    config: Rc<Config>,
    preview_cache: Rc<RefCell<PreviewCache>>,
    generation: Rc<Cell<u64>>,
    origin: Instant,
) {
    let _ = (state, pool, view, config, preview_cache, generation, origin);
}

/// Fold index batches into `State` and refresh, one main-loop wakeup at a time.
///
/// While batches are still streaming in, refreshes are throttled to one per
/// `REFRESH_THROTTLE_MS` when the user has typed something — the filter over a
/// growing index is the expensive step, so a burst of batches must not turn
/// into a burst of filter passes. An empty query refreshes freely (it is
/// nearly free). App batches trigger one final refresh.
async fn drain_index(
    receiver: Receiver<IndexMessage>,
    state: Rc<RefCell<State>>,
    pool: Rc<RefCell<Vec<ResultRow>>>,
    view: ResultsView,
    preview_cache: Rc<RefCell<PreviewCache>>,
    generation: Rc<Cell<u64>>,
    presented: Instant,
    origin: Instant,
) {
    let _ = (pool, view, preview_cache, generation, presented, origin);
    while let Ok(message) = receiver.recv().await {
        if let IndexMessage::Apps(items) = message {
            let mut state_ref = state.borrow_mut();
            state_ref.apps.write().unwrap().extend(items);
            state_ref.index_revision = state_ref.index_revision.wrapping_add(1);
        }
    }
}

#[derive(Clone, Debug)]
enum PreviewContent {
    Text(String),
    Dir(String),
}

const PREVIEW_CACHE_LIMIT: usize = 4 * 1024 * 1024;

struct PreviewCache {
    entries: HashMap<PathBuf, PreviewContent>,
    order: VecDeque<PathBuf>,
    bytes: usize,
}

impl PreviewCache {
    fn new() -> Self {
        Self { entries: HashMap::new(), order: VecDeque::new(), bytes: 0 }
    }

    fn size(content: &PreviewContent) -> usize {
        match content {
            PreviewContent::Text(text) | PreviewContent::Dir(text) => text.len(),
        }
    }

    fn get(&mut self, path: &Path) -> Option<PreviewContent> {
        let content = self.entries.get(path)?.clone();
        self.order.retain(|key| key != path);
        self.order.push_back(path.to_path_buf());
        Some(content)
    }

    fn insert(&mut self, path: PathBuf, content: PreviewContent) {
        let size = Self::size(&content);
        if let Some(old) = self.entries.remove(&path) {
            self.bytes = self.bytes.saturating_sub(Self::size(&old));
            self.order.retain(|key| key != &path);
        }
        if size > PREVIEW_CACHE_LIMIT {
            return;
        }
        while self.bytes + size > PREVIEW_CACHE_LIMIT {
            let Some(old_path) = self.order.pop_front() else { break };
            if let Some(old) = self.entries.remove(&old_path) {
                self.bytes = self.bytes.saturating_sub(Self::size(&old));
            }
        }
        self.bytes += size;
        self.order.push_back(path.clone());
        self.entries.insert(path, content);
    }
}

/// Apply the current query to the results list, reusing rows where possible.
fn refresh(
    pool: &mut Vec<ResultRow>,
    view: &ResultsView,
    state: &Rc<RefCell<State>>,
    generation: u64,
    preview_cache: Rc<RefCell<PreviewCache>>,
    current_generation: Rc<Cell<u64>>,
) {
    let shown = {
        let state = state.borrow();
        let apps = state.apps.read().unwrap();
        filter(&apps, &[], &state.query, MAX_RESULTS)
    };
    refresh_with_shown(pool, view, state, generation, preview_cache, current_generation, shown);
}

fn refresh_with_shown(
    pool: &mut Vec<ResultRow>,
    view: &ResultsView,
    state: &Rc<RefCell<State>>,
    generation: u64,
    preview_cache: Rc<RefCell<PreviewCache>>,
    current_generation: Rc<Cell<u64>>,
    shown: Vec<ItemRef>,
) {
    let started = Instant::now();
    let expression = state.borrow().query.trim().to_string();
    let calculator = {
        let query = state.borrow().query.clone();
        calculator_answer(&query)
    };
    {
        let mut state_ref = state.borrow_mut();
        state_ref.math_answer = calculator.clone();
        state_ref.math_expression = calculator.as_ref().map(|_| expression.clone());
    }
    let shown = {
        let state_ref = state.borrow();
        compose_shown(&state_ref, shown)
    };

    let query_nonempty = !state.borrow().query.trim().is_empty();
    let has_results = !shown.is_empty();
    set_preview_visibility(view, query_nonempty);
    view.scroller.set_opacity(if has_results { 1.0 } else { 0.0 });

    // Nothing changed, so avoid touching any widgets.
    if state.borrow().shown == shown {
        log_refresh(state, started);
        return;
    }

    let previous = view
        .list
        .selected_row()
        .map(|row| row.index() as usize)
        .and_then(|index| state.borrow().shown.get(index).copied());

    // Grow the row pool as needed; rows are appended once and then reused.
    while pool.len() < shown.len() {
        let slot = ResultRow::new(view.preview);
        view.list.append(&slot.row);
        pool.push(slot);
    }

    {
        let state = state.borrow();
        for (row_index, (slot, &item_ref)) in pool.iter().zip(&shown).enumerate() {
            match item_ref {
                ItemRef::Math => {
                    if let Some(answer) = &state.math_answer {
                        slot.bind_math(
                            answer,
                            state.math_expression.as_deref().unwrap_or(""),
                            row_index,
                        );
                        slot.row.set_visible(true);
                    }
                }
                _ => {
                    if let Some(item) = resolve(&state, item_ref) {
                        slot.bind(&item, row_index);
                        slot.row.set_visible(true);
                    }
                }
            }
        }
    }

    for slot in pool.iter().skip(shown.len()) {
        slot.row.set_visible(false);
    }

    let selected = previous
        .and_then(|item_ref| shown.iter().position(|&candidate| candidate == item_ref))
        .unwrap_or(0);
    state.borrow_mut().shown = shown.clone();

    if has_results {
        if let Some(row) = view.list.row_at_index(selected as i32) {
            view.list.select_row(Some(&row));
        }
    } else {
        view.list.unselect_all();
    }

    if query_nonempty && view.preview != PreviewPosition::None {
        update_preview(
            view,
            state,
            &shown,
            generation,
            preview_cache,
            current_generation,
        );
    }

    log_refresh(state, started);
}

fn compose_shown(state: &State, app_results: Vec<ItemRef>) -> Vec<ItemRef> {
    let mut result = Vec::new();
    for provider in &state.providers {
        match provider {
            Provider::AppList => result.extend(app_results.iter().copied()),
            Provider::Math if state.math_answer.is_some() => result.push(ItemRef::Math),
            Provider::Index => result.extend((0..state.file_results.len()).map(ItemRef::File)),
            _ => {}
        }
        if result.len() >= MAX_RESULTS {
            break;
        }
    }
    result.truncate(MAX_RESULTS);
    result
}

fn set_preview_visibility(view: &ResultsView, query_nonempty: bool) {
    let visible = query_nonempty && view.preview != PreviewPosition::None;
    view.preview_panel.set_opacity(if visible { 1.0 } else { 0.0 });
}

/// Look up an item by reference.
fn resolve(state: &State, item: ItemRef) -> Option<Item> {
    match item {
        ItemRef::App(index) => state.apps.read().unwrap().get(index).cloned(),
        ItemRef::Command(index) => state.apps.read().unwrap().get(index).cloned(),
        ItemRef::Math => None,
        ItemRef::File(index) => state.file_results.get(index).cloned().map(Item::file),
    }
}

/// Filter apps and files by `query`, ranking name matches above path-only
/// matches and apps above files within each rank.
///
/// An empty query yields nothing: the list stays hidden until the user types.
fn filter(apps: &[Item], files: &[Item], query: &str, limit: usize) -> Vec<ItemRef> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }

    let mut starts_with = Vec::new();
    let mut name_matches = Vec::new();
    let mut path_matches = Vec::new();

    for (index, item) in apps.iter().enumerate() {
        let name = item.name();
        let path = item.subtitle();
        let item_ref = if matches!(item.source, Source::Command(_)) {
            ItemRef::Command(index)
        } else {
            ItemRef::App(index)
        };
        if starts_with_ci(&name, query) {
            starts_with.push(item_ref);
        } else if contains_ci(&name, query) {
            name_matches.push(item_ref);
        } else if contains_ci(&path, query) {
            path_matches.push(item_ref);
        }
    }

    for (index, item) in files.iter().enumerate() {
        let name = item.name();
        let path = item.subtitle();
        if starts_with_ci(&name, query) {
            starts_with.push(ItemRef::File(index));
        } else if contains_ci(&name, query) {
            name_matches.push(ItemRef::File(index));
        } else if contains_ci(&path, query) {
            path_matches.push(ItemRef::File(index));
        }

    }

    starts_with
        .into_iter()
        .chain(name_matches)
        .chain(path_matches)
        .take(limit)
        .collect()
}

fn debug_zero_sources(query: &str, apps: &[Item], files: &[Item]) {
    if !debug_enabled() {
        return;
    }
    let app_matches = apps.iter().filter(|item| {
        contains_ci(&item.name(), query) || contains_ci(&item.subtitle(), query)
    }).count();
    let file_matches = files.iter().filter(|item| {
        contains_ci(&item.name(), query) || contains_ci(&item.subtitle(), query)
    }).count();
    if app_matches == 0 {
        debug_log(&format!("query zero results: source=apps query={query:?}"));
    }
    if app_matches == 0 && files.is_empty() {
        debug_log(&format!("query zero results: source=files query={query:?} (index empty)"));
    } else if file_matches == 0 {
        debug_log(&format!("query zero results: source=files query={query:?}"));
    }
}

fn update_preview(
    view: &ResultsView,
    state: &Rc<RefCell<State>>,
    shown: &[ItemRef],
    generation: u64,
    preview_cache: Rc<RefCell<PreviewCache>>,
    current_generation: Rc<Cell<u64>>,
) {
    let mut item_opt: Option<Source> = None;
    if let Some(selected_row) = view.list.selected_row() {
        let idx = selected_row.index() as usize;
        let state_borrow = state.borrow();
        if let Some(item_ref) = shown.get(idx) {
            item_opt = resolve(&state_borrow, *item_ref).map(|item| item.source.clone());
        }
    } else if !shown.is_empty() {
        let state_borrow = state.borrow();
        if let Some(item_ref) = shown.first() {
            item_opt = resolve(&state_borrow, *item_ref).map(|item| item.source.clone());
        }
    }

    let item = match item_opt {
        Some(i) => i,
        None => return,
    };

    match item {
        Source::App(entry) => {
            let content = format!("Name: {}\nID: {}\nExec: {}", entry.name, entry.id, entry.exec);
            view.preview_picture.set_visible(false);
            view.preview_label.set_visible(true);
            view.preview_label.set_label(&content);
        }
        Source::File(path) => {
            let path_buf = path.clone();
            if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                let ext_lower = ext.to_lowercase();
                if matches!(ext_lower.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg") {
                    view.preview_label.set_visible(false);
                    view.preview_picture.set_visible(true);
                    let file = gio::File::for_path(&path);
                    view.preview_picture.set_file(Some(&file));
                    view.preview_picture.set_can_shrink(true);
                    view.preview_picture.set_keep_aspect_ratio(true);
                    return;
                }
            }
            if path.is_dir() {
                load_dir_preview(
                    path_buf,
                    generation,
                    preview_cache,
                    view.preview_label.clone(),
                    view.preview_picture.clone(),
                    current_generation,
                );
                return;
            }
            load_text_preview(
                path_buf,
                generation,
                preview_cache,
                view.preview_label.clone(),
                view.preview_picture.clone(),
                current_generation,
            );
        }
        Source::Command(path) => {
            view.preview_picture.set_visible(false);
            view.preview_label.set_visible(true);
            view.preview_label.set_label(&format!("Command: {}", path.display()));
        }
    }
}

fn load_text_preview(
    path: PathBuf,
    generation: u64,
    preview_cache: Rc<RefCell<PreviewCache>>,
    label: gtk::Label,
    picture: gtk::Picture,
    current_generation: Rc<Cell<u64>>,
) {
    {
        let mut cache = preview_cache.borrow_mut();
        if let Some(PreviewContent::Text(text)) = cache.get(&path) {
            label.set_label(&text);
            picture.set_visible(false);
            label.set_visible(true);
            return;
            }
    }
    let path_clone = path.clone();
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let mut content = String::new();
        if let Ok(mut file) = std::fs::File::open(&path_clone) {
            let mut buf = Vec::new();
            let _ = file.read_to_end(&mut buf);
            let lines: Vec<String> = buf
                .split(|&b| b == b'\n')
                .take(200)
                .map(|s| String::from_utf8_lossy(s).to_string())
                .collect();
            content = lines.join("\n");
        }
        let _ = sender.send_blocking((path_clone, PreviewContent::Text(content)));
    });
    glib::spawn_future_local(async move {
        if let Ok((path, content)) = receiver.recv().await
            && generation == current_generation.get() {
                if let PreviewContent::Text(text) = &content {
                    label.set_label(text);
                    picture.set_visible(false);
                    label.set_visible(true);
                }
                preview_cache.borrow_mut().insert(path, content);
            }
    });
}

fn load_dir_preview(
    path: PathBuf,
    generation: u64,
    preview_cache: Rc<RefCell<PreviewCache>>,
    label: gtk::Label,
    picture: gtk::Picture,
    current_generation: Rc<Cell<u64>>,
) {
    {
        let mut cache = preview_cache.borrow_mut();
        if let Some(PreviewContent::Dir(text)) = cache.get(&path) {
            label.set_label(&text);
            picture.set_visible(false);
            label.set_visible(true);
            return;
            }
    }
    let path_clone = path.clone();
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let eza = Command::new("eza")
            .args(["-la", "--color=never"])
            .arg(&path_clone)
            .output();
        let output = if eza.is_ok() {
            eza
        } else {
            Command::new("ls").arg("-la").arg(&path_clone).output()
        };
        let text = output
            .map(|output| {
                let stdout = String::from_utf8_lossy(&output.stdout);
                if stdout.is_empty() {
                    String::from_utf8_lossy(&output.stderr).into_owned()
                } else {
                    stdout.into_owned()
                }
            })
            .unwrap_or_else(|error| format!("Could not list directory: {error}"));
        let _ = sender.send_blocking((path_clone, PreviewContent::Dir(text)));
    });
    glib::spawn_future_local(async move {
        if let Ok((path, content)) = receiver.recv().await
            && generation == current_generation.get() {
                if let PreviewContent::Dir(text) = &content {
                    label.set_label(text);
                    picture.set_visible(false);
                    label.set_visible(true);
                }
                preview_cache.borrow_mut().insert(path, content);
            }
    });
}

/// Resolve a themed symbolic icon for a file path.
///
/// Icons are cached per content type in a thread-local map, so binding many
/// rows of the same type (e.g. hundreds of `.rs` files) resolves it once.
fn content_icon(path: &Path) -> gio::Icon {
    let (content_type, _) = gio::content_type_guess(Some(path), None::<&[u8]>);

    ICON_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(content_type.to_string())
            .or_insert_with(|| gio::content_type_get_symbolic_icon(&content_type))
            .clone()
    })
}

/// Resolve the icon for an application from its desktop entry, cached per id.
///
/// The entry stores a themed icon *name* (from the `Icon=` key); `ThemedIcon`
/// resolves it against the theme, falling back to a generic executable icon.
fn app_icon(entry: &DesktopEntry) -> gio::Icon {
    APP_ICON_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .entry(entry.id.clone())
            .or_insert_with(|| {
                entry
                    .icon
                    .as_ref()
                    .map(|name| gio::ThemedIcon::new(name).upcast::<gio::Icon>())
                    .unwrap_or_else(|| {
                        gio::ThemedIcon::new("application-x-executable").upcast::<gio::Icon>()
                    })
            })
            .clone()
    })
}

/// The icon shown for a result row.
fn item_icon(item: &Item) -> gio::Icon {
    match &item.source {
        Source::File(path) => content_icon(path),
        Source::App(entry) => app_icon(entry),
        Source::Command(path) => content_icon(path),
    }
}

thread_local! {
    /// Content type -> symbolic icon.
    static ICON_CACHE: RefCell<HashMap<String, gio::Icon>> = RefCell::new(HashMap::new());
    /// Desktop id -> application icon.
    static APP_ICON_CACHE: RefCell<HashMap<String, gio::Icon>> = RefCell::new(HashMap::new());
}

/// Move the selection by `delta` rows, clamped to the visible result count.
fn move_selection(results: &gtk::ListBox, count: usize, delta: i32) {
    if count == 0 {
        return;
    }

    let count = count as i32;
    let current = results.selected_row().map(|row| row.index()).unwrap_or(-1);
    let next = (current + delta).clamp(0, count - 1);

    if let Some(row) = results.row_at_index(next) {
        results.select_row(Some(&row));
    }
}

/// Launch the selected result and close the overlay.
fn activate_selected(
    view: &ResultsView,
    window: &gtk::Window,
    state: &Rc<RefCell<State>>,
) {
    let index = view
        .list
        .selected_row()
        .map(|row| row.index() as usize)
        .or_else(|| (!state.borrow().shown.is_empty()).then_some(0));
    if let Some(index) = index {
        launch_shown(window, state, index);
    }
}

/// Launch the shown item at `shown_index` (0-based row position) and close the
/// overlay. Used by Enter and the Alt + digit shortcuts.
fn launch_shown(
    window: &gtk::Window,
    state: &Rc<RefCell<State>>,
    shown_index: usize,
) {
    let source = {
        let state = state.borrow();
        if state.shown.get(shown_index) == Some(&ItemRef::Math) {
            if let Some(value) = &state.math_answer
                && let Some(display) = gdk::Display::default() {
                    display.clipboard().set_text(value);
                }
            window.hide();
            return;
        }
        state.shown.get(shown_index).copied()
            .and_then(|item_ref| resolve(&state, item_ref))
            .map(|item| item.source.clone())
    };

    if let Some(source) = source {
        launch(&source);
        window.close();
    }
}

/// The 0-based row index for a digit `keyval`, if any.
///
/// "Alt + 1" through "Alt + 9" map to rows 0-8 and "Alt + 0" to row 9, for
/// both the top row of digits and the numeric keypad.
fn shortcut_index(keyval: gdk::Key) -> Option<usize> {
    use gdk::Key;

    let digit = match keyval {
        Key::_1 | Key::KP_1 => 1,
        Key::_2 | Key::KP_2 => 2,
        Key::_3 | Key::KP_3 => 3,
        Key::_4 | Key::KP_4 => 4,
        Key::_5 | Key::KP_5 => 5,
        Key::_6 | Key::KP_6 => 6,
        Key::_7 | Key::KP_7 => 7,
        Key::_8 | Key::KP_8 => 8,
        Key::_9 | Key::KP_9 => 9,
        Key::_0 | Key::KP_0 => 0,
        _ => return None,
    };

    // 1 -> 0, 2 -> 1, ..., 9 -> 8, 0 -> 9 (the tenth row).
    Some((digit + 9) % 10)
}

/// Open a result: files with the default opener, apps through GIO.
///
/// GIO types are deliberately created only here, at launch time: indexing runs
/// on worker threads where `gio::AppInfo` cannot travel.
fn launch(source: &Source) {
    match source {
        Source::File(path) => open_path(path),
        Source::App(entry) => match app_info_for(entry) {
            Some(info) => {
                if let Err(error) = info.launch(&[], gio::AppLaunchContext::NONE) {
                    eprintln!("trandex: could not launch application: {error}");
                }
            }
            None => eprintln!("trandex: could not launch application: bad Exec for {}", entry.name),
        },
        Source::Command(path) => {
            let _ = std::process::Command::new(path).spawn();
        }
    }
}

/// Turn a parsed `.desktop` entry into a GIO `AppInfo`, called at launch time.
fn app_info_for(entry: &DesktopEntry) -> Option<gio::AppInfo> {
    let exec = clean_exec(&entry.exec);
    if exec.is_empty() {
        return None;
    }
    gio::AppInfo::create_from_commandline(&exec, Some(&entry.name), gio::AppInfoCreateFlags::NONE)
        .ok()
}

/// Drop field codes and their trailing arguments from an `Exec` line.
///
/// `%f`, `%u`, ... are placeholders for launch-time arguments; with nothing to
/// pass, everything from the first token containing `%` is discarded.
fn clean_exec(exec: &str) -> String {
    exec.split_whitespace()
        .take_while(|token| !token.contains('%'))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Open `path` with `handlr`, falling back to `xdg-open`.
fn open_path(path: &Path) {
    let argument = path.to_string_lossy().to_string();

    if Command::new("handlr")
        .arg("open")
        .arg(&argument)
        .spawn()
        .is_err()
    {
        let _ = Command::new("xdg-open").arg(&argument).spawn();
    }
}

/// Whether per-keystroke timing should be written to stderr.
fn debug_enabled() -> bool {
    std::env::var_os("TRANDEX_DEBUG").is_some()
}

/// Log a startup milestone in `[ms since main] label` form so the gaps in the
/// timeline are obvious. Only emitted when `TRANDEX_DEBUG` is set.
pub fn log_ms(origin: Instant, label: &str) {
    if debug_enabled() {
        let milliseconds = origin.elapsed().as_secs_f64() * 1000.0;
        eprintln!("[ms since main: {:>8.1}] {label}", milliseconds);
    }
}

/// Write a diagnostic line when `TRANDEX_DEBUG` is set.
fn debug_log(message: &str) {
    if debug_enabled() {
        eprintln!("trandex[debug]: {message}");
    }
}

/// Log the cost of a filter pass (only when `TRANDEX_DEBUG` is set).
fn log_refresh(state: &Rc<RefCell<State>>, started: Instant) {
    if debug_enabled() {
        let state = state.borrow();
        eprintln!(
            "trandex[debug]: refresh query={:?} shown={} elapsed={:?}",
            state.query,
            state.shown.len(),
            started.elapsed()
        );
    }
}
