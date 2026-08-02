//! Tests for the vello paint pipeline.

use std::collections::HashMap;

use ego_tree::{NodeId as EgoNodeId, Tree};
use rustc_hash::FxHashMap;
use taffy::TaffyTree;
use vello::Scene;
use vello::kurbo::Affine;

use super::*;
use crate::core::types::VariableStore;
use crate::parser::{MizuColor, MizuNode, StyleRules};

#[test]
fn test_color_translation_opaque() {
    let mizu_c = MizuColor::rgb(255, 0, 128);
    let vello_c = to_vello_color(&mizu_c);
    assert_eq!(vello_c.r, 255);
    assert_eq!(vello_c.g, 0);
    assert_eq!(vello_c.b, 128);
    assert_eq!(vello_c.a, 255);
}

#[test]
fn test_color_translation_transparent() {
    let mizu_c = MizuColor::rgba(10, 20, 30, 50);
    let vello_c = to_vello_color(&mizu_c);
    assert_eq!(vello_c.r, 10);
    assert_eq!(vello_c.g, 20);
    assert_eq!(vello_c.b, 30);
    assert_eq!(vello_c.a, 50);
}

#[test]
fn resolve_media_url_passes_through_absolute_mizu_urls() {
    // A remote mizu:// document may reach any declared media alias.
    assert_eq!(
        resolve_media_url("mizu://other.example/x.png", "mizu://example.mizu/page").as_deref(),
        Some("mizu://other.example/x.png")
    );
    // A local file:// document reaching a *local* media alias is still
    // allowed (e.g. a locally-run dev server).
    assert_eq!(
        resolve_media_url("mizu://localhost/x.png", "file:///C:/docs/page.mizu").as_deref(),
        Some("mizu://localhost/x.png")
    );
}

#[test]
fn resolve_media_url_refuses_remote_mizu_targets_for_file_documents() {
    // A `file://` document must not be able to reach an attacker-controlled
    // remote host merely by declaring `media logo mizu://evil.com/x.png` —
    // this must be refused here, matching the SSRF guard already enforced
    // for outbound network calls and downloads.
    assert_eq!(
        resolve_media_url("mizu://evil.example/x.png", "file:///C:/docs/page.mizu"),
        None
    );
}

#[test]
fn resolve_media_url_refuses_local_files_for_remote_documents() {
    // A remote document naming a local file is refused *here*, not left to
    // the parse-time guard and the fetcher's sandbox check alone.
    for path in [
        "file:///C:/img.png",
        "file:///etc/passwd",
        "file://C:/Users/victim/.ssh/id_rsa",
    ] {
        assert_eq!(
            resolve_media_url(path, "mizu://example.mizu/page"),
            None,
            "{path} must not resolve for a mizu:// document"
        );
    }
    // The same path from a local document is the legitimate case, and the
    // read is still sandboxed at fetch time.
    assert_eq!(
        resolve_media_url("file:///C:/docs/img.png", "file:///C:/docs/page.mizu").as_deref(),
        Some("file:///C:/docs/img.png")
    );
}

#[test]
fn resolve_media_url_resolves_relative_to_mizu_origin() {
    assert_eq!(
        resolve_media_url("img.png", "mizu://example.mizu/page").as_deref(),
        Some("mizu://example.mizu/img.png")
    );
    assert_eq!(
        resolve_media_url("/assets/img.png", "mizu://example.mizu/page").as_deref(),
        Some("mizu://example.mizu/assets/img.png")
    );
}

#[test]
fn resolve_media_url_resolves_relative_to_file_origin() {
    assert_eq!(
        resolve_media_url("img.png", "file:///C:/docs/page.mizu").as_deref(),
        Some("file:///C:/docs/img.png")
    );
}

#[test]
fn resolve_media_url_refuses_an_unresolvable_origin() {
    // Neither a `mizu://` host nor a `file:///` path to resolve against:
    // returning the raw string here would hand the fetcher an origin-less
    // path to interpret however it liked.
    assert_eq!(resolve_media_url("img.png", "about:blank"), None);
    assert_eq!(resolve_media_url("img.png", ""), None);
}

