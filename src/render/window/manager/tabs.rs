//! Tab lifecycle: `split_active` (the borrow-checker escape hatch), plus
//! `active`/`active_mut`/`open_tab`/`close_tab`/`switch_to_tab`.

use std::collections::HashMap;

use ego_tree::Tree;
use rustc_hash::FxHashMap;

use crate::network::{TabId, UiEvent};
use crate::parser::MizuNode;
use crate::render::chrome_vello::CHROME_HEIGHT;
use crate::render::responsive::{RenderEnvironment, ViewportSize};

use super::types::{MAX_OPEN_TABS, MizuWindowManager, TabDocument, TabState, WindowCtx};
use super::viewport::resize_tab_viewport;

impl MizuWindowManager {
    /// Splits `self` into the active tab plus the window-level state it may
    /// need, as two independent borrows.
    ///
    /// # Panics
    ///
    /// Panics if the `active_tab < tabs.len()` invariant has been broken. That
    /// is deliberately loud rather than silently clamping: a
    /// wrong-but-in-range index would silently deliver one tab's input to
    /// another, which is exactly the class of bug invariant T1 exists to
    /// prevent.
    pub(crate) fn split_active(&mut self) -> (&mut TabState, WindowCtx<'_>) {
        let Self {
            tabs,
            active_tab,
            font_cx,
            layout_cx,
            image_cache,
            fetching_images,
            network_tx,
            logic_tx,
            preferences,
            window,
            window_logical_size,
            modifiers,
            start_time,
            history_log,
            storage_usage,
            ..
        } = self;
        // Direct index, matching `active()`/`active_mut()`: the
        // `active_tab < tabs.len()` invariant is maintained by `close_tab`,
        // and violating it must fail loudly rather than silently redirecting
        // one tab's input to another (see this method's doc comment).
        let tab = &mut tabs[*active_tab];
        let active_tab_id = tab.id;
        (
            tab,
            WindowCtx {
                active_tab_id,
                font_cx,
                layout_cx,
                image_cache,
                fetching_images,
                network_tx,
                logic_tx,
                preferences,
                window: window.as_ref(),
                window_logical_size: *window_logical_size,
                modifiers: *modifiers,
                start_time: *start_time,
                history_log,
                storage_usage,
            },
        )
    }

    /// Like [`Self::split_active`] but selects a tab by id.
    ///
    /// Returns `None` when `id` names no live tab — a message arriving for a
    /// tab the user has since closed. Callers must treat that as a silent
    /// drop and never fall back to the active tab: a `WorkerResponse` carries
    /// bare [`Symbol`]s that are only meaningful against *its own* tab's
    /// frozen interner, so resolving one against a different tab would write
    /// a value under whatever unrelated name that id happens to mean there.
    pub(crate) fn split_tab(&mut self, id: TabId) -> Option<(&mut TabState, WindowCtx<'_>)> {
        let Self {
            tabs,
            active_tab,
            font_cx,
            layout_cx,
            image_cache,
            fetching_images,
            network_tx,
            logic_tx,
            preferences,
            window,
            window_logical_size,
            modifiers,
            start_time,
            history_log,
            storage_usage,
            ..
        } = self;
        let active_tab_id = tabs[*active_tab].id;
        let tab = tabs.iter_mut().find(|t| t.id == id)?;
        Some((
            tab,
            WindowCtx {
                active_tab_id,
                font_cx,
                layout_cx,
                image_cache,
                fetching_images,
                network_tx,
                logic_tx,
                preferences,
                window: window.as_ref(),
                window_logical_size: *window_logical_size,
                modifiers: *modifiers,
                start_time: *start_time,
                history_log,
                storage_usage,
            },
        ))
    }

    /// Index of the active tab within [`Self::tabs`]. Always in range.
    pub fn active_tab_index(&self) -> usize {
        self.active_tab
    }

    /// The currently displayed tab.
    pub fn active(&self) -> &TabState {
        &self.tabs[self.active_tab]
    }

