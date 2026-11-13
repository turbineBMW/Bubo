//! A see-through conversation list.
//!
//! `--bubo-sidebar-opacity`, a number from 0 to 1, fades the sidebar's
//! background but never its text. It defaults to 1 here, and an Omarchy
//! theme's `bubo.css` (see `omarchy::palette`) can lower it with a one-line
//! `:root { --bubo-sidebar-opacity: 0.8; }`.
//!
//! A faded sidebar over the window's own background would only blend into
//! that colour, so below 1 the main window gets the `bubo-transparent` class,
//! which clears the window background so the desktop shows through. Everything
//! else that was painted by that background is painted again here, so only the
//! sidebar turns translucent. GTK has no API to read a custom property back,
//! so the opacity that decides the class is worked out from the CSS text by
//! [`opacity_in`] whenever the theme reloads.
use adw::prelude::*;
use gtk4::{self as gtk, gio};
use std::cell::Cell;

/// The custom property a theme sets.
pub const OPACITY: &str = "--bubo-sidebar-opacity";

/// Below the Omarchy provider (`APPLICATION + 2`), so a theme's value wins
/// over the default of 1.
///
/// Side by side, the split view wraps its pages in a `.sidebar-pane` and a
/// `.content-pane`; collapsed to one column, it moves both pages into a
/// navigation view instead, where neither is a sidebar and both stay opaque.
/// `.bubo-root` is the window's top-level stack (see `ui::build`).
const CSS: &str = "
:root { --bubo-sidebar-opacity: 1; }
/* Only the background fades, never the text. Scoped to the split view's own
   pane: libadwaita paints nested sidebar panes differently. */
navigation-split-view > .sidebar-pane {
    background-color: alpha(var(--sidebar-bg-color), var(--bubo-sidebar-opacity));
}
navigation-split-view > .sidebar-pane:backdrop {
    background-color: alpha(var(--sidebar-backdrop-color), var(--bubo-sidebar-opacity));
}
window.bubo-transparent { background-color: transparent; }
/* What was drawn on the window background just cleared: the open
   conversation (its headerbar, thread, composer and empty state), both pages
   when the split view is collapsed, and the sign-in, pairing, connecting and
   error pages the chats replace. Dialogs, popovers, toasts and the banner
   paint their own backgrounds. */
window.bubo-transparent .bubo-root > :not(navigation-split-view),
window.bubo-transparent navigation-split-view > .content-pane,
window.bubo-transparent navigation-split-view > navigation-view > navigation-view-page {
    background-color: var(--window-bg-color);
    color: var(--window-fg-color);
}
";

const TRANSPARENT: &str = "bubo-transparent";

thread_local! {
    /// The opacity the cascade currently resolves to.
    static CURRENT: Cell<f32> = const { Cell::new(1.0) };
}