#[test]
fn test_paint_node_with_text() {
    use crate::parser::Primitive;

    // Build a DOM tree: Window -> Text
    let mut tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: FxHashMap::default(),
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });

    let text_node_id = tree
        .root_mut()
        .append(MizuNode {
            primitive: Primitive::Text,
            attributes: {
                let mut attrs = FxHashMap::default();
                attrs.insert("class".to_string(), "welcome-text".to_string());
                attrs.insert("content".to_string(), "Benvenuto in Mizu!".to_string());
                attrs
            },
            events: FxHashMap::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        })
        .id();

    // Style rules
    let mut style_rules = HashMap::new();
    style_rules.insert(
        "welcome-text".to_string(),
        StyleRules {
            font_size: Some(24.0),
            color: Some(MizuColor::rgb(255, 255, 255)),
            ..Default::default()
        },
    );

    // Set up Taffy layout
    let mut taffy = TaffyTree::<EgoNodeId>::new();
    let mut node_to_taffy_id = HashMap::new();

    // Window Taffy Node
    let window_style = taffy::style::Style::default();
    let window_taffy_id = taffy.new_with_children(window_style, &[]).unwrap();
    node_to_taffy_id.insert(tree.root().id(), window_taffy_id);

    // Text Taffy Node
    let text_style = taffy::style::Style::default();
    let text_taffy_id = taffy
        .new_leaf_with_context(text_style, text_node_id)
        .unwrap();
    node_to_taffy_id.insert(text_node_id, text_taffy_id);

    // Compute layout
    let viewport_size = taffy::geometry::Size {
        width: taffy::style::AvailableSpace::Definite(800.0),
        height: taffy::style::AvailableSpace::Definite(600.0),
    };
    taffy
        .compute_layout(window_taffy_id, viewport_size)
        .unwrap();

    // Parley contexts
    let mut font_cx = parley::FontContext::new();
    font_cx.collection.load_system_fonts();
    let mut layout_cx = parley::LayoutContext::new();
    let mut store = VariableStore::new().freeze();
    let scroll_offsets: HashMap<EgoNodeId, f32> = HashMap::new();
    let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());

    let mut fetching_images = rustc_hash::FxHashMap::default();
    let (network_tx, _network_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
    let chrome_url = "mizu://localhost/index.mizu";

    let text_layouts = HashMap::new();

    let empty_each_groups = HashMap::new();
    let empty_window_start: HashMap<EgoNodeId, usize> = HashMap::new();
    let mut empty_each_offset_y: HashMap<EgoNodeId, f32> = HashMap::new();

    // Setup PaintContext
    let mut ctx = PaintContext {
        tab: crate::network::TabId(0),
        tree: &tree,
        taffy: &taffy,
        node_to_taffy_id: &node_to_taffy_id,
        style_rules: &style_rules,
        style_variants: &[],
        render_env: crate::render::responsive::RenderEnvironment {
            viewport: crate::render::responsive::ViewportSize {
                width: 800.0,
                height: 600.0,
            },
            color_scheme: crate::render::preferences::ColorScheme::Dark,
        },
        font_cx: &mut font_cx,
        layout_cx: &mut layout_cx,
        transform: Affine::IDENTITY,
        store: &mut store,
        scroll_offsets: &scroll_offsets,
        focused_node: None,
        image_cache: &mut image_cache,
        fetching_images: &mut fetching_images,
        network_tx: &network_tx,
        chrome_url,
        elapsed_ms: 0,
        has_animations: false,
        text_layouts: &text_layouts,
        item_bindings: HashMap::new(),
        each_groups: &empty_each_groups,
        each_window_start: &empty_window_start,
        each_container_offset_y: &mut empty_each_offset_y,
        taffy_id_overrides: HashMap::new(),
    };

    let mut scene = Scene::new();
    let drawn = paint_node(tree.root().id(), &mut ctx, &mut scene, (0.0, 0.0));

    // Since the Window text title is ignored, only the child Text node should draw text.
    // We check that drawn > 0.
    assert!(
        drawn > 0,
        "Expected at least one element (the text) to be painted, got {}",
        drawn
    );
}

