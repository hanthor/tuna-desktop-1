//! GNOME 51's `AppFavorites` (js/ui/appFavorites.js): the dash's pinned
//! apps, kept in `org.gnome.shell favorite-apps`.
//!
//! The key is the one source of truth. It is read when the shell starts
//! and again whenever it changes, whoever changes it (a drop on the dash,
//! the app menu's Pin to Dash, `gsettings set`, another session): the
//! listeners hear of a change only when the apps it names, in order, are
//! different (`_updateFavorites`' `haveChanged`). Every write sends back
//! only the apps that are installed, as `reload` leaves only those in
//! `_favorites`. Pinning and unpinning tell the user with a notification
//! that can undo them (`_showNotification`).
//!
//! Without the schema (a host with no GNOME shell settings) the dash
//! shows GNOME's default favorites and nothing can be pinned, as
//! `global.settings.is_writable('favorite-apps')` is false there.

use std::cell::RefCell;
use std::rc::Rc;

use gio::prelude::*;

use crate::live_apps::LiveApps;
use crate::providers;

const SCHEMA: &str = "org.gnome.shell";
/// `FAVORITE_APPS_KEY`.
pub const KEY: &str = "favorite-apps";

/// What pinning or unpinning tells the user: the summary
/// (`%s Pinned to Dash`) and what its Undo button does.
pub type Notifier = Rc<dyn Fn(String, Rc<dyn Fn()>)>;

pub struct AppFavorites {
    settings: Option<gio::Settings>,
    apps: Rc<LiveApps>,
    /// The favorites as last read: installed apps only, as desktop ids.
    ids: RefCell<Vec<String>>,
    listeners: RefCell<Vec<Rc<dyn Fn()>>>,
    notifier: RefCell<Option<Notifier>>,
}