/// Install once during application startup, before the Omarchy theme.
pub fn install(display: &gtk::gdk::Display) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    gtk::style_context_add_provider_for_display(
        display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

/// Record the opacity now in force and bring the main window in line.
pub fn set_opacity(opacity: f32) {
    CURRENT.with(|current| current.set(opacity));
    let Some(app) = gio::Application::default().and_downcast::<gtk::Application>() else {
        return;
    };
    // Only the main window: the picture viewer is an application window too,
    // and keeps its own black background.
    for window in app.windows() {
        if let Some(window) = window.downcast_ref::<adw::ApplicationWindow>() {
            sync(window);
        }
    }
}

/// Clear `window`'s background while the sidebar is translucent, and restore
/// it once it isn't.
pub fn sync(window: &adw::ApplicationWindow) {
    if CURRENT.with(Cell::get) < 1.0 {
        window.add_css_class(TRANSPARENT);
    } else {
        window.remove_css_class(TRANSPARENT);
    }
}

/// The last value [`OPACITY`] is given anywhere in `css`, clamped to 0..=1.
/// Comments are skipped; selectors are not checked, since the built-in rule
/// reads the property wherever the cascade puts it. A custom property takes
/// any value, so the last declaration wins even when it isn't a plain number,
/// and then nothing is known about the opacity. That includes a percentage:
/// GTK's `alpha()` only takes a number, and drops the sidebar's background
/// altogether when given one.
pub fn opacity_in(css: &str) -> Option<f32> {
    let mut found = None;
    let mut at = 0;
    while let Some(offset) = css[at..].find(['/', '-']) {
        let start = at + offset;
        let tail = &css[start..];
        if let Some(comment) = tail.strip_prefix("/*") {
            let Some(end) = comment.find("*/") else { break };
            at = start + 2 + end + 2;
            continue;
        }
        // `--x--bubo-sidebar-opacity` is a different property.
        let inside_a_name = css[..start]
            .chars()
            .next_back()
            .is_some_and(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'));
        let value = tail
            .strip_prefix(OPACITY)
            .filter(|_| !inside_a_name)
            // So is `--bubo-sidebar-opacity-x`, and `var(--bubo-sidebar-opacity)`
            // reads it rather than setting it.
            .and_then(|rest| rest.trim_start().strip_prefix(':'));
        let Some(value) = value else {
            at = start + 1;
            continue;
        };
        let end = value.find([';', '}', '\n']).unwrap_or(value.len());
        let text = value[..end].trim();
        found = text
            .parse::<f32>()
            .ok()
            .filter(|o| o.is_finite())
            .map(|o| o.clamp(0.0, 1.0));
        at = css.len() - value.len() + end;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_value_in_any_rule_wins() {
        let css = "/* --bubo-sidebar-opacity: 0.1; */\n\
                   :root {\n  --bubo-sidebar-opacity: 0.8;\n}\n\
                   window { --bubo-sidebar-opacity:0.6 }\n";
        assert_eq!(opacity_in(css), Some(0.6));
        assert_eq!(
            opacity_in(":root { --bubo-sidebar-opacity: 1; }"),
            Some(1.0)
        );
    }

    #[test]
    fn values_are_clamped() {
        assert_eq!(
            opacity_in(":root { --bubo-sidebar-opacity: 2; }"),
            Some(1.0)
        );
        assert_eq!(
            opacity_in(":root { --bubo-sidebar-opacity: -0.5; }"),
            Some(0.0)
        );
    }

    /// Anything GTK would not read as this property's value leaves it unset.
    #[test]
    fn garbage_and_look_alikes_are_ignored() {
        for css in [
            "",
            ":root { --bubo-sidebar-opacity: lots; }",
            ":root { --bubo-sidebar-opacity: NaN; }",
            // GTK's alpha() rejects a percentage.
            ":root { --bubo-sidebar-opacity: 75%; }",
            ":root { --bubo-sidebar-opacity-x: 0.5; }",
            ":root { --x--bubo-sidebar-opacity: 0.5; }",
            ".sidebar-pane { opacity: var(--bubo-sidebar-opacity); }",
            "/* --bubo-sidebar-opacity: 0.5",
        ] {
            assert_eq!(opacity_in(css), None, "{css}");
        }
        // A later declaration replaces an earlier one, usable or not.
        assert_eq!(
            opacity_in(
                ":root { --bubo-sidebar-opacity: 0.5; }\n\
                 :root { --bubo-sidebar-opacity: lots; }"
            ),
            None
        );
    }

    /// The default the built-in stylesheet states is the one assumed when a
    /// theme states none.
    #[test]
    fn the_built_in_default_is_opaque() {
        assert_eq!(opacity_in(CSS), Some(1.0));
    }

    /// The stylesheet parses, and the split view names its panes the way it
    /// assumes: `.sidebar-pane` only while both are shown.
    #[test]
    #[ignore = "requires a GTK display; run explicitly with --ignored"]
    fn the_stylesheet_matches_the_split_view() {
        adw::init().unwrap();
        let provider = gtk::CssProvider::new();
        let errors = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let errors_clone = errors.clone();
        provider.connect_parsing_error(move |_, _, error| {
            errors_clone.borrow_mut().push(error.to_string());
        });
        provider.load_from_string(CSS);
        assert!(
            errors.borrow().is_empty(),
            "CSS errors: {:?}",
            errors.borrow()
        );

        let sidebar = adw::NavigationPage::builder().title("Chats").build();
        let content = adw::NavigationPage::builder().title("Conversation").build();
        let split = adw::NavigationSplitView::builder()
            .sidebar(&sidebar)
            .content(&content)
            .build();
        assert_eq!(split.css_name(), "navigation-split-view");
        // Side by side, each page sits in a pane the split view classes.
        let pane = |page: &adw::NavigationPage, class: &str| {
            let pane = page.parent().unwrap();
            assert!(pane.has_css_class(class), "{:?}", pane.css_classes());
            assert!(pane.parent().is_some_and(|parent| parent == split));
        };
        pane(&sidebar, "sidebar-pane");
        pane(&content, "content-pane");
        assert_eq!(sidebar.css_name(), "navigation-view-page");
        // Collapsed, both pages move into a navigation view, out of any pane.
        split.set_collapsed(true);
        for page in [&sidebar, &content] {
            let view = page.parent().unwrap();
            assert_eq!(view.css_name(), "navigation-view");
            assert!(view.parent().is_some_and(|parent| parent == split));
        }
        split.set_collapsed(false);
        pane(&sidebar, "sidebar-pane");
    }
}