    /// Opens a blank tab on `url` and returns its id, or `None` when
    /// [`MAX_OPEN_TABS`] is already reached.
    ///
    /// Does **not** switch to it — the caller decides, because Ctrl+T switches
    /// while a background open (should one ever be added) must not.
    pub fn open_tab(&mut self, url: &str) -> Option<TabId> {
        if self.tabs.len() >= MAX_OPEN_TABS {
            tracing::warn!(
                open = self.tabs.len(),
                "refusing to open tab: limit reached"
            );
            return None;
        }
        let id = TabId(self.next_tab_id);
        self.next_tab_id += 1;
        let doc = TabDocument {
            // An empty `doc` root: a blank page until the first navigation
            // replaces the whole document via `reload_tab_document`.
            dom: Tree::new(MizuNode {
                primitive: crate::parser::Primitive::Doc,
                attributes: FxHashMap::default(),
                events: FxHashMap::default(),
                iterator_context: None,
                conditional_classes: Vec::new(),
            }),
            style_rules: HashMap::new(),
            style_variants: Vec::new(),
            logic_fns: FxHashMap::default(),
        };
        let env = RenderEnvironment {
            viewport: ViewportSize {
                width: self.window_logical_size.0,
                height: (self.window_logical_size.1 - CHROME_HEIGHT).max(0.0),
            },
            color_scheme: self.preferences.color_scheme,
        };
        let tab = match TabState::new(
            id,
            doc,
            env,
            url,
            &mut self.image_cache,
            &self.storage_usage,
        ) {
            Ok(t) => t,
            Err(e) => {
                tracing::error!(error = ?e, "failed to build new tab");
                return None;
            }
        };
        self.tabs.push(tab);
        Some(id)
    }

    /// Closes `id`, returning `false` when it was the last tab — the caller
    /// decides what that means (the event loop exits).
    ///
    /// Order matters: the worker is told first so it drops the document's
    /// store and interner, then the tab is unregistered from every in-flight
    /// image's waiter list, and only then removed. A late worker or network
    /// response tagged with `id` afterwards finds no tab and is dropped, which
    /// is exactly the intended behaviour — ids are never reused, so it can
    /// never be misrouted to a different document.
    pub fn close_tab(&mut self, id: TabId) -> bool {
        let Some(pos) = self.tabs.iter().position(|t| t.id == id) else {
            return true;
        };
        if self.tabs.len() == 1 {
            return false;
        }
        let _ = self.logic_tx.send((id, UiEvent::CloseTab));
        for waiters in self.fetching_images.values_mut() {
            waiters.retain(|w| *w != id);
        }
        self.fetching_images
            .retain(|_, waiters| !waiters.is_empty());
        self.tabs.remove(pos);
        // Browser convention: focus moves to the tab on the right, falling
        // back to the left when the closed tab was last.
        if self.active_tab > pos || self.active_tab >= self.tabs.len() {
            self.active_tab = self.active_tab.saturating_sub(1);
        }
        self.active_tab = self.active_tab.min(self.tabs.len() - 1);
        true
    }

    /// Makes `id` the visible tab. No-op for an unknown id.
    ///
    /// Returns `true` when the caller should rebuild window-level views of the
    /// tab (title, accessibility tree, redraw) — i.e. when the active tab
    /// actually changed.
    pub fn switch_to_tab(&mut self, id: TabId) -> bool {
        let Some(pos) = self.tabs.iter().position(|t| t.id == id) else {
            return false;
        };
        if pos == self.active_tab {
            return false;
        }
        self.active_tab = pos;
        if self.tabs[pos].layout_stale {
            let (width, height) = self.window_logical_size;
            let (tab, mut ctx) = self.split_active();
            if let Err(e) = resize_tab_viewport(tab, &mut ctx, width, height, None) {
                tracing::error!(error = ?e, "relayout on tab switch failed");
            }
        }
        true
    }

    /// The currently displayed tab, mutably.
    pub fn active_mut(&mut self) -> &mut TabState {
        &mut self.tabs[self.active_tab]
    }
}