/// The installed apps among `wanted`, once each and in order, as the
/// desktop ids `favorite-apps` holds (`reload`). `installed` gives an
/// app's desktop id when it is installed.
pub fn installed_ids(wanted: &[String], installed: impl Fn(&str) -> Option<String>) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for id in wanted {
        if let Some(id) = installed(id) {
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}

/// `ids` with `id` inserted at `pos` (`_addFavorite`): at the end for
/// `None`, and `splice` clamps a position past it.
pub fn inserted(ids: &[String], id: &str, pos: Option<usize>) -> Vec<String> {
    let mut ids = ids.to_vec();
    let pos = pos.map_or(ids.len(), |pos| pos.min(ids.len()));
    ids.insert(pos, id.to_owned());
    ids
}

/// The desktop id GNOME keys an installed app by.
pub fn desktop_id(app_id: &str) -> String {
    format!("{}.desktop", app_id.trim_end_matches(".desktop"))
}

impl AppFavorites {
    pub fn new(apps: Rc<LiveApps>) -> Rc<Self> {
        let source = gio::SettingsSchemaSource::default();
        let settings = source
            .and_then(|source| source.lookup(SCHEMA, true))
            .filter(|schema| schema.has_key(KEY))
            .map(|_| gio::Settings::new(SCHEMA));
        let this = Rc::new(Self {
            settings,
            apps,
            ids: RefCell::default(),
            listeners: RefCell::default(),
            notifier: RefCell::default(),
        });
        *this.ids.borrow_mut() = this.read();
        if let Some(settings) = &this.settings {
            let weak = Rc::downgrade(&this);
            settings.connect_changed(Some(KEY), move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.update();
                }
            });
        }
        // An app installed or removed changes which favorites exist.
        let weak = Rc::downgrade(&this);
        this.apps.connect_changed(move || {
            if let Some(this) = weak.upgrade() {
                this.update();
            }
        });
        this
    }

    /// What the key says now, narrowed to the installed apps.
    fn read(&self) -> Vec<String> {
        let wanted: Vec<String> = match &self.settings {
            Some(settings) => settings.strv(KEY).iter().map(|id| id.to_string()).collect(),
            None => crate::logic::DEFAULT_FAVORITES
                .iter()
                .map(|id| (*id).to_owned())
                .collect(),
        };
        let apps = self.apps.get();
        installed_ids(&wanted, |id| {
            providers::provider_app(&apps, id).map(|entry| desktop_id(&entry.app_id))
        })
    }

    /// `_updateFavorites`: reload, and tell the listeners only when the
    /// favorites are not the ones they last heard.
    fn update(&self) {
        let now = self.read();
        if *self.ids.borrow() == now {
            return;
        }
        *self.ids.borrow_mut() = now;
        let listeners = self.listeners.borrow().clone();
        for listener in listeners {
            listener();
        }
    }

    /// The favorites in dash order, as desktop ids.
    pub fn ids(&self) -> Vec<String> {
        self.ids.borrow().clone()
    }

    pub fn is_favorite(&self, desktop_id: &str) -> bool {
        self.ids.borrow().iter().any(|id| id == desktop_id)
    }

    /// `global.settings.is_writable('favorite-apps')`.
    pub fn writable(&self) -> bool {
        self.settings
            .as_ref()
            .is_some_and(|settings| settings.is_writable(KEY))
    }

    /// Call `f` whenever the favorites change.
    pub fn connect_changed(&self, f: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Rc::new(f));
    }

    /// How pinning and unpinning tell the user.
    pub fn set_notifier(&self, notifier: Notifier) {
        *self.notifier.borrow_mut() = Some(notifier);
    }

    fn write(&self, ids: &[String]) {
        if let Some(settings) = &self.settings {
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            if let Err(e) = settings.set_strv(KEY, refs.as_slice()) {
                eprintln!("tuna-shell-gtk: {KEY}: {e}");
            }
        }
    }

    fn notify(&self, summary: String, undo: Rc<dyn Fn()>) {
        let notifier = self.notifier.borrow().clone();
        if let Some(notifier) = notifier {
            notifier(summary, undo);
        }
    }

    fn name(&self, desktop_id: &str) -> Option<String> {
        let apps = self.apps.get();
        providers::provider_app(&apps, desktop_id).map(|entry| entry.name.clone())
    }

    /// `_addFavorite`: whether `desktop_id` was pinned (an installed app
    /// that is not a favorite yet).
    fn add(&self, desktop_id: &str, pos: Option<usize>) -> bool {
        if !self.writable() || self.is_favorite(desktop_id) || self.name(desktop_id).is_none() {
            return false;
        }
        self.write(&inserted(&self.ids(), desktop_id, pos));
        true
    }

    /// `addFavoriteAtPos`: pin at `pos` and offer to undo.
    pub fn add_at(self: &Rc<Self>, desktop_id: &str, pos: Option<usize>) {
        if !self.add(desktop_id, pos) {
            return;
        }
        let Some(name) = self.name(desktop_id) else {
            return;
        };
        let (this, id) = (Rc::downgrade(self), desktop_id.to_owned());
        self.notify(
            format!("{name} Pinned to Dash"),
            Rc::new(move || {
                if let Some(this) = this.upgrade() {
                    this.unpin(&id);
                }
            }),
        );
    }

    /// `moveFavoriteToPos`: out and back in as one write, without a
    /// notice.
    pub fn move_to(&self, desktop_id: &str, pos: usize) {
        if !self.writable() || !self.is_favorite(desktop_id) {
            return;
        }
        let rest: Vec<String> = self
            .ids()
            .into_iter()
            .filter(|id| id != desktop_id)
            .collect();
        self.write(&inserted(&rest, desktop_id, Some(pos)));
    }

    /// `_removeFavorite`: whether it was a favorite.
    fn unpin(&self, desktop_id: &str) -> bool {
        if !self.is_favorite(desktop_id) || !self.writable() {
            return false;
        }
        let ids: Vec<String> = self
            .ids()
            .into_iter()
            .filter(|id| id != desktop_id)
            .collect();
        self.write(&ids);
        true
    }

    /// `removeFavorite`: unpin and offer to pin it back where it was.
    pub fn remove(self: &Rc<Self>, desktop_id: &str) {
        let pos = self.ids().iter().position(|id| id == desktop_id);
        let name = self.name(desktop_id);
        if !self.unpin(desktop_id) {
            return;
        }
        let Some(name) = name else { return };
        let (this, id) = (Rc::downgrade(self), desktop_id.to_owned());
        self.notify(
            format!("{name} Unpinned From Dash"),
            Rc::new(move || {
                if let Some(this) = this.upgrade() {
                    this.add(&id, pos);
                }
            }),
        );
    }
}