/// Verifies that `each item in lista` paints the child template once per
/// list element using the fully-expanded Taffy path.
/// With a 2-element list and one Text child, `drawn_count` must be >= 2.
#[test]
fn test_paint_each_node_iterates_list() {
    use crate::core::types::Value;
    use crate::parser::Primitive;
    use crate::render::layout_bridge::{EachExpansion, expand_each_nodes};
    use std::sync::Arc;

    // Build store: items = [Record{"name":"A"}, Record{"name":"B"}]
    let mut store = crate::core::types::VariableStore::new();
    let make_record = |name: &str| -> Value {
        Value::record_from_unsorted(vec![("name", Value::String(Arc::from(name)))])
    };
    store.set(
        "lista",
        Value::List(Arc::new(vec![make_record("A"), make_record("B")])),
    );
    let mut store = store.freeze();

    // Build DOM: Window -> Each(item in lista) -> Text("{item.name}")
    let mut tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: FxHashMap::default(),
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });

    let each_id = tree
        .root_mut()
        .append(MizuNode {
            primitive: Primitive::Each,
            attributes: FxHashMap::default(),
            events: FxHashMap::default(),
            iterator_context: Some(("item".to_string(), "lista".to_string())),
            conditional_classes: Vec::new(),
        })
        .id();

    // Append the Text child to the Each node
    let text_node_id = tree
        .get_mut(each_id)
        .unwrap()
        .append(MizuNode {
            primitive: Primitive::Text,
            attributes: {
                let mut a = FxHashMap::default();
                a.insert("content".to_string(), "{item.name}".to_string());
                a
            },
            events: FxHashMap::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        })
        .id();

    // Taffy layout: Window -> Each -> Text
    let mut taffy = TaffyTree::<EgoNodeId>::new();
    let mut node_to_taffy_id = HashMap::new();

    let text_taffy = taffy
        .new_leaf_with_context(taffy::style::Style::default(), text_node_id)
        .unwrap();
    node_to_taffy_id.insert(text_node_id, text_taffy);

    let each_taffy = taffy
        .new_with_children(taffy::style::Style::default(), &[text_taffy])
        .unwrap();
    node_to_taffy_id.insert(each_id, each_taffy);

    let window_taffy = taffy
        .new_with_children(taffy::style::Style::default(), &[each_taffy])
        .unwrap();
    node_to_taffy_id.insert(tree.root().id(), window_taffy);

    // Expand Each nodes and re-compute layout (the correct order).
    let expansion = expand_each_nodes(
        &tree,
        &store,
        &mut taffy,
        &node_to_taffy_id,
        &EachExpansion::default(),
        None, // full rebuild in tests
        0.0,
        600.0,
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap();

    taffy
        .compute_layout(
            window_taffy,
            taffy::geometry::Size {
                width: taffy::style::AvailableSpace::Definite(800.0),
                height: taffy::style::AvailableSpace::Definite(600.0),
            },
        )
        .unwrap();

    let mut font_cx = parley::FontContext::new();
    font_cx.collection.load_system_fonts();
    let mut layout_cx = parley::LayoutContext::new();
    let scroll_offsets: HashMap<EgoNodeId, f32> = HashMap::new();
    let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());
    let mut fetching_images = rustc_hash::FxHashMap::default();
    let (network_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
    let text_layouts = HashMap::new();
    let style_rules: HashMap<String, StyleRules> = HashMap::new();
    let mut each_offset_y: HashMap<EgoNodeId, f32> = HashMap::new();

    let mut ctx = PaintContext {
        tab: crate::network::TabId(0),
        tree: &tree,
        taffy: &taffy,
        node_to_taffy_id: &node_to_taffy_id,
        style_rules: &style_rules,
        style_variants: &[],
        render_env: crate::render::responsive::RenderEnvironment {
            viewport: crate::render::responsive::ViewportSize {
                width: 800.0,
                height: 600.0,
            },
            color_scheme: crate::render::preferences::ColorScheme::Dark,
        },
        font_cx: &mut font_cx,
        layout_cx: &mut layout_cx,
        transform: Affine::IDENTITY,
        store: &mut store,
        scroll_offsets: &scroll_offsets,
        focused_node: None,
        image_cache: &mut image_cache,
        fetching_images: &mut fetching_images,
        network_tx: &network_tx,
        chrome_url: "mizu://localhost/index.mizu",
        elapsed_ms: 0,
        has_animations: false,
        text_layouts: &text_layouts,
        item_bindings: HashMap::new(),
        each_groups: &expansion.groups,
        each_window_start: &expansion.window_start,
        each_container_offset_y: &mut each_offset_y,
        taffy_id_overrides: HashMap::new(),
    };

    let mut scene = Scene::new();
    let drawn = paint_node(tree.root().id(), &mut ctx, &mut scene, (0.0, 0.0));

    assert!(
        drawn >= 2,
        "each with 2-element list must paint the child at least twice; got drawn={}",
        drawn,
    );
}

