//! `ui.make(container, tree)` — the Lua half of the data-driven UI builder.
//!
//! Two jobs, deliberately kept apart from the rules in `floptle_ui::make`:
//! reading a Lua table into a [`MadeNode`] tree, and doing what the resulting
//! diff says to the ECS.
//!
//! The behaviour hooks are why the parser exists at all rather than the scene
//! format growing a "build from data" node. A described button carries its own
//! `onClicked` closure — a screen's structure AND what it does in one place,
//! with no prefab and no second file. Those closures can't travel through
//! `floptle-ui` (a leaf crate that has never heard of Lua), so they ride
//! alongside the tree, addressed by path, and get bound to entities once the
//! reconcile has decided which entity each described node is.

use floptle_core::{Entity, Made, Matter, Name, Parent, World, transform::Transform};
use floptle_ui::ElementSpec;
use floptle_ui::make::{Existing, Kind, MadeNode, Op, PropVal};
use mlua::{Lua, RegistryKey, Table, Value};

/// Every UI hook a described element can carry a closure for, as `on` + the
/// hook's own name capitalised (`clicked` → `onClicked`). One rule instead of
/// a list of aliases: whatever a script can write as a function in a script
/// file, a made element can carry inline.
pub const HOOKS: &[&str] = &[
    "clicked",
    "pressed",
    "released",
    "hoverStart",
    "hoverEnd",
    "changed",
    "submitted",
    "cancelled",
    "focusEnter",
    "focusExit",
    "dragStart",
    "dragMove",
    "dragEnter",
    "dragOver",
    "dragLeave",
    "dragCancel",
    "dropped",
];

/// The hook an `onSomething` key names, if any.
pub fn hook_of(key: &str) -> Option<&'static str> {
    let rest = key.strip_prefix("on")?;
    let mut c = rest.chars();
    let lowered: String = c.next()?.to_lowercase().collect::<String>() + c.as_str();
    HOOKS.iter().copied().find(|h| *h == lowered)
}

/// Could this element ever fire this hook?
///
/// The interaction pass only looks at elements that opted in — a plain box is
/// scenery, and hit-testing every rectangle on screen would make "click the
/// panel behind the button" a bug you couldn't turn off. So a listener on the
/// wrong element is silent, and silence is a bad error message: this is what
/// lets `ui.on` say so instead.
pub fn hook_reaches(spec: &ElementSpec, hook: &str) -> bool {
    let takes_pointer =
        spec.button || spec.slider.is_some_and(|s| s.interact) || spec.field.is_some();
    match hook {
        // A focused element answers a submit press with `clicked`, so a
        // pad-only menu is a legitimate reason to have no `button`.
        "clicked" | "pressed" | "released" => takes_pointer || spec.focusable,
        // Hovering a tooltip element IS an interaction, and a draggable one is
        // hovered before it's picked up.
        "hoverStart" | "hoverEnd" => {
            takes_pointer || spec.draggable || !spec.tooltip.is_empty() || spec.drop_target
        }
        "changed" => spec.field.is_some() || spec.slider.is_some(),
        "submitted" | "cancelled" => spec.field.is_some(),
        "focusEnter" | "focusExit" => spec.focusable || spec.field.is_some(),
        "dragStart" | "dragMove" | "dragCancel" => spec.draggable,
        "dragEnter" | "dragOver" | "dragLeave" => spec.drop_target,
        "dropped" => spec.drop_target || spec.draggable,
        _ => true,
    }
}

/// What to turn on so `hook` can reach this element — the second half of the
/// warning, because "it will never fire" without "tick Button" is a riddle.
pub fn hook_needs(hook: &str) -> &'static str {
    match hook {
        "clicked" | "pressed" | "released" => "Button (or Focusable, for pad input)",
        "hoverStart" | "hoverEnd" => "Button, a tooltip, or Draggable",
        "changed" => "a text field or an interactive slider",
        "submitted" | "cancelled" => "a text field",
        "focusEnter" | "focusExit" => "Focusable",
        "dragStart" | "dragMove" | "dragCancel" => "Draggable",
        "dragEnter" | "dragOver" | "dragLeave" | "dropped" => "Drop target",
        _ => "the matching element setting",
    }
}