/// Where the dash puts a dragged app (`handleDragOver`'s slot): the
/// item under `x` of `items` items spread over `box_width`, no further
/// than after the last of the `favorites` (an app dropped among the
/// running ones pins at the end of the favorites).
pub fn placeholder_slot(
    x: f64,
    box_width: f64,
    items: usize,
    favorites: usize,
    empty: bool,
) -> usize {
    if empty || items == 0 || box_width <= 0.0 {
        return 0;
    }
    let slot = (x * items as f64 / box_width).floor().max(0.0) as usize;
    slot.min(favorites)
}

/// Whether the placeholder at `slot` would sit beside the dragged
/// favorite itself, before or after it (`favPos`): the dash then shows
/// none, since the drop would change nothing.
pub fn beside_itself(slot: usize, dragged_at: Option<usize>) -> bool {
    dragged_at.is_some_and(|at| slot == at || slot == at + 1)
}

/// The favorite position a drop at placeholder `slot` pins at
/// (`acceptDrop`): the favorites among the items before it, not
/// counting the dragged app itself. `items` are the dash items'
/// desktop ids, none for a window-backed item.
pub fn drop_position(
    items: &[Option<String>],
    slot: usize,
    favorites: &[String],
    dragged: &str,
) -> usize {
    items
        .iter()
        .take(slot)
        .flatten()
        .filter(|id| id.as_str() != dragged && favorites.contains(id))
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn only_installed_apps_stay_and_once() {
        let installed = |id: &str| {
            let id = id.trim_end_matches(".desktop");
            ["a", "b"].contains(&id).then(|| desktop_id(id))
        };
        assert_eq!(
            installed_ids(&ids(&["a.desktop", "gone.desktop", "b", "a"]), installed),
            ids(&["a.desktop", "b.desktop"]),
        );
    }

    #[test]
    fn insertion_clamps_like_splice() {
        let list = ids(&["a.desktop", "b.desktop"]);
        assert_eq!(
            inserted(&list, "c.desktop", None),
            ids(&["a.desktop", "b.desktop", "c.desktop"])
        );
        assert_eq!(
            inserted(&list, "c.desktop", Some(0)),
            ids(&["c.desktop", "a.desktop", "b.desktop"])
        );
        assert_eq!(
            inserted(&list, "c.desktop", Some(1)),
            ids(&["a.desktop", "c.desktop", "b.desktop"])
        );
        assert_eq!(
            inserted(&list, "c.desktop", Some(9)),
            ids(&["a.desktop", "b.desktop", "c.desktop"])
        );
    }

    #[test]
    fn the_slot_follows_the_pointer_and_stops_after_the_favorites() {
        // Four items over 400px, two favorites.
        assert_eq!(placeholder_slot(10.0, 400.0, 4, 2, false), 0);
        assert_eq!(placeholder_slot(150.0, 400.0, 4, 2, false), 1);
        assert_eq!(placeholder_slot(250.0, 400.0, 4, 2, false), 2);
        assert_eq!(placeholder_slot(390.0, 400.0, 4, 2, false), 2);
        // An empty dash always takes the first slot.
        assert_eq!(placeholder_slot(300.0, 100.0, 0, 0, true), 0);
        assert_eq!(placeholder_slot(-5.0, 400.0, 4, 2, false), 0);
    }

    #[test]
    fn no_slot_beside_the_dragged_favorite() {
        assert!(beside_itself(1, Some(1)));
        assert!(beside_itself(2, Some(1)));
        assert!(!beside_itself(0, Some(1)));
        assert!(!beside_itself(3, Some(1)));
        assert!(!beside_itself(1, None));
    }

    #[test]
    fn a_drop_pins_after_the_favorites_before_it() {
        let favorites = ids(&["a.desktop", "b.desktop", "c.desktop"]);
        let items: Vec<Option<String>> = ["a.desktop", "b.desktop", "c.desktop", "run.desktop"]
            .iter()
            .map(|s| Some((*s).to_owned()))
            .collect();
        // A new app dropped at slot 2 goes in third.
        assert_eq!(drop_position(&items, 2, &favorites, "new.desktop"), 2);
        // The dragged favorite is not counted: a.desktop to slot 3 lands
        // at 2 (b and c stay ahead of it once it leaves).
        assert_eq!(drop_position(&items, 3, &favorites, "a.desktop"), 2);
        assert_eq!(drop_position(&items, 0, &favorites, "c.desktop"), 0);
        // A window-backed item before the slot is no favorite.
        let mixed = vec![Some("a.desktop".to_owned()), None];
        assert_eq!(drop_position(&mixed, 2, &favorites[..1], "new.desktop"), 1);
    }
}