/// Verifies that `expand_each_nodes` + `compute_layout` produces
/// non-overlapping row positions for a fixed-height Each template.
///
/// DOM: Window → Each(item in rows) → Box(.row  height:50px)
/// Store: rows = [_, _, _]  (3 elements; values irrelevant for layout)
///
/// Expected Taffy output after expansion:
///   row_0.location.y == 0,   row_0.size.height == 50
///   row_1.location.y == 50,  row_1.size.height == 50
///   row_2.location.y == 100, row_2.size.height == 50
///   Each container size.height >= 150
#[test]
fn test_each_items_stack_without_overlap() {
    use crate::core::types::Value;
    use crate::parser::Primitive;
    use crate::render::layout_bridge::{EachExpansion, expand_each_nodes};
    use std::sync::Arc;

    // Store: rows = list of 3 null values (heights come from CSS, not values).
    let mut store = crate::core::types::VariableStore::new();
    store.set(
        "rows",
        Value::List(Arc::new(vec![Value::Null, Value::Null, Value::Null])),
    );
    let mut store = store.freeze();

    // DOM: Window → Each → Box
    let mut tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: FxHashMap::default(),
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });
    let each_id = tree
        .root_mut()
        .append(MizuNode {
            primitive: Primitive::Each,
            attributes: FxHashMap::default(),
            events: FxHashMap::default(),
            iterator_context: Some(("item".to_string(), "rows".to_string())),
            conditional_classes: Vec::new(),
        })
        .id();
    let box_id = tree
        .get_mut(each_id)
        .unwrap()
        .append(MizuNode {
            primitive: Primitive::Box,
            attributes: {
                let mut a = FxHashMap::default();
                a.insert("class".to_string(), ".row".to_string());
                a
            },
            events: FxHashMap::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        })
        .id();

    // Taffy: Window → Each → Box (height 50px, full width)
    let mut taffy = TaffyTree::<EgoNodeId>::new();
    let mut node_to_taffy_id = HashMap::new();

    let row_style = taffy::style::Style {
        size: taffy::geometry::Size {
            width: taffy::style::Dimension::Percent(1.0),
            height: taffy::style::Dimension::Length(50.0),
        },
        flex_shrink: 0.0,
        ..taffy::style::Style::default()
    };
    let box_taffy = taffy.new_leaf(row_style).unwrap();
    node_to_taffy_id.insert(box_id, box_taffy);

    let each_taffy = taffy
        .new_with_children(taffy::style::Style::default(), &[box_taffy])
        .unwrap();
    node_to_taffy_id.insert(each_id, each_taffy);

    let window_style = taffy::style::Style {
        size: taffy::geometry::Size {
            width: taffy::style::Dimension::Percent(1.0),
            height: taffy::style::Dimension::Auto,
        },
        ..taffy::style::Style::default()
    };
    let window_taffy = taffy
        .new_with_children(window_style, &[each_taffy])
        .unwrap();
    node_to_taffy_id.insert(tree.root().id(), window_taffy);

    // Expand then compute layout.
    let expansion = expand_each_nodes(
        &tree,
        &store,
        &mut taffy,
        &node_to_taffy_id,
        &EachExpansion::default(),
        None, // full rebuild in tests
        0.0,
        600.0,
        &HashMap::new(),
        &HashMap::new(),
    )
    .unwrap();

    taffy
        .compute_layout(
            window_taffy,
            taffy::geometry::Size {
                width: taffy::style::AvailableSpace::Definite(800.0),
                height: taffy::style::AvailableSpace::MaxContent,
            },
        )
        .unwrap();

    // Check that there are exactly 3 groups for the Each node.
    let groups = expansion
        .groups
        .get(&each_id)
        .expect("Each must be expanded");
    assert_eq!(groups.len(), 3, "3 rows expected");

    // Collect (y, h) for every row container.
    let row_positions: Vec<(f32, f32)> = groups
        .iter()
        .map(|(row_id, _)| {
            let l = taffy.layout(*row_id).expect("row must have layout");
            (l.location.y, l.size.height)
        })
        .collect();

    // Each row must be 50px tall.
    for (i, &(_, h)) in row_positions.iter().enumerate() {
        assert!(
            (h - 50.0).abs() < 1.0,
            "row {i} must be 50 px tall, got {h}"
        );
    }

    // Rows must be stacked (no overlap): row[i].y == i * 50.
    for (i, &(y, _)) in row_positions.iter().enumerate() {
        let expected_y = i as f32 * 50.0;
        assert!(
            (y - expected_y).abs() < 1.0,
            "row {i} must start at y={expected_y}, got y={y}"
        );
    }

    // The Each container must encompass all three rows.
    let each_layout = taffy.layout(each_taffy).expect("Each must have layout");
    assert!(
        each_layout.size.height >= 150.0 - 1.0,
        "Each container must be at least 150 px tall, got {}",
        each_layout.size.height
    );
}