/// A described element's behaviour closure: where it sits in the tree, which
/// hook it answers, and the function itself.
pub type Hook = (Vec<u16>, &'static str, RegistryKey);

/// One `ui.make` call, queued for the driver to apply.
pub struct MakeRequest {
    /// The element or layer node the described tree lives under.
    pub container: u32,
    pub roots: Vec<MadeNode>,
    /// `(path, hook, function)`. The path indexes `roots`, then children.
    pub hooks: Vec<Hook>,
}

/// What a reconcile decided.
pub struct MakeResult {
    /// Which entity each described node turned out to be, by path — how the
    /// queued closures find the elements they belong to.
    pub bound: Vec<(Vec<u16>, u32)>,
    /// Elements that are no longer described. Handed back rather than
    /// despawned here so the driver's ONE destroy path runs: it also clears
    /// script environments and physics, which a made container's repeater rows
    /// may well have.
    pub destroy: Vec<u32>,
}

// ---------------------------------------------------------------------------
// Lua table -> description
// ---------------------------------------------------------------------------

/// Read the tree argument: one element table, or an array of them.
pub fn parse_tree(lua: &Lua, v: &Value) -> mlua::Result<(Vec<MadeNode>, Vec<Hook>)> {
    let Value::Table(t) = v else {
        return Err(mlua::Error::runtime("ui.make expects (node, table)"));
    };
    let mut hooks = Vec::new();
    let mut roots = Vec::new();
    // A list of elements, or one element? Only a list starts with a table:
    // an element starts with its kind, or with nothing and some properties.
    if matches!(t.raw_get::<Value>(1)?, Value::Table(_)) {
        for i in 1..=t.raw_len() {
            let child: Value = t.raw_get(i)?;
            let Value::Table(ct) = child else {
                return Err(mlua::Error::runtime(format!(
                    "ui.make: entry {i} of the tree is not an element table"
                )));
            };
            let path = vec![(roots.len()) as u16];
            roots.push(parse_node(lua, &ct, &path, &mut hooks, "")?);
        }
    } else {
        roots.push(parse_node(lua, t, &[0], &mut hooks, "")?);
    }
    Ok((roots, hooks))
}

fn parse_node(
    lua: &Lua,
    t: &Table,
    path: &[u16],
    hooks: &mut Vec<Hook>,
    trail: &str,
) -> mlua::Result<MadeNode> {
    // The kind is the first entry when it's a string; otherwise the table is
    // all properties and children, and it's a box.
    let (kind, first_child) = match t.raw_get::<Value>(1)? {
        Value::String(s) => {
            let name = s.to_str()?.to_string();
            let kind = Kind::parse(&name).ok_or_else(|| {
                mlua::Error::runtime(format!(
                    "ui.make{trail}: no element kind called '{name}' (have: {})",
                    Kind::NAMES.join(", ")
                ))
            })?;
            (kind, 2)
        }
        _ => (Kind::Box, 1),
    };
    let mut node = MadeNode { kind, ..Default::default() };
    // `items` turns each function child into one child per item, which is the
    // whole reason this exists: the roster is four fighters today and nine
    // tomorrow, and the scene file can't hold either.
    let items: Option<Vec<Value>> = match t.raw_get::<Value>("items")? {
        Value::Nil => None,
        Value::Table(list) => {
            Some((1..=list.raw_len()).map(|i| list.raw_get(i)).collect::<mlua::Result<_>>()?)
        }
        _ => return Err(mlua::Error::runtime(format!("ui.make{trail}: `items` must be a table"))),
    };
    // ---- properties ----
    for pair in t.pairs::<Value, Value>() {
        let (k, v) = pair?;
        let Value::String(k) = k else { continue };
        let key = k.to_str()?.to_string();
        // Three keys are the builder's own, not the element's.
        match key.as_str() {
            "items" => continue,
            "key" => {
                node.key = string_of(&v);
                continue;
            }
            "name" => {
                node.name = string_of(&v);
                continue;
            }
            _ => {}
        }
        if let Some(hook) = hook_of(&key) {
            let Value::Function(f) = v else {
                return Err(mlua::Error::runtime(format!(
                    "ui.make{trail}: {key} must be a function"
                )));
            };
            hooks.push((path.to_vec(), hook, lua.create_registry_value(f)?));
            continue;
        }
        let val = prop_value(&v).ok_or_else(|| {
            mlua::Error::runtime(format!(
                "ui.make{trail}: `{key}` is set to something that isn't a number, string, \
                 boolean, colour or list"
            ))
        })?;
        if !floptle_ui::make::known_prop(&key) {
            let near = floptle_ui::make::suggest(&key);
            let hint = if near.is_empty() {
                String::new()
            } else {
                format!(" (did you mean {}?)", near.join(", "))
            };
            return Err(mlua::Error::runtime(format!(
                "ui.make{trail}: no UI property called `{key}`{hint}"
            )));
        }
        node.props.push((key, val));
    }
    // A stable order, so two calls that describe the same screen produce the
    // same spec: Lua's `pairs` makes no promises about hash order at all.
    node.props.sort_by(|a, b| a.0.cmp(&b.0));
    // ---- children ----
    let n = t.raw_len();
    for i in first_child..=n {
        let child: Value = t.raw_get(i)?;
        let mut push = |node_t: &Table, made: &mut MadeNode| -> mlua::Result<()> {
            let mut p = path.to_vec();
            p.push(made.children.len() as u16);
            let trail = format!("{trail} > {:?}", made.kind);
            let child = parse_node(lua, node_t, &p, hooks, &trail)?;
            made.children.push(child);
            Ok(())
        };
        match child {
            Value::Table(ct) => push(&ct, &mut node)?,
            Value::Function(f) => match &items {
                // The mapping function: called once per item, with the item
                // and its 1-based position. Returning nil skips that row,
                // which is how a filtered list stays one expression.
                Some(list) => {
                    for (idx, item) in list.iter().enumerate() {
                        match f.call::<Value>((item.clone(), idx + 1))? {
                            Value::Table(ct) => push(&ct, &mut node)?,
                            Value::Nil => {}
                            _ => {
                                return Err(mlua::Error::runtime(format!(
                                    "ui.make{trail}: the function for item {} returned something \
                                     that isn't an element table",
                                    idx + 1
                                )));
                            }
                        }
                    }
                }
                // No `items`: a plain deferred child, so a conditional part of
                // a screen can be `function() if paused then return {...} end end`.
                None => match f.call::<Value>(())? {
                    Value::Table(ct) => push(&ct, &mut node)?,
                    Value::Nil => {}
                    _ => {
                        return Err(mlua::Error::runtime(format!(
                            "ui.make{trail}: a function child must return an element table or nil"
                        )));
                    }
                },
            },
            Value::Nil => {}
            _ => {
                return Err(mlua::Error::runtime(format!(
                    "ui.make{trail}: child {i} is not an element table"
                )));
            }
        }
    }
    if items.is_some() && node.children.is_empty() {
        return Err(mlua::Error::runtime(format!(
            "ui.make{trail}: `items` needs a function child to turn each item into an element"
        )));
    }
    Ok(node)
}

fn string_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.to_str().map(|s| s.to_string()).unwrap_or_default(),
        Value::Integer(i) => i.to_string(),
        Value::Number(n) => format!("{n}"),
        _ => String::new(),
    }
}

