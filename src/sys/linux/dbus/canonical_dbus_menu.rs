//! Canonical D-Bus Menu implementation
//!
//! https://github.com/gnustep/libs-dbuskit/blob/master/Bundles/DBusMenu/com.canonical.dbusmenu.xml

use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedValue;
use zbus::zvariant::Type;
use zbus::zvariant::Value;
use zbus::Connection;

#[derive(Debug, Default, Type, Serialize, Deserialize, Value, OwnedValue)]
pub struct Layout {
    pub id: i32,
    pub properties: HashMap<String, OwnedValue>,
    pub children: Vec<OwnedValue>,
}

pub struct DbusMenu<T>
where
    T: crate::TrayIconEvent,
{
    // Shared and lockable so the `event` handler can mutate the checkable
    // (`is_checked`) state authoritatively, instead of relying on the tray
    // host's optimistic client-side toggle. This keeps the server the source
    // of truth for checkable / radio items.
    menu_sys: Arc<RwLock<super::super::MenuSys<T>>>,
}

impl<T> DbusMenu<T>
where
    T: crate::TrayIconEvent,
{
    pub fn new(menu_sys: super::super::MenuSys<T>) -> Self {
        DbusMenu {
            menu_sys: Arc::new(RwLock::new(menu_sys)),
        }
    }

    fn build_layout_from_items(&self, items: &[super::super::MenuItemData<T>]) -> Vec<OwnedValue> {
        let mut children = vec![];

        for item in items {
            if item.is_separator {
                let mut properties = HashMap::new();
                properties.insert(
                    "type".to_string(),
                    OwnedValue::try_from(Value::new("separator")).unwrap(),
                );

                let layout = Layout {
                    id: item.id,
                    properties,
                    children: vec![],
                };
                children.push(OwnedValue::try_from(layout).unwrap());
            } else {
                let mut properties = HashMap::new();
                properties.insert(
                    "label".to_string(),
                    OwnedValue::try_from(Value::new(item.label.as_str())).unwrap(),
                );

                // Always set the enabled property explicitly
                properties.insert(
                    "enabled".to_string(),
                    OwnedValue::try_from(Value::new(!item.is_disabled)).unwrap(),
                );

                if item.is_checkable {
                    // Radio items are mutually exclusive within a group; the
                    // host renders them as radio buttons. Checkbox items are
                    // independent toggles.
                    let toggle_type = if item.is_radio { "radio" } else { "checkbox" };
                    properties.insert(
                        "toggle-type".to_string(),
                        OwnedValue::try_from(Value::new(toggle_type)).unwrap(),
                    );
                    properties.insert(
                        "toggle-state".to_string(),
                        OwnedValue::try_from(Value::new(if item.is_checked { 1i32 } else { 0i32 }))
                            .unwrap(),
                    );
                }

                let child_layouts = if !item.children.is_empty() {
                    properties.insert(
                        "children-display".to_string(),
                        OwnedValue::try_from(Value::new("submenu")).unwrap(),
                    );
                    self.build_layout_from_items(&item.children)
                } else {
                    vec![]
                };

                let layout = Layout {
                    id: item.id,
                    properties,
                    children: child_layouts,
                };
                children.push(OwnedValue::try_from(layout).unwrap());
            }
        }

        children
    }

    fn find_item_by_id<'a>(
        &self,
        id: i32,
        items: &'a [super::super::MenuItemData<T>],
    ) -> Option<&'a super::super::MenuItemData<T>> {
        for item in items {
            if item.id == id {
                return Some(item);
            }
            if !item.children.is_empty() {
                if let Some(found) = self.find_item_by_id(id, &item.children) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Mutable counterpart of [`find_item_by_id`].
    fn find_item_by_id_mut<'a>(
        id: i32,
        items: &'a mut [super::super::MenuItemData<T>],
    ) -> Option<&'a mut super::super::MenuItemData<T>> {
        for item in items {
            if item.id == id {
                return Some(item);
            }
            if !item.children.is_empty() {
                if let Some(found) = Self::find_item_by_id_mut(id, &mut item.children) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Enforce radio-group exclusivity for the item `target_id` within one
    /// level of `siblings`. A group is the maximal run of consecutive radio
    /// items containing `target_id`. Returns `Some((event_id, changed))` when
    /// `target_id` is a radio item in this level, where `changed` lists every
    /// item whose `is_checked` actually flipped (id, new state).
    fn select_radio_in_siblings(
        target_id: i32,
        siblings: &mut [super::super::MenuItemData<T>],
    ) -> Option<(Option<T>, Vec<(i32, bool)>)> {
        let pos = siblings.iter().position(|i| i.id == target_id)?;
        if !siblings[pos].is_radio {
            return None;
        }

        let n = siblings.len();
        let mut start = pos;
        while start > 0 && siblings[start - 1].is_radio {
            start -= 1;
        }
        let mut end = pos;
        while end + 1 < n && siblings[end + 1].is_radio {
            end += 1;
        }

        let mut changed = Vec::new();
        for i in start..=end {
            let want = siblings[i].id == target_id;
            if siblings[i].is_checked != want {
                siblings[i].is_checked = want;
                changed.push((siblings[i].id, want));
            }
        }

        let event_id = siblings[pos].event_id.clone();
        Some((event_id, changed))
    }

    /// Recursively locate the radio group containing `target_id` anywhere in
    /// the menu tree and enforce exclusivity within it.
    fn apply_radio_selection(
        target_id: i32,
        items: &mut [super::super::MenuItemData<T>],
    ) -> Option<(Option<T>, Vec<(i32, bool)>)> {
        if let Some(result) = Self::select_radio_in_siblings(target_id, items) {
            return Some(result);
        }
        for item in items.iter_mut() {
            if !item.children.is_empty() {
                if let Some(result) = Self::apply_radio_selection(target_id, &mut item.children) {
                    return Some(result);
                }
            }
        }
        None
    }

    fn toggle_state_value(checked: bool) -> OwnedValue {
        OwnedValue::try_from(Value::new(if checked { 1i32 } else { 0i32 })).unwrap()
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl<T> DbusMenu<T>
where
    T: crate::TrayIconEvent,
{
    // methods
    async fn get_layout(
        &self,
        parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<(u32, Layout)> {
        let menu_sys = self
            .menu_sys
            .read()
            .map_err(|_| zbus::fdo::Error::Failed("menu lock poisoned".to_string()))?;

        if parent_id == 0 {
            // Root menu
            let children = self.build_layout_from_items(&menu_sys.items);

            Ok((
                super::current_layout_revision(),
                Layout {
                    id: parent_id,
                    properties: HashMap::new(),
                    children,
                },
            ))
        } else {
            // Submenu
            if let Some(item) = self.find_item_by_id(parent_id, &menu_sys.items) {
                let children = self.build_layout_from_items(&item.children);

                Ok((
                    super::current_layout_revision(),
                    Layout {
                        id: parent_id,
                        properties: HashMap::new(),
                        children,
                    },
                ))
            } else {
                Err(zbus::fdo::Error::InvalidArgs(
                    "parentId not found".to_string(),
                ))
            }
        }
    }

    async fn get_group_properties(
        &self,
        _ids: Vec<i32>,
        _property_names: Vec<String>,
    ) -> zbus::fdo::Result<Vec<(i32, HashMap<String, OwnedValue>)>> {
        Ok(Vec::new())
    }

    async fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        let menu_sys = self
            .menu_sys
            .read()
            .map_err(|_| zbus::fdo::Error::Failed("menu lock poisoned".to_string()))?;

        if let Some(item) = self.find_item_by_id(id, &menu_sys.items) {
            match name.as_str() {
                "label" => Ok(OwnedValue::try_from(Value::new(item.label.as_str())).unwrap()),
                "enabled" => Ok(OwnedValue::try_from(Value::new(!item.is_disabled)).unwrap()),
                "toggle-type" if item.is_checkable => Ok(OwnedValue::try_from(Value::new(
                    if item.is_radio { "radio" } else { "checkbox" },
                ))
                .unwrap()),
                "toggle-state" if item.is_checkable => {
                    Ok(Self::toggle_state_value(item.is_checked))
                }
                _ => Err(zbus::fdo::Error::InvalidArgs(format!(
                    "Property '{}' for id {} not found",
                    name, id
                ))),
            }
        } else {
            Err(zbus::fdo::Error::InvalidArgs(format!(
                "Property '{}' for id {} not found",
                name, id
            )))
        }
    }

    async fn event(
        &self,
        #[zbus(connection)] conn: &Connection,
        id: i32,
        event_id: String,
        _data: OwnedValue,
        _timestamp: u32,
    ) -> zbus::fdo::Result<()> {
        // Only "clicked" is meaningful for our menu items.
        if event_id != "clicked" {
            return Ok(());
        }

        // Update the toggle-state authoritatively on the server side so the
        // tray host does not have to guess:
        //  * a radio item selects itself and clears its radio group,
        //  * a checkbox item toggles,
        //  * a plain item changes nothing.
        let (event_to_send, changed) = {
            let mut menu_sys = self
                .menu_sys
                .write()
                .map_err(|_| zbus::fdo::Error::Failed("menu lock poisoned".to_string()))?;

            if let Some((event_id, changed)) = Self::apply_radio_selection(id, &mut menu_sys.items)
            {
                (event_id, changed)
            } else {
                match Self::find_item_by_id_mut(id, &mut menu_sys.items) {
                    Some(it) if it.is_checkable => {
                        it.is_checked = !it.is_checked;
                        (it.event_id.clone(), vec![(it.id, it.is_checked)])
                    }
                    Some(it) => (it.event_id.clone(), Vec::new()),
                    None => (None, Vec::new()),
                }
            }
        };

        // Push the corrected toggle-state(s) to the host immediately and bump
        // the layout revision so any cached layout is invalidated.
        if !changed.is_empty() {
            let updated: Vec<(i32, HashMap<String, OwnedValue>)> = changed
                .iter()
                .map(|(tid, checked)| {
                    let mut props = HashMap::new();
                    props.insert("toggle-state".to_string(), Self::toggle_state_value(*checked));
                    (*tid, props)
                })
                .collect();

            if let Ok(iface) = conn
                .object_server()
                .interface::<_, Self>("/MenuBar")
                .await
            {
                let emitter = iface.signal_emitter();
                let _ =
                    Self::items_properties_updated(&emitter, updated, Vec::new()).await;
                let _ = Self::layout_updated(&emitter, super::next_layout_revision(), 0).await;
            }
        }

        // Forward the click to the application so it can act on it (e.g.
        // rebuild the menu with the new selection).
        if let Some(event) = event_to_send {
            if let Ok(menu_sys) = self.menu_sys.read() {
                if let Some(tx) = &menu_sys.event_sender {
                    let _ = tx.send((id, event));
                }
            }
        }

        Ok(())
    }

    async fn event_group(
        &self,
        #[zbus(connection)] _conn: &Connection,
        _events: Vec<(i32, String, OwnedValue, u32)>,
    ) -> zbus::fdo::Result<Vec<i32>> {
        Ok(vec![])
    }

    async fn about_to_show(&self) -> zbus::fdo::Result<bool> {
        Ok(false)
    }

    async fn about_to_show_group(&self) -> zbus::fdo::Result<(Vec<i32>, Vec<i32>)> {
        Ok(Default::default())
    }

    // properties
    #[zbus(property)]
    fn version(&self) -> zbus::fdo::Result<u32> {
        Ok(3)
    }

    #[zbus(property)]
    async fn text_direction(&self) -> zbus::fdo::Result<String> {
        Ok("ltr".to_string())
    }

    #[zbus(property)]
    async fn status(&self) -> zbus::fdo::Result<String> {
        Ok("normal".to_string())
    }

    #[zbus(property)]
    async fn icon_theme_path(&self) -> zbus::fdo::Result<Vec<String>> {
        Ok(vec![])
    }

    // signals
    #[zbus(signal)]
    pub async fn items_properties_updated(
        ctxt: &SignalEmitter<'_>,
        updated_props: Vec<(i32, HashMap<String, OwnedValue>)>,
        removed_props: Vec<(i32, Vec<String>)>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn layout_updated(
        ctxt: &SignalEmitter<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}