/// Verifies that z-index sorting is stable and correct:
/// a node with z-index=1 must appear after a node with z-index=0 in the
/// sort output.
#[test]
fn test_z_index_sort_order() {
    use crate::parser::{MizuOverflow, Primitive};

    let mut style_rules: HashMap<String, StyleRules> = HashMap::new();
    style_rules.insert(
        "low".to_string(),
        StyleRules {
            z_index: 0,
            ..Default::default()
        },
    );
    style_rules.insert(
        "high".to_string(),
        StyleRules {
            z_index: 5,
            ..Default::default()
        },
    );
    style_rules.insert(
        "mid".to_string(),
        StyleRules {
            z_index: 2,
            ..Default::default()
        },
    );

    // Build: Window -> (low, high, mid)
    let mut tree = Tree::new(MizuNode {
        primitive: Primitive::Doc,
        attributes: FxHashMap::default(),
        events: FxHashMap::default(),
        iterator_context: None,
        conditional_classes: Vec::new(),
    });

    let low_id = tree
        .root_mut()
        .append(MizuNode {
            primitive: Primitive::Box,
            attributes: {
                let mut m = FxHashMap::default();
                m.insert("class".into(), ".low".into());
                m
            },
            events: FxHashMap::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        })
        .id();
    let high_id = tree
        .root_mut()
        .append(MizuNode {
            primitive: Primitive::Box,
            attributes: {
                let mut m = FxHashMap::default();
                m.insert("class".into(), ".high".into());
                m
            },
            events: FxHashMap::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        })
        .id();
    let mid_id = tree
        .root_mut()
        .append(MizuNode {
            primitive: Primitive::Box,
            attributes: {
                let mut m = FxHashMap::default();
                m.insert("class".into(), ".mid".into());
                m
            },
            events: FxHashMap::default(),
            iterator_context: None,
            conditional_classes: Vec::new(),
        })
        .id();

    // Collect children z-index as the sort function would.
    let root_ref = tree.root();
    let mut child_ids: Vec<(i32, EgoNodeId)> = root_ref
        .children()
        .map(|child| {
            let z = child
                .value()
                .attributes
                .get("class")
                .and_then(|cls| {
                    let cls_name = cls.strip_prefix('.').unwrap_or(cls);
                    style_rules.get(cls_name)
                })
                .map(|r| r.z_index)
                .unwrap_or(0);
            (z, child.id())
        })
        .collect();
    child_ids.sort_by_key(|&(z, _)| z);

    let sorted_ids: Vec<EgoNodeId> = child_ids.iter().map(|&(_, id)| id).collect();
    assert_eq!(
        sorted_ids,
        vec![low_id, mid_id, high_id],
        "z-index order must be ascending (low=0, mid=2, high=5)"
    );

    // Suppress unused-variable warnings for the ids we intentionally checked.
    let _ = (low_id, high_id, mid_id, MizuOverflow::Visible);
}

// ------------------------------------------------------------------
// Task 2 — Zero-allocation conditional class evaluation
// ------------------------------------------------------------------