/// A Lua value as a property. Colours arrive as the plain `{r,g,b,a}` table
/// `color()` returns, and any other number array is a list.
fn prop_value(v: &Value) -> Option<PropVal> {
    Some(match v {
        Value::Integer(i) => PropVal::Num(*i as f32),
        Value::Number(n) => PropVal::Num(*n as f32),
        Value::Boolean(b) => PropVal::Bool(*b),
        Value::String(s) => PropVal::Str(s.to_str().ok()?.to_string()),
        Value::Table(t) => {
            // A colour is the `{r=,g=,b=,a=}` table `color()` returns; anything
            // else with numbers in it is a list. Told apart by the NAMED keys,
            // because `read_color` also accepts positional ones — and under
            // that reading `radius = {8, 8, 0, 0}` would be a colour.
            let named = ["r", "g", "b", "a"]
                .iter()
                .any(|k| t.raw_get::<Option<f64>>(*k).ok().flatten().is_some());
            if named {
                PropVal::Color(crate::api::read_color(t).ok()?)
            } else {
                let n = t.raw_len();
                let mut list = Vec::with_capacity(n);
                for i in 1..=n {
                    list.push(t.raw_get::<f32>(i).ok()?);
                }
                PropVal::List(list)
            }
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Description -> ECS
// ---------------------------------------------------------------------------

/// Bring `container`'s made children in line with `roots`.
///
/// Only elements this function created are ever touched — see [`Made`]. An
/// element you placed in the scene under the same container is left exactly
/// where it is, which is what makes it safe to hang a data-driven list inside
/// a hand-designed panel.
pub fn reconcile(world: &mut World, container: u32, roots: &[MadeNode]) -> MakeResult {
    let mut out = MakeResult { bound: Vec::new(), destroy: Vec::new() };
    let Some(parent) = entity_of(world, container) else { return out };
    reconcile_children(world, parent, roots, &mut Vec::new(), &mut out);
    out
}

fn entity_of(world: &World, index: u32) -> Option<Entity> {
    world.query::<Transform>().map(|(e, _)| e).find(|e| e.index() == index)
}

fn reconcile_children(
    world: &mut World,
    parent: Entity,
    wanted: &[MadeNode],
    path: &mut Vec<u16>,
    out: &mut MakeResult,
) {
    let mut have: Vec<(Entity, Made)> = world
        .query::<Made>()
        .filter(|(e, _)| world.get::<Parent>(*e).is_some_and(|p| p.0 == parent))
        .map(|(e, m)| (e, m.clone()))
        .collect();
    have.sort_by_key(|(e, m)| (m.slot, e.index()));
    let existing: Vec<Existing> = have
        .iter()
        .map(|(_, m)| Existing {
            key: m.key.clone(),
            kind: Kind::parse(&m.kind).unwrap_or_default(),
        })
        .collect();
    for op in floptle_ui::make::plan(&existing, wanted) {
        match op {
            Op::Keep { old, new } => {
                let e = have[old].0;
                let node = &wanted[new];
                let spec = world
                    .get::<ElementSpec>(e)
                    .map(|s| node.rebuild(s))
                    .unwrap_or_else(|| node.build());
                install(world, e, node, spec, new);
                descend(world, e, node, path, new, out);
            }
            Op::Create { new } => {
                let node = &wanted[new];
                let e = world.spawn();
                world.insert(e, Transform::IDENTITY);
                world.insert(e, Matter::Empty);
                world.insert(e, Parent(parent));
                install(world, e, node, node.build(), new);
                descend(world, e, node, path, new, out);
            }
            Op::Remove { old } => collect_subtree(world, have[old].0, &mut out.destroy),
        }
    }
}

/// Write a described node onto its entity.
fn install(world: &mut World, e: Entity, node: &MadeNode, mut spec: ElementSpec, slot: usize) {
    // Described order IS draw and flow order. Without this a keyed row that
    // moved would still draw where its entity was created — invisible in a
    // Free layout, and wrong in every stack.
    if !node.mentions("order") {
        spec.order = slot as i32;
    }
    world.insert(e, spec);
    world.insert(e, Name(node_name(node, slot)));
    world.insert(e, Made {
        key: node.key.clone(),
        slot: slot as u32,
        kind: kind_name(node.kind).to_string(),
    });
}

fn descend(
    world: &mut World,
    e: Entity,
    node: &MadeNode,
    path: &mut Vec<u16>,
    slot: usize,
    out: &mut MakeResult,
) {
    path.push(slot as u16);
    out.bound.push((path.clone(), e.index()));
    reconcile_children(world, e, &node.children, path, out);
    path.pop();
}

/// A made element's node name: what you asked for, else its key, else its kind
/// and position. Names matter — masks, scrollbars and nav overrides all
/// address elements by name — so "unnamed" is not an option.
fn node_name(node: &MadeNode, slot: usize) -> String {
    if !node.name.is_empty() {
        return node.name.clone();
    }
    if !node.key.is_empty() {
        return node.key.clone();
    }
    format!("{}{slot}", kind_name(node.kind))
}

fn kind_name(k: Kind) -> &'static str {
    match k {
        Kind::Box => "box",
        Kind::Row => "row",
        Kind::Col => "col",
        Kind::Text => "text",
        Kind::Image => "image",
        Kind::Button => "button",
        Kind::Field => "field",
        Kind::Slider => "slider",
        Kind::Scroll => "scroll",
    }
}

fn collect_subtree(world: &World, e: Entity, out: &mut Vec<u32>) {
    out.push(e.index());
    let kids: Vec<Entity> = world
        .query::<Parent>()
        .filter(|(_, p)| p.0 == e)
        .map(|(k, _)| k)
        .collect();
    for k in kids {
        collect_subtree(world, k, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use floptle_ui::Size;

    /// What `ui.on` warns about: an element that could never fire the hook.
    /// The two that matter most are the near-misses — a pad-only menu item is
    /// focusable without being a button, and a drop target is hovered without
    /// being one either.
    #[test]
    fn a_hook_only_reaches_an_element_that_can_fire_it() {
        let plain = ElementSpec::default();
        assert!(!hook_reaches(&plain, "clicked"));
        assert!(!hook_reaches(&plain, "hoverStart"));
        let btn = ElementSpec { button: true, ..Default::default() };
        assert!(hook_reaches(&btn, "clicked") && hook_reaches(&btn, "hoverStart"));
        assert!(!hook_reaches(&btn, "dropped"), "a button is not a drop target");
        let pad = ElementSpec { focusable: true, ..Default::default() };
        assert!(hook_reaches(&pad, "clicked"), "submit on a focused element IS a click");
        let slot = ElementSpec { drop_target: true, ..Default::default() };
        assert!(hook_reaches(&slot, "dropped") && hook_reaches(&slot, "hoverStart"));
        assert!(hook_needs("clicked").contains("Button"));
    }

    fn container(world: &mut World) -> Entity {
        let e = world.spawn();
        world.insert(e, Transform::IDENTITY);
        world.insert(e, Matter::Empty);
        world.insert(e, ElementSpec::default());
        e
    }

    fn made_children(world: &World, parent: Entity) -> Vec<Entity> {
        let mut v: Vec<(u32, Entity)> = world
            .query::<Made>()
            .filter(|(e, _)| world.get::<Parent>(*e).is_some_and(|p| p.0 == parent))
            .map(|(e, m)| (m.slot, e))
            .collect();
        v.sort_by_key(|(slot, e)| (*slot, e.index()));
        v.into_iter().map(|(_, e)| e).collect()
    }

    fn parse(lua: &Lua, src: &str) -> (Vec<MadeNode>, Vec<Hook>) {
        let v: Value = lua.load(src).eval().expect("the test's Lua is valid");
        parse_tree(lua, &v).expect("parses")
    }

    #[test]
    fn a_table_becomes_a_tree() {
        let lua = Lua::new();
        let (roots, _) = parse(
            &lua,
            r#"return { "col", gap = 12, pad = 4,
                 { "text", text = "Roster" },
                 { "row", { "image", texture = "a.png" }, { "image", texture = "b.png" } },
               }"#,
        );
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].kind, Kind::Col);
        assert_eq!(roots[0].children.len(), 2);
        assert_eq!(roots[0].children[0].kind, Kind::Text);
        assert_eq!(roots[0].children[1].children.len(), 2);
        let spec = roots[0].build();
        assert_eq!(spec.stack.unwrap().gap, 12.0);
    }

    /// The character-select strip from the proposal, in the form that made it
    /// worth building: a row per item, with the count coming from data.
    #[test]
    fn items_turn_a_list_into_children() {
        let lua = Lua::new();
        let (roots, _) = parse(
            &lua,
            r#"return { "row", gap = 12, justify = "center",
                 items = { "ana", "bo", "cy" },
                 function(id, i) return { "image", key = id, texture = id .. ".png" } end,
               }"#,
        );
        assert_eq!(roots[0].children.len(), 3);
        assert_eq!(roots[0].children[1].key, "bo");
        let img = roots[0].children[2].build().image.unwrap();
        assert_eq!(img.texture, "cy.png");
    }

    #[test]
    fn a_mapping_function_can_skip_an_item() {
        let lua = Lua::new();
        let (roots, _) = parse(
            &lua,
            r#"return { "col", items = { 1, 2, 3, 4 },
                 function(n) if n % 2 == 0 then return { "text", text = tostring(n) } end end }"#,
        );
        assert_eq!(roots[0].children.len(), 2);
    }

    #[test]
    fn hooks_ride_along_addressed_by_path() {
        let lua = Lua::new();
        let (_, hooks) = parse(
            &lua,
            r#"return { "col",
                 { "button", onClicked = function() end },
                 { "field", onSubmitted = function() end, onChanged = function() end },
               }"#,
        );
        assert_eq!(hooks.len(), 3);
        assert!(hooks.iter().any(|(p, h, _)| p == &vec![0, 0] && *h == "clicked"));
        assert!(hooks.iter().any(|(p, h, _)| p == &vec![0, 1] && *h == "submitted"));
    }

    #[test]
    fn on_names_map_to_hooks_and_nothing_else_does() {
        assert_eq!(hook_of("onClicked"), Some("clicked"));
        assert_eq!(hook_of("onHoverStart"), Some("hoverStart"));
        assert_eq!(hook_of("onDropped"), Some("dropped"));
        assert_eq!(hook_of("onClick"), None, "the hook is `clicked`, so the key is onClicked");
        assert_eq!(hook_of("order"), None);
    }

    /// A property with a typo must stop the build with a message, not paint a
    /// screen that silently ignores a line of the description.
    #[test]
    fn a_typo_is_an_error_that_names_the_property() {
        let lua = Lua::new();
        let v: Value = lua.load(r#"return { "box", colour = 1 }"#).eval().unwrap();
        let err = parse_tree(&lua, &v).unwrap_err().to_string();
        assert!(err.contains("colour"), "{err}");
        let v: Value = lua.load(r#"return { "bax" }"#).eval().unwrap();
        let err = parse_tree(&lua, &v).unwrap_err().to_string();
        assert!(err.contains("bax") && err.contains("box"), "{err}");
    }

    #[test]
    fn a_table_of_tables_is_a_list_of_roots() {
        let lua = Lua::new();
        let (roots, _) = parse(&lua, r#"return { { "text", text = "a" }, { "text", text = "b" } }"#);
        assert_eq!(roots.len(), 2);
    }

    /// Lua hands properties over in hash order, which differs run to run. The
    /// description has to come out the same both times or the "did anything
    /// change" comparison downstream is meaningless.
    #[test]
    fn the_same_table_always_parses_to_the_same_description() {
        let lua = Lua::new();
        let src =
            r##"return { "box", w = 10, fill = "#ff0000", radius = 4, text = "x", pad = 2 }"##;
        let (a, _) = parse(&lua, src);
        let (b, _) = parse(&lua, src);
        assert_eq!(a, b);
    }

    // ---- reconcile ---------------------------------------------------------

    #[test]
    fn a_described_tree_becomes_nodes() {
        let mut world = World::new();
        let c = container(&mut world);
        let lua = Lua::new();
        let (roots, _) = parse(
            &lua,
            r#"return { "col", { "text", text = "one" }, { "text", text = "two" } }"#,
        );
        let out = reconcile(&mut world, c.index(), &roots);
        assert!(out.destroy.is_empty());
        let kids = made_children(&world, c);
        assert_eq!(kids.len(), 1, "one root under the container");
        let labels = made_children(&world, kids[0]);
        assert_eq!(labels.len(), 2);
        let t = world.get::<ElementSpec>(labels[1]).unwrap().text.as_ref().unwrap();
        assert_eq!(t.text, "two");
        // Every described node reports the entity it became, so its closures
        // can be bound.
        assert_eq!(out.bound.len(), 3);
    }

    /// The property that makes this a builder and not a node factory.
    #[test]
    fn a_second_call_keeps_the_elements_it_can() {
        let mut world = World::new();
        let c = container(&mut world);
        let lua = Lua::new();
        let (three, _) = parse(
            &lua,
            r#"return { "col", items = { "a", "b", "c" },
                 function(id) return { "text", key = id, text = id } end }"#,
        );
        reconcile(&mut world, c.index(), &three);
        let col = made_children(&world, c)[0];
        let before = made_children(&world, col);
        assert_eq!(before.len(), 3);

        let (four, _) = parse(
            &lua,
            r#"return { "col", items = { "a", "b", "c", "d" },
                 function(id) return { "text", key = id, text = id } end }"#,
        );
        let out = reconcile(&mut world, c.index(), &four);
        let after = made_children(&world, col);
        assert!(out.destroy.is_empty(), "nothing is thrown away to add a row");
        assert_eq!(after.len(), 4);
        assert_eq!(&after[..3], &before[..], "the first three are the SAME entities");
    }

    /// Reordering keyed rows moves the entities, not just the labels — and the
    /// described order becomes the flow order.
    #[test]
    fn keyed_rows_carry_their_entity_through_a_reorder() {
        let mut world = World::new();
        let c = container(&mut world);
        let lua = Lua::new();
        let (asc, _) = parse(
            &lua,
            r#"return { "col", items = { "a", "b", "c" },
                 function(id) return { "text", key = id, text = id } end }"#,
        );
        reconcile(&mut world, c.index(), &asc);
        let col = made_children(&world, c)[0];
        let before = made_children(&world, col);

        let (desc, _) = parse(
            &lua,
            r#"return { "col", items = { "c", "b", "a" },
                 function(id) return { "text", key = id, text = id } end }"#,
        );
        reconcile(&mut world, c.index(), &desc);
        let after = made_children(&world, col);
        assert_eq!(after, vec![before[2], before[1], before[0]]);
        // …and the one that is now first draws and flows first.
        assert_eq!(world.get::<ElementSpec>(after[0]).unwrap().order, 0);
    }

    #[test]
    fn a_row_that_leaves_is_handed_back_for_destruction() {
        let mut world = World::new();
        let c = container(&mut world);
        let lua = Lua::new();
        let (two, _) = parse(
            &lua,
            r#"return { "col", items = { "a", "b" },
                 function(id) return { "text", key = id, text = id } end }"#,
        );
        reconcile(&mut world, c.index(), &two);
        let col = made_children(&world, c)[0];
        let gone = made_children(&world, col)[1];

        let (one, _) = parse(
            &lua,
            r#"return { "col", items = { "a" },
                 function(id) return { "text", key = id, text = id } end }"#,
        );
        let out = reconcile(&mut world, c.index(), &one);
        assert_eq!(out.destroy, vec![gone.index()]);
    }

    /// Hand-placed siblings are not the builder's business. A data-driven list
    /// inside a designed panel must not eat the panel.
    #[test]
    fn elements_the_builder_did_not_make_are_left_alone() {
        let mut world = World::new();
        let c = container(&mut world);
        let authored = world.spawn();
        world.insert(authored, Transform::IDENTITY);
        world.insert(authored, Name("Title".into()));
        world.insert(authored, Parent(c));
        world.insert(authored, ElementSpec::default());

        let lua = Lua::new();
        let (roots, _) = parse(&lua, r#"return { "text", text = "made" }"#);
        let out = reconcile(&mut world, c.index(), &roots);
        assert!(out.destroy.is_empty());
        assert!(world.is_alive(authored));
        assert_eq!(world.get::<Name>(authored).unwrap().0, "Title");

        // …and a second call with nothing described removes only what it made.
        let out = reconcile(&mut world, c.index(), &[]);
        assert_eq!(out.destroy.len(), 1);
        assert!(world.is_alive(authored));
    }

    /// A described property that disappears must stop applying, or the table
    /// stops describing the screen.
    #[test]
    fn dropping_a_property_puts_it_back_to_default() {
        let mut world = World::new();
        let c = container(&mut world);
        let lua = Lua::new();
        let (wide, _) = parse(&lua, r#"return { "box", w = 200 }"#);
        reconcile(&mut world, c.index(), &wide);
        let e = made_children(&world, c)[0];
        assert_eq!(world.get::<ElementSpec>(e).unwrap().size[0], Size::Fixed(200.0));

        let (plain, _) = parse(&lua, r#"return { "box" }"#);
        reconcile(&mut world, c.index(), &plain);
        assert_eq!(world.get::<ElementSpec>(e).unwrap().size[0], Size::Fit);
    }

    /// Every kind must be recognisable as itself on the next call, INCLUDING
    /// when properties have blurred it: a `text` with a background fill and a
    /// `box` with a label are the same ElementSpec. Inferring the kind from
    /// the element would rebuild those two from scratch every single call.
    #[test]
    fn a_kind_is_still_itself_after_properties_disguise_it() {
        let lua = Lua::new();
        let src = r#"return { "col",
             { "text", key = "a", text = "hi", fill = { r = 0, g = 0, b = 0, a = 1 } },
             { "box",  key = "b", text = "also hi" },
             { "button", key = "c", texture = "x.png" } }"#;
        let mut world = World::new();
        let c = container(&mut world);
        let (roots, _) = parse(&lua, src);
        reconcile(&mut world, c.index(), &roots);
        let col = made_children(&world, c)[0];
        let before = made_children(&world, col);

        let (again, _) = parse(&lua, src);
        let out = reconcile(&mut world, c.index(), &again);
        assert!(out.destroy.is_empty(), "a re-render must not churn the screen");
        assert_eq!(made_children(&world, col), before);
    }
}