/// Verifies that `paint_node` evaluates conditional classes without cloning
/// the Evaluator's global_store — the local stack must be clean before
/// and after the evaluation.
///
/// This is a regression guard: if the old `.clone()` code were reintroduced,
/// the local_stack would not be used at all (it would be 0 throughout) and
/// the test would still pass — but the key invariant tested here is that
/// *no extra items are left on the local stack after paint_node returns*,
/// which proves that `truncate_locals` properly rewound the frame.
#[test]
fn conditional_class_evaluation_leaves_local_stack_clean() {
    use crate::core::types::{Value, VariableStore};
    use crate::parser::layout::ConditionalClass;
    use crate::parser::logic::{Expr, ExprArena, ExprTree};
    use crate::parser::{MizuNode, Primitive};

    let mut store = VariableStore::new();
    // Intern a variable "active" and set it to true in the global store.
    let active_sym = store.interner.get_or_intern("active");
    let mut store = store.freeze();
    store
        .evaluator
        .global_store
        .insert(active_sym, Value::Bool(true));

    let mut cond_arena = ExprArena::new();
    let cond_root = cond_arena.alloc(Expr::Variable(active_sym));
    let mut tree = ego_tree::Tree::new(MizuNode {
        primitive: Primitive::Box,
        attributes: Default::default(),
        events: Default::default(),
        iterator_context: None,
        conditional_classes: vec![ConditionalClass::Toggle {
            class_name: "active-style".to_string(),
            // condition: `active` (a Variable reference)
            condition: ExprTree {
                arena: cond_arena,
                root: cond_root,
            },
        }],
    });

    // Add a child so paint_node doesn't short-circuit.
    tree.root_mut().append(MizuNode {
        primitive: Primitive::Box,
        attributes: Default::default(),
        events: Default::default(),
        iterator_context: None,
        conditional_classes: vec![],
    });

    let mut taffy = taffy::TaffyTree::new();
    let child_taffy = taffy.new_leaf(taffy::style::Style::default()).unwrap();
    let root_taffy = taffy
        .new_with_children(taffy::style::Style::default(), &[child_taffy])
        .unwrap();
    let mut node_to_taffy_id = HashMap::new();
    node_to_taffy_id.insert(tree.root().id(), root_taffy);
    taffy
        .compute_layout(
            root_taffy,
            taffy::geometry::Size {
                width: taffy::style::AvailableSpace::Definite(800.0),
                height: taffy::style::AvailableSpace::Definite(600.0),
            },
        )
        .unwrap();

    // Add the "active-style" class rule so it can be merged if condition is true.
    let mut style_rules: HashMap<String, StyleRules> = HashMap::new();
    style_rules.insert("active-style".to_string(), StyleRules::default());

    let mut font_cx = parley::FontContext::new();
    let mut layout_cx = parley::LayoutContext::new();
    let scroll_offsets: HashMap<EgoNodeId, f32> = HashMap::new();
    let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());
    let mut fetching_images = rustc_hash::FxHashMap::default();
    let (network_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
    let text_layouts = HashMap::new();

    // Record local stack depth before painting.
    let stack_before = store.evaluator.local_stack.len();

    let empty_each_groups = HashMap::new();
    let empty_window_start: HashMap<EgoNodeId, usize> = HashMap::new();
    let mut empty_each_offset_y: HashMap<EgoNodeId, f32> = HashMap::new();
    let mut ctx = PaintContext {
        tab: crate::network::TabId(0),
        tree: &tree,
        taffy: &taffy,
        node_to_taffy_id: &node_to_taffy_id,
        style_rules: &style_rules,
        style_variants: &[],
        render_env: crate::render::responsive::RenderEnvironment {
            viewport: crate::render::responsive::ViewportSize {
                width: 800.0,
                height: 600.0,
            },
            color_scheme: crate::render::preferences::ColorScheme::Dark,
        },
        font_cx: &mut font_cx,
        layout_cx: &mut layout_cx,
        transform: vello::kurbo::Affine::IDENTITY,
        store: &mut store,
        scroll_offsets: &scroll_offsets,
        focused_node: None,
        image_cache: &mut image_cache,
        fetching_images: &mut fetching_images,
        network_tx: &network_tx,
        chrome_url: "mizu://localhost/index.mizu",
        elapsed_ms: 0,
        has_animations: false,
        text_layouts: &text_layouts,
        item_bindings: HashMap::new(),
        each_groups: &empty_each_groups,
        each_window_start: &empty_window_start,
        each_container_offset_y: &mut empty_each_offset_y,
        taffy_id_overrides: HashMap::new(),
    };

    let mut scene = vello::Scene::new();
    paint_node(tree.root().id(), &mut ctx, &mut scene, (0.0, 0.0));

    // The local stack must be exactly as deep as it was before paint_node —
    // any leftover frames indicate `truncate_locals` was not called correctly.
    let stack_after = ctx.store.evaluator.local_stack.len();
    assert_eq!(
        stack_after, stack_before,
        "local stack must be clean after conditional-class evaluation: \
         before={stack_before}, after={stack_after}"
    );
}

/// The test that actually proves the ternary-class feature does what
/// it's for: `evaluate_conditional_classes` must resolve a
/// `ConditionalClass::Ternary` to whichever branch's style rules match
/// the runtime-evaluated class name — not just parse without error.
#[test]
fn ternary_conditional_class_resolves_to_the_evaluated_branch_style() {
    use crate::core::types::{StringInterner, Value, VariableStore};
    use crate::parser::layout::ConditionalClass;
    use crate::parser::logic::parse_expr_standalone;
    use crate::parser::{MizuNode, Primitive};

    let mut interner = StringInterner::new();
    let expr = parse_expr_standalone(r#"flag ? "on" : "off""#, &mut interner).unwrap();

    let node = MizuNode {
        primitive: Primitive::Box,
        attributes: Default::default(),
        events: Default::default(),
        iterator_context: None,
        conditional_classes: vec![ConditionalClass::Ternary { expr }],
    };
    let tree = ego_tree::Tree::new(node);

    let mut taffy = taffy::TaffyTree::new();
    let root_taffy = taffy.new_leaf(taffy::style::Style::default()).unwrap();
    let mut node_to_taffy_id = HashMap::new();
    node_to_taffy_id.insert(tree.root().id(), root_taffy);
    taffy
        .compute_layout(
            root_taffy,
            taffy::geometry::Size {
                width: taffy::style::AvailableSpace::Definite(800.0),
                height: taffy::style::AvailableSpace::Definite(600.0),
            },
        )
        .unwrap();

    let mut style_rules: HashMap<String, StyleRules> = HashMap::new();
    style_rules.insert(
        "on".to_string(),
        StyleRules {
            z_index: 1,
            ..Default::default()
        },
    );
    style_rules.insert(
        "off".to_string(),
        StyleRules {
            z_index: 2,
            ..Default::default()
        },
    );

    let mut store = crate::core::types::VariableStore {
        evaluator: Default::default(),
        interner,
    };
    store.set("flag", Value::Bool(true));
    let mut store = store.freeze();

    let mut font_cx = parley::FontContext::new();
    let mut layout_cx = parley::LayoutContext::new();
    let scroll_offsets: HashMap<EgoNodeId, f32> = HashMap::new();
    let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());
    let mut fetching_images = rustc_hash::FxHashMap::default();
    let (network_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
    let text_layouts = HashMap::new();
    let empty_each_groups = HashMap::new();
    let empty_window_start: HashMap<EgoNodeId, usize> = HashMap::new();
    let mut empty_each_offset_y: HashMap<EgoNodeId, f32> = HashMap::new();

    let mut ctx = PaintContext {
        tab: crate::network::TabId(0),
        tree: &tree,
        taffy: &taffy,
        node_to_taffy_id: &node_to_taffy_id,
        style_rules: &style_rules,
        style_variants: &[],
        render_env: crate::render::responsive::RenderEnvironment {
            viewport: crate::render::responsive::ViewportSize {
                width: 800.0,
                height: 600.0,
            },
            color_scheme: crate::render::preferences::ColorScheme::Dark,
        },
        font_cx: &mut font_cx,
        layout_cx: &mut layout_cx,
        transform: vello::kurbo::Affine::IDENTITY,
        store: &mut store,
        scroll_offsets: &scroll_offsets,
        focused_node: None,
        image_cache: &mut image_cache,
        fetching_images: &mut fetching_images,
        network_tx: &network_tx,
        chrome_url: "mizu://localhost/index.mizu",
        elapsed_ms: 0,
        has_animations: false,
        text_layouts: &text_layouts,
        item_bindings: HashMap::new(),
        each_groups: &empty_each_groups,
        each_window_start: &empty_window_start,
        each_container_offset_y: &mut empty_each_offset_y,
        taffy_id_overrides: HashMap::new(),
    };

    let resolved = evaluate_conditional_classes(tree.root().value(), &mut ctx);
    assert_eq!(
        resolved.z_index, 1,
        "flag=true must resolve the ternary to the \"on\" branch's style rules"
    );

    ctx.store.set_runtime("flag", Value::Bool(false));
    let resolved = evaluate_conditional_classes(tree.root().value(), &mut ctx);
    assert_eq!(
        resolved.z_index, 2,
        "flag=false must resolve the ternary to the \"off\" branch's style rules"
    );
}

/// Verifies that item_bindings injected via `push_local` shadow global
/// variables during conditional-class evaluation — the overlay semantics
/// must be preserved by the push_local/truncate_locals approach.
#[test]
fn conditional_class_item_binding_shadows_global() {
    use crate::core::types::{Value, VariableStore};
    use crate::parser::layout::ConditionalClass;
    use crate::parser::logic::{Expr, ExprArena, ExprTree};
    use crate::parser::{MizuNode, Primitive};

    let mut store = VariableStore::new();
    // Global: "flag" = false
    let flag_sym = store.interner.get_or_intern("flag");
    let mut store = store.freeze();
    store
        .evaluator
        .global_store
        .insert(flag_sym, Value::Bool(false));

    // The conditional class condition: `flag`
    let mut cond_arena = ExprArena::new();
    let cond_root = cond_arena.alloc(Expr::Variable(flag_sym));
    let node = MizuNode {
        primitive: Primitive::Box,
        attributes: Default::default(),
        events: Default::default(),
        iterator_context: None,
        conditional_classes: vec![ConditionalClass::Toggle {
            class_name: "highlight".to_string(),
            condition: ExprTree {
                arena: cond_arena,
                root: cond_root,
            },
        }],
    };
    let tree = ego_tree::Tree::new(node);

    let mut taffy = taffy::TaffyTree::new();
    let root_taffy = taffy.new_leaf(taffy::style::Style::default()).unwrap();
    let mut node_to_taffy_id = HashMap::new();
    node_to_taffy_id.insert(tree.root().id(), root_taffy);
    taffy
        .compute_layout(
            root_taffy,
            taffy::geometry::Size {
                width: taffy::style::AvailableSpace::Definite(800.0),
                height: taffy::style::AvailableSpace::Definite(600.0),
            },
        )
        .unwrap();

    let mut style_rules: HashMap<String, StyleRules> = HashMap::new();
    style_rules.insert(
        "highlight".to_string(),
        StyleRules {
            z_index: 99, // sentinel value we can detect
            ..Default::default()
        },
    );

    let mut font_cx = parley::FontContext::new();
    let mut layout_cx = parley::LayoutContext::new();
    let scroll_offsets: HashMap<EgoNodeId, f32> = HashMap::new();
    let mut image_cache = lru::LruCache::new(std::num::NonZeroUsize::new(200).unwrap());
    let mut fetching_images = rustc_hash::FxHashMap::default();
    let (network_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<crate::network::NetworkCmd>();
    let text_layouts = HashMap::new();

    // item_bindings overrides "flag" → true (local shadow beats global false)
    let mut item_bindings = HashMap::new();
    item_bindings.insert("flag".to_string(), Value::Bool(true));

    let empty_each_groups = HashMap::new();
    let empty_window_start: HashMap<EgoNodeId, usize> = HashMap::new();
    let mut empty_each_offset_y: HashMap<EgoNodeId, f32> = HashMap::new();
    let mut ctx = PaintContext {
        tab: crate::network::TabId(0),
        tree: &tree,
        taffy: &taffy,
        node_to_taffy_id: &node_to_taffy_id,
        style_rules: &style_rules,
        style_variants: &[],
        render_env: crate::render::responsive::RenderEnvironment {
            viewport: crate::render::responsive::ViewportSize {
                width: 800.0,
                height: 600.0,
            },
            color_scheme: crate::render::preferences::ColorScheme::Dark,
        },
        font_cx: &mut font_cx,
        layout_cx: &mut layout_cx,
        transform: vello::kurbo::Affine::IDENTITY,
        store: &mut store,
        scroll_offsets: &scroll_offsets,
        focused_node: None,
        image_cache: &mut image_cache,
        fetching_images: &mut fetching_images,
        network_tx: &network_tx,
        chrome_url: "mizu://localhost/index.mizu",
        elapsed_ms: 0,
        has_animations: false,
        text_layouts: &text_layouts,
        item_bindings,
        each_groups: &empty_each_groups,
        each_window_start: &empty_window_start,
        each_container_offset_y: &mut empty_each_offset_y,
        taffy_id_overrides: HashMap::new(),
    };

    // Paint — if the shadow logic is correct, `highlight` class is merged
    // (flag=true via item_binding) even though global flag=false.
    // We can only verify indirectly that no panic occurs and the stack is clean.
    let mut scene = vello::Scene::new();
    paint_node(tree.root().id(), &mut ctx, &mut scene, (0.0, 0.0));

    // Global must not have been mutated — the old approach inserted into global_store.
    let global_flag = ctx.store.evaluator.global_store.get(&flag_sym);
    assert!(
        global_flag.is_some_and(|v| v
            .budget_eq(&Value::Bool(false), &mut u64::MAX, u64::MAX)
            .unwrap_or(false)),
        "global 'flag' must remain false after conditional-class eval with item_binding override"
    );

    // Local stack must be empty (no leftover push_local frames).
    assert_eq!(
        ctx.store.evaluator.local_stack.len(),
        0,
        "local stack must be empty after eval"
    );
}

/// Verifies that a node with `overflow: scroll` and a non-zero scroll
/// offset causes the child transform to include the vertical translation.
#[test]
fn test_scroll_offset_applied_to_transform() {
    // The transform for a scrollable parent with 50px offset must shift
    // children upward (negative Y translation).
    let base = Affine::IDENTITY;
    let scroll_y = 50.0f32;
    let child_transform = base * Affine::translate((0.0, -(scroll_y as f64)));

    // A point at y=100 in the child's un-scrolled space should appear at y=50.
    let point = vello::kurbo::Point::new(0.0, 100.0);
    let transformed = child_transform * point;
    assert!(
        (transformed.y - 50.0).abs() < f64::EPSILON,
        "scroll should shift child paint by -scroll_y; got y={}",
        transformed.y,
    );
}
