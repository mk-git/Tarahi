//! SVG rendering backend (the first `Canvas` implementation).
//!
//! Coordinate mapping (Analysis doc 06): VSDX stores inches with origin
//! bottom-left and Y up; SVG is Y down. Each shape becomes a `<g>` with the
//! XForm transform
//!   translate(PinX, PageH-PinY) rotate(-Angle) scale(FlipX,FlipY)
//!   translate(-LocPinX, +LocPinY)
//! applied to geometry whose local Y is negated at path-build time.

use std::collections::HashMap;
use std::fmt::Write as _;

use vsdx::model::Sheet;
#[cfg(test)]
use vsdx::model::{Section, SheetChild};
use vsdx::{Cell, Document, Row, Shape};

const PX_PER_IN: f64 = 96.0;

/// Text renders as glyphs only while its on-screen em height clears
/// this many pixels; below it the canvas layer path emits gray greek
/// bars instead (GitHub #124 item 2 — Visio greeks text at low zoom,
/// and the bars skip the font resolve / glyph-outline work the usvg
/// parse would otherwise pay for text nobody can read). Export, print,
/// and stencil thumbnails never greek: their calls pass no ppi.
pub const TEXT_GREEK_PX: f64 = 4.0;

/// Count of the recorded text sizes (inches, one entry per text block
/// a layer serialization met) that would greek at `ppi`. The canvas
/// layer cache compares this across ppi-bucket changes: equal counts
/// mean the same blocks greeked on both sides (greeked-ness is
/// monotone in size, so the greeked set is always a prefix), i.e. the
/// retained SVG/tree serializes identically and the re-render can
/// skip the re-parse (#124 item 2 + item 6.3).
pub fn greeked_count(sizes: &[f64], ppi: f32) -> usize {
    sizes
        .iter()
        .filter(|&&s| s * (ppi as f64) < TEXT_GREEK_PX)
        .count()
}

/// Greeking context for canvas-layer serializations (#124 item 2).
/// `Some` only on the `render_shape_layer_opts` path; the ppi decides
/// per text block whether it emits glyphs or bars, and every text
/// size met is recorded so the layer cache can build its re-serialize
/// profile (see [`greeked_count`]).
pub(crate) struct GreekCtx {
    ppi: f32,
    sizes: std::cell::RefCell<Vec<f64>>,
}

impl GreekCtx {
    fn new(ppi: f32) -> Self {
        Self {
            ppi,
            sizes: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn greeks(&self, size_in: f64) -> bool {
        size_in * (self.ppi as f64) < TEXT_GREEK_PX
    }
}

/// Contact sheet of every master's shape tree, laid out on a grid — the
/// rendering path for stencils (`.vssx`), whose pages are empty.
pub fn render_masters(doc: &Document) -> anyhow::Result<String> {
    let cell = 1.2f64; // cell size (inches)
    let cols = 6usize;
    let mut masters: Vec<&vsdx::Master> = doc.masters.iter().collect();
    masters.sort_by_key(|m| m.id);
    let rows = masters.len().div_ceil(cols).max(1);
    let w = cell * cols as f64;
    let h = cell * rows as f64;

    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{pw:.0}" height="{ph:.0}">"#,
        pw = w * PX_PER_IN,
        ph = h * PX_PER_IN,
    );
    let _ = writeln!(
        svg,
        r#"<!-- Tarahi master contact sheet ({} masters) -->"#,
        masters.len()
    );
    let _ = writeln!(
        svg,
        r##"<rect x="0" y="0" width="{w}" height="{h}" fill="#ffffff"/>"##,
    );
    for (i, m) in masters.iter().enumerate() {
        let (cx, cy) = ((i % cols) as f64, (i / cols) as f64);
        let _ = writeln!(
            svg,
            r#"<g transform="translate({x},{y})">"#,
            x = cx * cell,
            y = cy * cell,
        );
        let _ = writeln!(
            svg,
            r##"<rect x="0" y="0" width="{c}" height="{c}" fill="none" stroke="#e0e0e0" stroke-width="0.01"/>"##,
            c = cell
        );
        // Center the master's real shape bounds in the cell (masters sit
        // centered on master pages several times the shape size, so the
        // pin position cannot be ignored — see `master_bbox`).
        let size = |n: &str| {
            m.shapes
                .first()
                .and_then(|s| s.sheet.cell(n))
                .and_then(|c| c.numeric())
                .unwrap_or(1.0)
        };
        let (mw, mh) = (size("Width"), size("Height"));
        let master_h = m
            .sheet()
            .and_then(|s| s.cell("PageHeight"))
            .and_then(|c| c.numeric())
            .unwrap_or(mh);
        let bounds = master_bbox(doc, m, master_h).unwrap_or((0.0, 0.0, mw, mh));
        let (tx, ty, k) = fit_transform(cell, 0.7, bounds);
        let _ = writeln!(
            svg,
            r#"<g transform="translate({tx:.5},{ty:.5}) scale({k:.5},{k:.5})">"#,
        );
        let fields = crate::fields::FieldsCtx::for_master(doc, &m.shapes);
        for shape in &m.shapes {
            render_shape(
                doc,
                shape,
                master_h,
                &[],
                &m.part,
                None,
                None,
                None,
                Some(&fields),
                // Masters render on contact sheets — the jump overlay
                // covers page renders only (#294 review), and a master
                // carries no page to resolve a radius from: stored-jump
                // evidence is unavailable, so nothing strips.
                (0.0, 0.0),
                &mut svg,
            );
        }
        let _ = writeln!(svg, "</g>");
        let _ = writeln!(
            svg,
            r##"<text x="{x:.4}" y="{y:.4}" font-size="0.07" text-anchor="middle" fill="#333">{name}</text>"##,
            name = escape_text(m.name_u.as_deref().unwrap_or("-")),
            x = cell / 2.0,
            y = cell - 0.06,
        );
        let _ = writeln!(svg, "</g>");
    }
    let _ = writeln!(svg, "</svg>");
    Ok(svg)
}

/// Union bounding box (unrotated) of a master's root shapes, in the
/// master page's y-down render coordinates — the region a thumbnail
/// must show. Masters typically center their shape on a master page
/// several times the shape size, so the pin position cannot be ignored.
fn master_bbox(doc: &Document, m: &vsdx::Master, master_h: f64) -> Option<(f64, f64, f64, f64)> {
    let mut acc: Option<(f64, f64, f64, f64)> = None;
    for s in &m.shapes {
        let chain: Vec<&Shape> = std::iter::once(s).collect();
        let env = shape_env(doc, s, &chain);
        let get = |n: &str| -> Option<f64> {
            doc.resolve_cell(s, &chain, n)
                .and_then(|c| cell_num(&c, &env))
        };
        let (pin_x, pin_y) = (get("PinX")?, get("PinY")?);
        let (w, h) = (get("Width").unwrap_or(1.0), get("Height").unwrap_or(1.0));
        let (lx, ly) = (
            get("LocPinX").unwrap_or(w / 2.0),
            get("LocPinY").unwrap_or(h / 2.0),
        );
        // Same placement math as `render_shape`: the box spans
        // (pin − loc) in x and (page_h − pin + loc − h .. + h) in y.
        let x0 = pin_x - lx;
        let y0 = (master_h - pin_y) + ly - h;
        let rect = (x0, y0, w, h);
        acc = Some(match acc {
            None => rect,
            Some((ax, ay, aw, ah)) => {
                let x1 = (ax + aw).max(rect.0 + rect.2);
                let y1 = (ay + ah).max(rect.1 + rect.3);
                let nx = ax.min(rect.0);
                let ny = ay.min(rect.1);
                (nx, ny, x1 - nx, y1 - ny)
            }
        });
    }
    acc
}

/// Fit `(bx, by, bw, bh)` centered into a `cell`-sized square with
/// `pad` fraction of usable size: the SVG group transform.
fn fit_transform(cell: f64, pad: f64, (bx, by, bw, bh): (f64, f64, f64, f64)) -> (f64, f64, f64) {
    let k = (cell * pad / bw.abs().max(1e-6)).min(cell * pad / bh.abs().max(1e-6));
    let tx = cell / 2.0 - k * (bx + bw / 2.0);
    let ty = cell / 2.0 - k * (by + bh / 2.0);
    (tx, ty, k)
}

/// One master rendered as a standalone SVG (stencil thumbnail): the
/// master's shape tree centered and scaled into a square viewBox.
pub fn render_master(doc: &Document, m: &vsdx::Master) -> anyhow::Result<String> {
    let cell = 1.0f64;
    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {c} {c}" width="{pw:.0}" height="{ph:.0}">"#,
        c = cell,
        pw = cell * PX_PER_IN,
        ph = cell * PX_PER_IN,
    );
    let _ = writeln!(
        svg,
        r##"<rect x="0" y="0" width="{c}" height="{c}" fill="#ffffff"/>"##,
        c = cell
    );
    let size = |n: &str| {
        m.shapes
            .first()
            .and_then(|s| s.sheet.cell(n))
            .and_then(|c| c.numeric())
            .unwrap_or(1.0)
    };
    let (mw, mh) = (size("Width"), size("Height"));
    let master_h = m
        .sheet()
        .and_then(|s| s.cell("PageHeight"))
        .and_then(|c| c.numeric())
        .unwrap_or(mh);
    // Center the master's real shape bounds (not the master page) in
    // the cell; the shape is placed on the master page by `render_shape`
    // and masters sit centered on pages several times their size.
    let bounds = master_bbox(doc, m, master_h).unwrap_or((0.0, 0.0, mw, mh));
    let (tx, ty, k) = fit_transform(cell, 0.78, bounds);
    let _ = writeln!(
        svg,
        r#"<g transform="translate({tx:.5},{ty:.5}) scale({k:.5},{k:.5})">"#,
    );
    let fields = crate::fields::FieldsCtx::for_master(doc, &m.shapes);
    for shape in &m.shapes {
        render_shape(
            doc,
            shape,
            master_h,
            &[],
            &m.part,
            None,
            None,
            None,
            Some(&fields),
            // Masters render on contact sheets — the jump overlay
            // covers page renders only (#294 review), and a master
            // carries no page to resolve a radius from: stored-jump
            // evidence is unavailable, so nothing strips.
            (0.0, 0.0),
            &mut svg,
        );
    }
    let _ = writeln!(svg, "</g>");
    let _ = writeln!(svg, "</svg>");
    Ok(svg)
}

pub fn render_page(doc: &Document, page: &vsdx::Page) -> anyhow::Result<String> {
    render_page_with(doc, page, &[])
}

/// The export render of one page (#35): page chrome dropped (no gray
/// sheet border — Visio's own exports carry none), white background
/// per `transparent`. The ink is identical to [`render_page`] — only
/// the chrome differs.
pub fn render_page_export(
    doc: &Document,
    page: &vsdx::Page,
    transparent: bool,
) -> anyhow::Result<String> {
    render_page_full(doc, page, &[], None, None, false, !transparent)
}

/// [`render_page`] with hidden layer members: shapes whose
/// `LayerMember` names one of `hidden` (Visio layer row indexes, the
/// LayerMember cell convention) are skipped entirely — the annotation
/// toggle (#29) and friends.
pub fn render_page_with(
    doc: &Document,
    page: &vsdx::Page,
    hidden: &[i64],
) -> anyhow::Result<String> {
    render_page_opts(doc, page, hidden, None, None)
}

/// [`render_page`] with one shape's TEXT suppressed (GitHub #22): the
/// in-place canvas editor covers the text block, so the page raster
/// underneath must not also show the shape's old text (ghost text
/// under the caret). The shape itself — geometry, fill, borders —
/// keeps rendering; only its own `emit_text` pass is skipped.
pub fn render_page_no_text(
    doc: &Document,
    page: &vsdx::Page,
    hidden: &[i64],
    skip_text_shape: i64,
) -> anyhow::Result<String> {
    render_page_opts(doc, page, hidden, None, Some(skip_text_shape))
}

/// Union sink for the ink-bounds walk with the #219 focus gate:
/// nodes contribute only once the walk is inside the focus subtree
/// (always, when unfocused). `enter` arms the gate at the focus node;
/// the walk saves/restores `inside` around each child so a later
/// sibling of the focus never leaks into the region.
struct Focus {
    acc: Option<(f64, f64, f64, f64)>,
    focus: Option<i64>,
    inside: bool,
}

impl Focus {
    fn enter(&mut self, id: i64) {
        if Some(id) == self.focus {
            self.inside = true;
        }
    }

    fn union(&mut self, r: (f64, f64, f64, f64)) {
        if !self.inside {
            return;
        }
        self.acc = Some(match self.acc {
            None => r,
            Some((ax, ay, aw, ah)) => {
                let x1 = (ax + aw).max(r.0 + r.2);
                let y1 = (ay + ah).max(r.1 + r.3);
                let nx = ax.min(r.0);
                let ny = ay.min(r.1);
                (nx, ny, x1 - nx, y1 - ny)
            }
        });
    }
}

/// The ink bounds of one top-level shape's subtree, in page
/// coordinates (inches, y-down like the SVG viewBox): the union of
/// every node's transformed [0..w]×[-h..0] box and text box, padded to
/// absorb stroke bleed, clamped to the page ± 2 in. This is the
/// viewBox (and the on-canvas quad) of a per-shape layer (#93).
pub fn shape_ink_bounds(
    doc: &Document,
    page: &vsdx::Page,
    shape_id: i64,
) -> Option<(f64, f64, f64, f64)> {
    shape_ink_bounds_focused(doc, page, shape_id, None)
}

/// [`shape_ink_bounds`] restricted to one DESCENDANT's own subtree
/// (#219): `focus` names a sub-shape inside `shape_id`'s subtree and
/// only that sub-shape's subtree is unioned — the damage region a
/// localized edit re-rasters. The cell-resolution chain still runs
/// from the top-level root, so inherited/master cells resolve exactly
/// as the renderer resolves them. `None` is the whole-subtree bounds.
pub fn shape_ink_bounds_focused(
    doc: &Document,
    page: &vsdx::Page,
    shape_id: i64,
    focus: Option<i64>,
) -> Option<(f64, f64, f64, f64)> {
    let page_h = num(page.sheet().and_then(|s| s.cell("PageHeight"))).unwrap_or(11.0);
    let root = page.shapes.iter().find(|s| s.id == shape_id)?;
    let mut u = Focus {
        acc: None,
        focus,
        inside: focus.is_none(),
    };
    fn walk(
        doc: &Document,
        s: &Shape,
        chain: &[&Shape],
        page_h: f64,
        top: bool,
        to_page: Affine,
        u: &mut Focus,
    ) {
        let mut chain: Vec<&Shape> = chain.to_vec();
        chain.push(s);
        let (node_box, a) = node_ink_box(doc, s, &chain, page_h, top, to_page);
        u.union(node_box);
        for sub in &s.sub_shapes {
            let saved = u.inside;
            u.enter(sub.id);
            walk(doc, sub, &chain, page_h, false, a, u);
            u.inside = saved;
        }
    }
    u.enter(root.id);
    walk(doc, root, &[], page_h, true, Affine::IDENTITY, &mut u);
    let (x, y, w, h) = u.acc?;
    // Stroke bleed margin; then clamp to the page ± 2 in so a rogue
    // formula cannot push a layer off to infinity.
    let pad = 0.08;
    let (x, y, w, h) = (x - pad, y - pad, w + 2.0 * pad, h + 2.0 * pad);
    let pw = num(page.sheet().and_then(|s| s.cell("PageWidth"))).unwrap_or(8.5);
    let (min_x, min_y) = (-2.0, -2.0);
    let (max_x, max_y) = (pw + 2.0, page_h + 2.0);
    let x = x.max(min_x);
    let y = y.max(min_y);
    let w = w.min(max_x - x);
    let h = h.min(max_y - y);
    Some((x, y, w.max(0.01), h.max(0.01)))
}

/// One node's own ink box in page space (inches, y-down), with the
/// affine that maps its local frame there: the per-node half of the
/// ink-bounds walk ([`shape_ink_bounds_focused`]) and of the #236
/// cull's subtree-box pass. `chain` includes `s`. The box unions the
/// transformed [0..w]×[-h..0] XForm box, every geometry row's real
/// extent (#202 — curve hulls and arc apexes), and the text block's
/// placement box; no padding, no clamping (the callers decide).
/// Mirrored master content maps into the instance box, so this box
/// bounds it too.
fn node_ink_box(
    doc: &Document,
    s: &Shape,
    chain: &[&Shape],
    page_h: f64,
    top: bool,
    to_page: Affine,
) -> ((f64, f64, f64, f64), Affine) {
    let env = shape_env(doc, s, chain);
    // Takes the env per call (not by capture) so `env` can move into
    // the shared frame env below (#218 review).
    let get = |n: &str, env: &vsdx::formula::Env| -> Option<f64> {
        doc.resolve_cell(s, chain, n)
            .and_then(|c| cell_num(&c, env))
    };
    let width = get("Width", &env).unwrap_or(1.0);
    let height = get("Height", &env).unwrap_or(1.0);
    // The exact transform string render_shape/render_shape_local
    // emit for this node, composed into one affine.
    let local = if top {
        let (sx, sy) = (
            if get("FlipX", &env).unwrap_or(0.0) != 0.0 {
                -1.0
            } else {
                1.0
            },
            if get("FlipY", &env).unwrap_or(0.0) != 0.0 {
                -1.0
            } else {
                1.0
            },
        );
        Affine::translate(
            get("PinX", &env).unwrap_or(0.0),
            page_h - get("PinY", &env).unwrap_or(0.0),
        )
        .then(&Affine::rotate_deg(
            -get("Angle", &env).unwrap_or(0.0).to_degrees(),
        ))
        .then(&Affine::scale(sx, sy))
        .then(&Affine::translate(
            -get("LocPinX", &env).unwrap_or(width / 2.0),
            get("LocPinY", &env).unwrap_or(height / 2.0),
        ))
    } else {
        Affine::translate(
            get("PinX", &env).unwrap_or(0.0),
            -get("PinY", &env).unwrap_or(0.0),
        )
        .then(&Affine::rotate_deg(
            -get("Angle", &env).unwrap_or(0.0).to_degrees(),
        ))
        .then(&Affine::translate(
            -get("LocPinX", &env).unwrap_or(width / 2.0),
            get("LocPinY", &env).unwrap_or(height / 2.0),
        ))
    };
    let a = to_page.then(&local);
    let mut u = Focus {
        acc: None,
        focus: None,
        inside: true,
    };
    // The node's own ink box (local y-down) — mirrored master
    // content maps into the instance box, so this bounds it too.
    let box_union = |x0: f64, y0: f64, x1: f64, y1: f64, a: &Affine, u: &mut Focus| {
        let c = [
            a.apply(x0, y0),
            a.apply(x1, y0),
            a.apply(x1, y1),
            a.apply(x0, y1),
        ];
        let xs = [c[0].0, c[1].0, c[2].0, c[3].0];
        let ys = [c[0].1, c[1].1, c[2].1, c[3].1];
        u.union((
            xs.iter().cloned().fold(f64::INFINITY, f64::min),
            ys.iter().cloned().fold(f64::INFINITY, f64::min),
            xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                - xs.iter().cloned().fold(f64::INFINITY, f64::min),
            ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                - ys.iter().cloned().fold(f64::INFINITY, f64::min),
        ));
    };
    box_union(0.0, -height, width, 0.0, &a, &mut u);
    // #202: the geometry rows' REAL extent — a curved connector's
    // arc belly leaves the begin-corner box (a 1D connector's
    // Height is 0), and a layer viewBox built from the box alone
    // clips the belly into a straight line (every arc-pair
    // connector in utilisation.vsdx rendered flat). Union every
    // row's endpoint and control points: every path this module
    // emits is a line/quad/cubic whose curve stays inside the
    // hull of those points, plus the elliptical arc's apex
    // (2·on-curve − chord mid) as the conservative bound.
    // What the renderer draws: the instance's own Geometry when it
    // carries any, else the MASTER source's rows scaled into the
    // instance frame (render_all_geometry's master path) — a fresh
    // stencil-drop instance renders entirely through it, and its
    // arrowheads and arc bellies must bound the ink too or the
    // layer viewBox clips them and the #236 cull skips them
    // (#218 review round 3).
    let own = s.sheet.sections_named("Geometry");
    // ONE env per frame, shared across every walked section (#218
    // review): the per-entry clones discarded row-coordinate
    // writebacks, so a Geometry2 referencing `Geometry1.X2` fell
    // back to the authored pose (#249's cross-row case, across
    // sections). A shape walks either its OWN sections (instance
    // env) or its master's (the prepared menv) — never both.
    let mut walk: Vec<(&vsdx::Section, (f64, f64))> =
        own.iter().map(|g| (*g, (1.0, 1.0))).collect();
    // Starts as the instance env; the master branch below REPLACES it.
    // The text block after the walk still reads the INSTANCE env's
    // Txt* cells (they are the shape's own), so a copy survives the move.
    let text_env = env.clone();
    let mut frame_env = env;
    if own.is_empty() {
        if let Some(src) = doc.instance_source(s, chain) {
            let mw = src
                .sheet
                .cell("Width")
                .and_then(|c| c.numeric())
                .filter(|v| v.abs() > 1e-9);
            let mh = src
                .sheet
                .cell("Height")
                .and_then(|c| c.numeric())
                .filter(|v| v.abs() > 1e-9);
            let scale = (
                mw.map_or(1.0, |mw| width / mw),
                mh.map_or(1.0, |mh| height / mh),
            );
            // The master env builds ONCE with the instance's GLUED
            // Begin/End and the Scratch/Controls formulas
            // RE-EVALUATED against them — the cached V is the
            // authored pose, and the band masters derive their whole
            // vertical span from them (`Scratch.Y2 =
            // ABS(BeginY-EndY)`, #249).
            let mut menv = shape_env(doc, src, &[]);
            for n in ["BeginX", "BeginY", "EndX", "EndY"] {
                if let Some(v) = doc.resolve_cell(s, chain, n).and_then(|c| c.numeric()) {
                    menv.insert(n.to_string(), v);
                }
            }
            for sec_name in ["Scratch", "Controls"] {
                for sec in src.sheet.sections_named(sec_name) {
                    for row in &sec.rows {
                        for (name, cell) in &row.cells {
                            let key = match sec_name {
                                "Scratch" => format!("Scratch.{}{}", name, row.ix + 1),
                                _ => {
                                    let rn =
                                        row.n.clone().unwrap_or_else(|| format!("Row_{}", row.ix));
                                    format!("Controls.{}.{}", rn, name)
                                }
                            };
                            let v = cell
                                .f
                                .as_deref()
                                .filter(|f| *f != "Inh" && *f != "No Formula")
                                .and_then(|f| vsdx::formula::eval(f, &menv).ok())
                                .and_then(|v| v.as_f64())
                                .filter(|v| v.is_finite())
                                .or_else(|| cell.numeric());
                            if let Some(v) = v {
                                menv.insert(key, v);
                            }
                        }
                    }
                }
            }
            walk.extend(
                src.sheet
                    .sections_named("Geometry")
                    .into_iter()
                    .map(|g| (g, scale)),
            );
            frame_env = menv;
        }
    }
    // An instance PolylineTo row without its own POLYLINE() text draws
    // its master row's vertices (render_all_geometry: section by index,
    // row by IX, else by its occurrence among the PolylineTo rows), so
    // the bounds walk them too (Greptile on PR #329). Absolute axes
    // scale from master into instance units; fractional ones stay.
    let walks_own = !own.is_empty();
    let master_polyline = |walk_ix: usize, ix: i64, occ: usize| -> Option<String> {
        if !walks_own {
            return None;
        }
        let src = doc.instance_source(s, chain)?;
        let sec = *src.sheet.sections_named("Geometry").get(walk_ix)?;
        let is_poly = |r: &&vsdx::Row| r.t.as_deref() == Some("PolylineTo");
        let mrow = match sec.rows.iter().find(|r| ix > 0 && r.ix == ix) {
            Some(r) => Some(r).filter(is_poly)?,
            None => sec.rows.iter().filter(is_poly).nth(occ)?,
        };
        let text = mrow
            .cell("A")
            .and_then(|c| c.v.clone().or_else(|| c.f.clone()))?;
        let (xt, yt, pts) = crate::curve::parse_polyline(&text)?;
        let ratio = |cell: &str, dim: f64| {
            src.sheet
                .cell(cell)
                .and_then(|c| c.numeric())
                .filter(|v| v.abs() > 1e-9)
                .map_or(1.0, |m| dim / m)
        };
        let (sx, sy) = (ratio("Width", width), ratio("Height", height));
        let mut out = format!("POLYLINE({xt}, {yt}");
        for (px, py) in pts {
            let px = if xt == 0.0 { px } else { px * sx };
            let py = if yt == 0.0 { py } else { py * sy };
            let _ = write!(out, ", {px},{py}");
        }
        out.push(')');
        Some(out)
    };
    for (walk_ix, (geo, gscale)) in walk.into_iter().enumerate() {
        // Shared: writebacks persist for later sections of the frame.
        let genv = &mut frame_env;
        let gname = format!("Geometry{}", walk_ix + 1);
        let mut cur = (0.0f64, 0.0f64);
        let mut first_pt: Option<(f64, f64)> = None;
        let mut poly_occ = 0usize;
        fn row_num_in(env: &vsdx::formula::Env, row: &vsdx::Row, n: &str) -> f64 {
            row.cell(n)
                .and_then(|c| {
                    c.f.as_deref()
                        .and_then(|f| vsdx::formula::eval(f, env).ok())
                        .and_then(|v| v.as_f64())
                        .filter(|v| v.is_finite())
                        .or(c.numeric())
                })
                .unwrap_or(0.0)
        }
        for row in &geo.rows {
            let Some(t) = row.t.as_deref() else { continue };
            if row.is_deleted() {
                continue; // deleted rows stay deleted (#293)
            }
            let rel = t.starts_with("Rel");
            let (mut x, mut y) = (row_num_in(genv, row, "X"), row_num_in(genv, row, "Y"));
            if rel {
                // Raw signed dims — rel_adjust (which the renderer
                // uses) multiplies raw, so the bound must match.
                x *= width;
                y *= height;
            }
            // Feed the section-local coordinates back for cross-row
            // references (`Geometry1.X2`, #249) — before the scale,
            // which maps onto the page but not into the next row's
            // frame.
            genv.insert(format!("{}.X{}", gname, row.ix), x);
            genv.insert(format!("{}.Y{}", gname, row.ix), y);
            // Master-frame rows scale into the instance like the
            // renderer's emit (every point, literals and evaluated
            // formulas alike).
            x *= gscale.0;
            y *= gscale.1;
            // The affine maps local y-DOWN ([0..w]×[-h..0]) like the
            // box union above; the row coordinates are y-UP.
            let pt = |x: f64, y: f64, u: &mut Focus| {
                let (px, py) = a.apply(x, -y);
                u.union((px, py, 0.0, 0.0));
            };
            pt(x, y, &mut u);
            match t {
                "ArcTo" | "RelArcTo" => {
                    // The apex: chord mid + A·right-of-travel.
                    let sag = row_num_in(genv, row, "A");
                    let (dx, dy) = (x - cur.0, y - cur.1);
                    let len = dx.hypot(dy).max(1e-9);
                    let mid = ((cur.0 + x) / 2.0, (cur.1 + y) / 2.0);
                    pt(mid.0 + dy / len * sag, mid.1 - dx / len * sag, &mut u);
                }
                "EllipticalArcTo" | "RelEllipticalArcTo" => {
                    let (mut mx, mut my) = (row_num_in(genv, row, "A"), row_num_in(genv, row, "B"));
                    if rel {
                        // Raw signed dims — rel_adjust multiplies
                        // raw (negative widths mirror the bound).
                        mx *= width;
                        my *= height;
                    }
                    mx *= gscale.0;
                    my *= gscale.1;
                    pt(mx, my, &mut u);
                    // The arc's apex beyond the on-curve point.
                    pt(
                        2.0 * mx - (cur.0 + x) / 2.0,
                        2.0 * my - (cur.1 + y) / 2.0,
                        &mut u,
                    );
                }
                "CubicBezTo" | "RelCubBezTo" => {
                    // (A,B) and (C,D): two control points.
                    let (mut a, mut b) = (row_num_in(genv, row, "A"), row_num_in(genv, row, "B"));
                    let (mut c, mut d) = (row_num_in(genv, row, "C"), row_num_in(genv, row, "D"));
                    if rel {
                        a *= width.max(1e-6);
                        b *= height.max(1e-6);
                        c *= width.max(1e-6);
                        d *= height.max(1e-6);
                    }
                    pt(a * gscale.0, b * gscale.1, &mut u);
                    pt(c * gscale.0, d * gscale.1, &mut u);
                }
                "QuadBezTo" | "RelQuadBezTo" => {
                    let (mut a, mut b) = (row_num_in(genv, row, "A"), row_num_in(genv, row, "B"));
                    if rel {
                        a *= width.max(1e-6);
                        b *= height.max(1e-6);
                    }
                    pt(a * gscale.0, b * gscale.1, &mut u);
                }
                "NURBSTo" => {
                    // #188: the interior control points live in the
                    // E formula's (x, y, knot, weight) groups.
                    let e_text = row
                        .cell("E")
                        .and_then(|c| c.v.clone().or_else(|| c.f.clone()))
                        .unwrap_or_default();
                    if let Some(f) = crate::curve::parse_nurbs(&e_text) {
                        // Per-axis: a fraction lands directly in
                        // instance units (the frame's own Width/
                        // Height); an absolute axis carries
                        // master-unit literals and takes the scale
                        // once — mirroring the render arm (#218
                        // review round: no squared stretch).
                        let fx = if f.x_type == 0.0 { width } else { gscale.0 };
                        let fy = if f.y_type == 0.0 { height } else { gscale.1 };
                        for (gx, gy, _, _) in &f.points {
                            pt(gx * fx, gy * fy, &mut u);
                        }
                    }
                }
                "PolylineTo" => {
                    // The POLYLINE() interior vertices (same per-axis
                    // rule as NURBSTo), the master's when the row has
                    // none of its own.
                    let a_text = row
                        .cell("A")
                        .and_then(|c| c.v.clone().or_else(|| c.f.clone()))
                        .unwrap_or_default();
                    let occ = poly_occ;
                    poly_occ += 1;
                    let a_text = match crate::curve::parse_polyline(&a_text) {
                        Some(_) => a_text,
                        None => master_polyline(walk_ix, row.ix, occ).unwrap_or_default(),
                    };
                    if let Some((xt, yt, pts)) = crate::curve::parse_polyline(&a_text) {
                        let fx = if xt == 0.0 { width } else { gscale.0 };
                        let fy = if yt == 0.0 { height } else { gscale.1 };
                        for (px, py) in pts {
                            pt(px * fx, py * fy, &mut u);
                        }
                    }
                }
                _ => {}
            }
            cur = (x, y);
            if first_pt.is_none() {
                first_pt = Some((x, y));
            }
        }
        // #185: arrowheads draw at physical size beyond the row-point
        // hull — a size-5/6 head on a horizontal 1D connector reaches
        // ±0.125/±0.25 in sideways against the 0.08 in bleed pad, so
        // union a conservative square (the gallery's largest reach) at
        // each end that carries a head. Same gate as the renderer's.
        let arrowed = |arrow: f64| arrow >= 0.5 && !(46.0..=50.5).contains(&arrow);
        if get("BeginArrow", &text_env).is_some_and(arrowed)
            || get("EndArrow", &text_env).is_some_and(arrowed)
        {
            let weight = get("LineWeight", &text_env).unwrap_or(0.75 / 72.0);
            let pad = |pt: (f64, f64), arrow: Option<f64>, sz: f64, u: &mut Focus| {
                if !arrow.is_some_and(arrowed) {
                    return;
                }
                let r = arrow_head_reach(weight, sz);
                // LOCAL y-down corners — box_union applies the affine
                // itself; pre-applying it here doubled the transform
                // and threw the pad square across the page (#218
                // review).
                let (lx, ly) = (pt.0, -pt.1);
                box_union(lx - r, ly - r, lx + r, ly + r, &a, u);
            };
            if let Some((x, y)) = first_pt {
                pad(
                    (x, y),
                    get("BeginArrow", &text_env),
                    get("BeginArrowSize", &text_env).unwrap_or(2.0),
                    &mut u,
                );
            }

            pad(
                cur,
                get("EndArrow", &text_env),
                get("EndArrowSize", &text_env).unwrap_or(2.0),
                &mut u,
            );
        }
    }
    // Text can overflow the shape box (TxtWidth/TxtHeight are
    // independent cells); bound it with emit_text's placement math.
    // The decision uses the RENDERED text: a self-closing `<fld/>`
    // marker substitutes a nonempty value the plain `text` cell does
    // not carry — omitting the frame clipped the substituted label
    // (CodeRabbit PR review).
    let has_text = s
        .sheet
        .text
        .as_deref()
        .map(str::trim)
        .is_some_and(|t| !t.is_empty())
        || s.sheet
            .text_raw
            .as_deref()
            .is_some_and(|r| r.contains("<fld"));
    if has_text {
        let tw = get("TxtWidth", &text_env).unwrap_or(width).max(0.02);
        let th = get("TxtHeight", &text_env).unwrap_or(height).max(0.02);
        let (tlx, tly) = (
            get("TxtLocPinX", &text_env).unwrap_or(tw / 2.0),
            get("TxtLocPinY", &text_env).unwrap_or(th / 2.0),
        );
        let (tx, ty) = (
            get("TxtPinX", &text_env).unwrap_or(width / 2.0),
            get("TxtPinY", &text_env).unwrap_or(height / 2.0),
        );
        // #231 outdented bullets hang LEFT of the text frame: a
        // negative IndFirst draws the glyph at the text area's left +
        // IndFirst, which can precede the frame (and the shape box) —
        // the frame-only bounds let that ink escape the ink box, so
        // the layer viewBox clips it and the #236 cull skips the
        // child (local CodeRabbit round 3 + final pass). Both branches
        // bound the EXTENDED frame (left edge out by the escape): the
        // unrotated branch through the shape transform directly; the
        // rotated branch through emit_text's own wrapper first — the
        // escape rotates with the block.
        let (x0, y0) = (tx - tlx, -(ty - tly + th));
        let tb = resolve_text_block(doc, s, chain, width, height);
        // Text taller than its frame overflows on the side opposite
        // the alignment: down for top, up for bottom, both ways for
        // middle. Bound the laid-out height (TEXTHEIGHT, the same
        // slots emit_text paints), not just the frame (CodeRabbit on
        // PR #325).
        let (ty0, ty1) = {
            let text = crate::fields::substituted_text(doc, s, chain, None)
                .or_else(|| s.sheet.text.clone())
                .unwrap_or_default();
            let m = crate::textfunc::SheetTextMeasure::new(doc, s, chain, text.clone());
            let h = {
                use vsdx::formula::TextMeasure as _;
                m.text_height(&text, tw)
            }
            .unwrap_or(th)
            .max(th);
            match tb.valign {
                1 => (y0, y0 + h),
                3 => (y0 + th - h, y0 + th),
                _ => (y0 + (th - h) / 2.0, y0 + (th + h) / 2.0),
            }
        };
        let bullet_escape = tb.bullet.as_ref().map_or(0.0, |b| {
            let escape = tb.insets.0 + tb.ind_first;
            if escape < 0.0 {
                -escape + bullet_advance_in(&tb, b)
            } else {
                0.0
            }
        });
        if get("TxtAngle", &text_env).unwrap_or(0.0) != 0.0 {
            // Rotated text: push the extended frame's corners through
            // emit_text's own wrapper — the pin rotation (flips
            // composed, net = −sx·sy·TxtAngle) in the shape-local
            // y-down frame — then the shape transform. The old
            // centered circumscribed square under-covered BOTH the
            // one-sided bullet escape (half of it — Greptile round 8)
            // and oblique rotations of the frame itself.
            let angle = get("TxtAngle", &text_env).unwrap_or(0.0);
            let (fsx, fsy) = (
                if get("FlipX", &text_env).unwrap_or(0.0) != 0.0 {
                    -1.0
                } else {
                    1.0
                },
                if get("FlipY", &text_env).unwrap_or(0.0) != 0.0 {
                    -1.0
                } else {
                    1.0
                },
            );
            let net_deg = -(fsx * fsy) * angle.to_degrees();
            let text_tf = Affine::translate(tx, -ty)
                .then(&Affine::scale(fsx, fsy))
                .then(&Affine::rotate_deg(net_deg))
                .then(&Affine::translate(-tx, ty));
            // `then` applies its argument FIRST (self · next, the SVG
            // transform-list reading) — so the text rotation runs on
            // the shape-LOCAL corner and `a` carries the result to
            // the page. The round-8 form composed them backwards and
            // rotated page coordinates around a shape-local pivot,
            // landing the hull far from the shape (Greptile round 9).
            let a_text = a.then(&text_tf);
            box_union(x0 - bullet_escape, ty0, x0 + tw, ty1, &a_text, &mut u);
        } else {
            box_union(x0 - bullet_escape, ty0, x0 + tw, ty1, &a, &mut u);
        }
    }
    (u.acc.unwrap_or((0.0, 0.0, 0.0, 0.0)), a)
}

/// The page-space ink box of every shape in `shape_id`'s subtree —
/// each entry unions the node's own box with all its descendants'
/// ([`node_ink_box`], one shared walk). This is the #236 patch cull's
/// predicate source: a node whose SUBTREE box misses the padded
/// damage rect paints nothing inside it and is omitted from the patch
/// serialization. The top-level root's own entry covers its whole
/// subtree.
pub fn subtree_ink_boxes(
    doc: &Document,
    page: &vsdx::Page,
    shape_id: i64,
) -> Option<HashMap<i64, (f64, f64, f64, f64)>> {
    let page_h = num(page.sheet().and_then(|s| s.cell("PageHeight"))).unwrap_or(11.0);
    let root = page.shapes.iter().find(|s| s.id == shape_id)?;
    let mut map: HashMap<i64, (f64, f64, f64, f64)> = HashMap::new();
    fn walk(
        doc: &Document,
        s: &Shape,
        chain: &[&Shape],
        page_h: f64,
        top: bool,
        to_page: Affine,
        map: &mut HashMap<i64, (f64, f64, f64, f64)>,
    ) -> (f64, f64, f64, f64) {
        let mut chain: Vec<&Shape> = chain.to_vec();
        chain.push(s);
        let (own, a) = node_ink_box(doc, s, &chain, page_h, top, to_page);
        let mut acc = own;
        for sub in &s.sub_shapes {
            let sub_box = walk(doc, sub, &chain, page_h, false, a, map);
            acc = union_box(acc, sub_box);
        }
        map.insert(s.id, acc);
        acc
    }
    walk(doc, root, &[], page_h, true, Affine::IDENTITY, &mut map);
    Some(map)
}

/// Union two (x, y, w, h) page rects — the bounds-walk's accumulator
/// as a plain function (the #236 subtree-box pass unions bottom-up).
fn union_box(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> (f64, f64, f64, f64) {
    let x1 = (a.0 + a.2).max(b.0 + b.2);
    let y1 = (a.1 + a.3).max(b.1 + b.3);
    let nx = a.0.min(b.0);
    let ny = a.1.min(b.1);
    (nx, ny, x1 - nx, y1 - ny)
}

/// One top-level shape's subtree as a standalone layer SVG (#93): the
/// same absolute page coordinates and the same `render_shape` walk as
/// [`render_page_with`] — pixel-identical ink — but the viewBox is the
/// subtree's [`shape_ink_bounds`] and no page chrome/background is
/// emitted (the canvas composites its own paper under the layer
/// quads). The canvas, not the export path, is the consumer.
pub fn render_shape_layer(
    doc: &Document,
    page: &vsdx::Page,
    shape_id: i64,
    hidden: &[i64],
    bounds: (f64, f64, f64, f64),
) -> anyhow::Result<String> {
    render_shape_layer_opts(doc, page, shape_id, hidden, bounds, None, None, None)
        .map(|(_, svg)| svg)
}

/// [`render_shape_layer`] with the canvas-only options (GitHub #22,
/// #124): `skip_text_shape` suppresses one shape's text while the
/// GUI's in-place editor is open on it, and `greek_ppi` turns on text
/// greeking for layers rasterized at `ppi` — text blocks whose em
/// height would fall under [`TEXT_GREEK_PX`] device pixels emit gray
/// bars instead of glyphs. Returns the SVG plus every text size the
/// walk met (inches), the cache's greek profile for deciding whether
/// a ppi-bucket change re-renders or re-serializes.
///
/// `fields` shares one page walk's [`crate::fields::FieldsCtx`]
/// across roots (its `Sheet.N!` layer caches per CONTEXT — a
/// per-shape context rebuilds it every call, the quadratic cost the
/// sharing exists to remove); `None` builds a fresh one for one-shot
/// renders.
#[allow(clippy::too_many_arguments)] // the layer's full identity + the shared field walk
pub fn render_shape_layer_opts(
    doc: &Document,
    page: &vsdx::Page,
    shape_id: i64,
    hidden: &[i64],
    bounds: (f64, f64, f64, f64),
    skip_text: Option<i64>,
    greek_ppi: Option<f32>,
    fields: Option<&crate::fields::FieldsCtx<'_>>,
) -> anyhow::Result<(Vec<f64>, String)> {
    render_shape_layer_inner(
        doc, page, shape_id, hidden, bounds, skip_text, greek_ppi, None, fields,
    )
}

/// [`render_shape_layer_opts`] for the #236 damage-region patch: only
/// the subtree nodes whose ink box intersects `cull_rect` (page
/// inches) are serialized. Omitted nodes lie entirely outside the
/// patch rect (the caller pads it past stroke bleed and anti-aliasing
/// reach), so pixels inside the patch are unchanged — the
/// `layers::culled_patch_pixels_match_the_full_layer_render` pin
/// holds the patch-vs-full byte comparison for the culled tree.
/// `bounds` stays the FULL layer's viewBox: the patch renders through
/// the same grid as the existing texture.
#[allow(clippy::too_many_arguments)] // the layer's full identity + the cull rect (#236)
pub fn render_shape_layer_opts_culled(
    doc: &Document,
    page: &vsdx::Page,
    shape_id: i64,
    hidden: &[i64],
    bounds: (f64, f64, f64, f64),
    skip_text: Option<i64>,
    greek_ppi: Option<f32>,
    cull_rect: (f64, f64, f64, f64),
    fields: Option<&crate::fields::FieldsCtx<'_>>,
) -> anyhow::Result<(Vec<f64>, String)> {
    let boxes = subtree_ink_boxes(doc, page, shape_id)
        .ok_or_else(|| anyhow::anyhow!("shape {shape_id} not found on page {}", page.id))?;
    let cull = InkCull {
        rect: cull_rect,
        boxes: &boxes,
    };
    render_shape_layer_inner(
        doc,
        page,
        shape_id,
        hidden,
        bounds,
        skip_text,
        greek_ppi,
        Some(&cull),
        fields,
    )
}

/// The #236 cull predicate: a shape id → its subtree ink box, plus the
/// padded damage rect. `skips` is the render walk's per-node test — a
/// node whose SUBTREE box misses the rect paints nothing inside it
/// (the full render's own viewBox clips anything beyond the box), so
/// omitting it changes no pixel the patch will show.
struct InkCull<'a> {
    rect: (f64, f64, f64, f64),
    boxes: &'a HashMap<i64, (f64, f64, f64, f64)>,
}

impl InkCull<'_> {
    fn skips(&self, id: i64) -> bool {
        // A node missing from the map cannot happen (the pass walks
        // the same subtree); keep it if it does — conservative.
        self.boxes
            .get(&id)
            .is_some_and(|b| !boxes_intersect(*b, self.rect))
    }
}

/// Two (x, y, w, h) page rects intersect (closed test — a shared edge
/// counts, so the caller's padding is what creates distance).
fn boxes_intersect(a: (f64, f64, f64, f64), b: (f64, f64, f64, f64)) -> bool {
    a.0 <= b.0 + b.2 && b.0 <= a.0 + a.2 && a.1 <= b.1 + b.3 && b.1 <= a.1 + a.3
}

#[allow(clippy::too_many_arguments)] // shared body of the plain and culled serializers + the field walk
fn render_shape_layer_inner(
    doc: &Document,
    page: &vsdx::Page,
    shape_id: i64,
    hidden: &[i64],
    bounds: (f64, f64, f64, f64),
    skip_text: Option<i64>,
    greek_ppi: Option<f32>,
    cull: Option<&InkCull>,
    fields: Option<&crate::fields::FieldsCtx<'_>>,
) -> anyhow::Result<(Vec<f64>, String)> {
    // A caller-supplied context shares the page walk's `Sheet.N!`
    // layer across roots; a fresh one serves one-shot renders.
    let owned_fields;
    let fields = match fields {
        Some(f) => f,
        None => {
            owned_fields = crate::fields::FieldsCtx::for_page(doc, page);
            &owned_fields
        }
    };
    let page_h = num(page.sheet().and_then(|s| s.cell("PageHeight"))).unwrap_or(11.0);
    let Some(shape) = page.shapes.iter().find(|s| s.id == shape_id) else {
        anyhow::bail!("shape {shape_id} not found on page {}", page.id);
    };
    let hidden_member = |s: &Shape| -> bool {
        s.sheet
            .cell("LayerMember")
            .and_then(|c| c.numeric())
            .is_some_and(|m| hidden.contains(&(m as i64)))
    };
    let greek = greek_ppi.map(GreekCtx::new);
    // The canvas render (a ppi is given) paints every stroke at least
    // one device pixel wide, like Visio on screen; exports pass none.
    let _floor = StrokeFloorGuard::set(greek_ppi, bounds);
    let mut svg = String::new();
    let (bx, by, bw, bh) = bounds;
    let _ = writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{bx:.5} {by:.5} {bw:.5} {bh:.5}" width="{pw:.0}" height="{ph:.0}">"#,
        pw = bw * PX_PER_IN,
        ph = bh * PX_PER_IN,
    );
    if !hidden_member(shape) {
        render_shape(
            doc,
            shape,
            page_h,
            &[],
            &page.part,
            skip_text,
            greek.as_ref(),
            cull,
            Some(fields),
            // The canvas jump overlay draws exactly this walk's
            // top-level connectors (#294 review). The page's own
            // radii (X and Y — a stored jump on a vertical run was
            // authored at the Y spacing) are the stored-jump evidence.
            crate::geom::jump_radii_in(doc, page),
            &mut svg,
        );
    }
    let _ = writeln!(svg, "</svg>");
    let sizes = greek.map(|g| g.sizes.into_inner()).unwrap_or_default();
    Ok((sizes, svg))
}

/// [`render_page_with`] restricted to one top-level shape's subtree —
/// the GUI's group-edit mode (GitHub #78): every other top-level shape
/// is skipped; the group renders with its members at their page
/// positions through the normal group transforms.
pub fn render_page_scoped(
    doc: &Document,
    page: &vsdx::Page,
    hidden: &[i64],
    only: i64,
) -> anyhow::Result<String> {
    render_page_opts(doc, page, hidden, Some(only), None)
}

fn render_page_opts(
    doc: &Document,
    page: &vsdx::Page,
    hidden: &[i64],
    only: Option<i64>,
    skip_text: Option<i64>,
) -> anyhow::Result<String> {
    render_page_full(doc, page, hidden, only, skip_text, true, true)
}

/// [`render_page_opts`] with the page chrome gated: `border` drops the
/// gray sheet frame, `background` drops the white page fill — the
/// file exports (#35) produce the ink only (Visio's own SVG/PDF/PNG
/// exports carry no sheet chrome), while the canvas and `dc render`
/// keep the full picture.
fn render_page_full(
    doc: &Document,
    page: &vsdx::Page,
    hidden: &[i64],
    only: Option<i64>,
    skip_text: Option<i64>,
    border: bool,
    background: bool,
) -> anyhow::Result<String> {
    let w = num(page.sheet().and_then(|s| s.cell("PageWidth"))).unwrap_or(8.5);
    let h = num(page.sheet().and_then(|s| s.cell("PageHeight"))).unwrap_or(11.0);

    let mut svg = String::new();
    let _ = writeln!(
        svg,
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{pw:.0}" height="{ph:.0}">"#,
        pw = w * PX_PER_IN,
        ph = h * PX_PER_IN,
    );
    let _ = writeln!(
        svg,
        r#"<!-- Tarahi (cleanroom VSDX renderer); page '{}' -->"#,
        escape_attr(&page.name)
    );
    if border {
        let _ = writeln!(
            svg,
            r##"<rect x="0" y="0" width="{w}" height="{h}" fill="none" stroke="#c8c8c8" stroke-width="0.01"/>"##
        );
    }
    // White background under the sheet (Visio page is white); an
    // export with `--transparent` omits it so the alpha channel shows.
    if background {
        let _ = writeln!(
            svg,
            r##"<rect x="0" y="0" width="{w}" height="{h}" fill="#ffffff"/>"##,
        );
    }
    let hidden_member = |s: &Shape| -> bool {
        s.sheet
            .cell("LayerMember")
            .and_then(|c| c.numeric())
            .is_some_and(|m| hidden.contains(&(m as i64)))
    };
    // ONE field context for the whole walk (CodeRabbit agent
    // review): the shared `Sheet.N!` layer caches per CONTEXT, so a
    // per-shape context rebuilt it for every root — the quadratic
    // cost the shared layer exists to remove.
    let fields = crate::fields::FieldsCtx::for_page(doc, page);
    for shape in &page.shapes {
        if hidden_member(shape) {
            continue;
        }
        // Guides are page furniture, never ink (#113): Visio excludes
        // them from print and export, and the canvas draws them as its
        // own overlay.
        if shape.type_.as_deref() == Some("Guide") {
            continue;
        }
        if let Some(only) = only {
            if shape.id != only {
                continue;
            }
        }
        render_shape(
            doc,
            shape,
            h,
            &[],
            &page.part,
            skip_text,
            None,
            None,
            Some(&fields),
            // The overlay covers a full-page render only; scoped
            // (`only`) renders draw no jump group (#251, #294 review) —
            // but the ink strips with the page's radius either way: a
            // stored arc is stale decoration even in a scoped view.
            if only.is_none() {
                crate::geom::jump_radii_in(doc, page)
            } else {
                (0.0, 0.0)
            },
            &mut svg,
        );
    }
    // #187: line jumps — Visio prints hops. The export appends the
    // same treatment the canvas paints (white gap, under redraw, hop
    // arc) as a page-space group above the ink. The gap/extension
    // paddings pin the canvas's screen-px extras at 96 dpi. Scoped
    // renders (`only`) draw one shape's subtree — page-wide jumps
    // would drag in crossings from connectors outside the scope.
    // The page's Show-Line-Jumps switch gates the overlay (#251), and
    // a resolved radius of 0 (an explicit LineJumpFactorX=0, #294
    // review) draws nothing rather than degenerate zero-size hops.
    let radius = crate::geom::jump_radius_in(doc, page);
    let radius_y = crate::geom::jump_radius_y_in(doc, page);
    let jumps = if only.is_none()
        && page.line_jumps_shown()
        && (radius > 0.0 || radius_y > 0.0)
    {
        crate::geom::page_jumps(doc, page, hidden)
    } else {
        Vec::new()
    };
    // Per-crossing hop radius: the jumping SEGMENT's own axis (the
    // more-horizontal line may hop on a vertical leg — Visio sizes
    // that with the Y cells, PR #299 review). An explicit Factor=0 on
    // that axis is a zero-size hop — the crossing draws no arc at all,
    // never a degenerate one (#294 review); a page whose every hop is
    // zero-sized draws no group.
    let jumps: Vec<_> = jumps
        .into_iter()
        .filter(|j| {
            if j.dir.0.abs() >= j.dir.1.abs() {
                radius > 0.0
            } else {
                radius_y > 0.0
            }
        })
        .collect();
    if !jumps.is_empty() {
        // #294: the hop radius is the page's own LineToLineX/Y ×
        // LineJumpFactorX/Y / 2, resolved through the style chain —
        // never the old 0.12 constant.
        let _ = writeln!(svg, r#"<g data-dc="line-jumps">"#);
        for j in &jumps {
            let r = if j.dir.0.abs() >= j.dir.1.abs() {
                radius
            } else {
                radius_y
            };
            append_jump_scaled(&mut svg, h, j, r);
        }
        let _ = writeln!(svg, "</g>");
    }
    let _ = writeln!(svg, "</svg>");
    Ok(svg)
}

/// The page-space paddings of the canvas overlay's screen-px extras
/// (2 px gap weight, 1 px under extension), pinned at 96 dpi.
const JUMP_GAP_EXTRA_IN: f64 = 2.0 / 96.0;
const JUMP_UNDER_EXT_IN: f64 = 1.0 / 96.0;

/// One hop in page-space SVG (y-down: `sy = page_h − y`), the canvas
/// overlay's three strokes — the white gap across the jumper, the
/// under line redrawn straight through it, and the hop arc bulging
/// page-up (`canvas.rs`'s screen-up flip, computed here in page
/// space so rotated jumpers stay correct). `radius` is the page's
/// resolved jump radius (#294 — LineToLineX × LineJumpFactorX / 2).
fn append_jump_scaled(svg: &mut String, page_h: f64, j: &crate::geom::Jump, radius: f64) {
    let (r, w, uw) = (radius, j.weight, j.under_weight);
    let sy = |y: f64| page_h - y;
    let pt = |p: (f64, f64)| format!("{:.5} {:.5}", p.0, sy(p.1));
    let hex = |c: (u8, u8, u8)| format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2);
    // 1. the gap — paper-white, slightly wider than the jumper.
    let g1 = (j.at.0 - j.dir.0 * r, j.at.1 - j.dir.1 * r);
    let g2 = (j.at.0 + j.dir.0 * r, j.at.1 + j.dir.1 * r);
    let white = "#ffffff";
    let _ = writeln!(
        svg,
        r#"<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="{white}" stroke-width="{gw:.4}"/>"#,
        x1 = g1.0,
        y1 = sy(g1.1),
        x2 = g2.0,
        y2 = sy(g2.1),
        gw = w + JUMP_GAP_EXTRA_IN,
    );
    // 2. the under line passes straight through the gap.
    let ext = r + uw + JUMP_UNDER_EXT_IN;
    let u1 = (j.at.0 - j.under.0 * ext, j.at.1 - j.under.1 * ext);
    let u2 = (j.at.0 + j.under.0 * ext, j.at.1 + j.under.1 * ext);
    let _ = writeln!(
        svg,
        r#"<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2}" stroke="{c}" stroke-width="{uw:.4}"/>"#,
        x1 = u1.0,
        y1 = sy(u1.1),
        x2 = u2.0,
        y2 = sy(u2.1),
        c = hex(j.under_color),
        uw = uw,
    );
    // 3. the hop arc — a 9-point semicircle from g1 around to g2
    // through at + n·r, the normal flipped toward page-up.
    let mut n = (-j.dir.1, j.dir.0);
    if n.1 < 0.0 {
        n = (-n.0, -n.1);
    }
    let mut path = String::new();
    for k in 0..=8 {
        let t = k as f64 / 8.0;
        let ang = std::f64::consts::PI * t;
        let local = (
            -j.dir.0 * r * ang.cos() + n.0 * r * ang.sin(),
            -j.dir.1 * r * ang.cos() + n.1 * r * ang.sin(),
        );
        let p = (j.at.0 + local.0, j.at.1 + local.1);
        path.push_str(if k == 0 { "M " } else { "L " });
        path.push_str(&pt(p));
        path.push(' ');
    }
    let _ = writeln!(
        svg,
        r#"<path d="{path}" fill="none" stroke="{c}" stroke-width="{w:.4}"/>"#,
        c = hex(j.color),
        w = w,
    );
}

/// An SVG-order affine: `x' = a·x + c·y + e`, `y' = b·x + d·y + f`
/// (y-down SVG space; the composition order matches the `transform=`
/// strings this module emits, so geometry derived here lands on the
/// same pixels the renderer draws).
#[derive(Copy, Clone, Debug, PartialEq)]
pub struct Affine {
    pub a: f64,
    pub b: f64,
    pub c: f64,
    pub d: f64,
    pub e: f64,
    pub f: f64,
}

impl Affine {
    pub const IDENTITY: Affine = Affine {
        a: 1.0,
        b: 0.0,
        c: 0.0,
        d: 1.0,
        e: 0.0,
        f: 0.0,
    };

    fn translate(tx: f64, ty: f64) -> Affine {
        Affine {
            e: tx,
            f: ty,
            ..Affine::IDENTITY
        }
    }

    /// SVG rotate(deg): clockwise on screen (y grows down).
    fn rotate_deg(deg: f64) -> Affine {
        let (s, c) = deg.to_radians().sin_cos();
        Affine {
            a: c,
            b: s,
            c: -s,
            d: c,
            ..Affine::IDENTITY
        }
    }

    fn scale(sx: f64, sy: f64) -> Affine {
        Affine {
            a: sx,
            d: sy,
            ..Affine::IDENTITY
        }
    }

    /// Matrix product `self · next` — `next` applies to points first.
    /// This is the reading order of an SVG `transform="A B C"` list
    /// (the leftmost matrix is the outermost).
    fn then(&self, next: &Affine) -> Affine {
        Affine {
            a: self.a * next.a + self.c * next.b,
            b: self.b * next.a + self.d * next.b,
            c: self.a * next.c + self.c * next.d,
            d: self.b * next.c + self.d * next.d,
            e: self.a * next.e + self.c * next.f + self.e,
            f: self.b * next.e + self.d * next.f + self.f,
        }
    }

    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        (
            self.a * x + self.c * y + self.e,
            self.b * x + self.d * y + self.f,
        )
    }

    /// Inverse (undefined on singular matrices — callers use XForm
    /// chains, which carry non-zero scales).
    pub fn invert(&self) -> Affine {
        let det = self.a * self.d - self.b * self.c;
        let (a, b, c, d) = (self.d / det, -self.b / det, -self.c / det, self.a / det);
        Affine {
            a,
            b,
            c,
            d,
            e: -(a * self.e + c * self.f),
            f: -(b * self.e + d * self.f),
        }
    }
}

/// The SVG-space affine the renderer composes for one level of the
/// `chain` (root-first, from `render_shape`/`render_shape_local`): a
/// top-level shape maps its local frame into page SVG space
/// (y-down, origin top-left); a nested shape maps into its parent's
/// local SVG frame with no flip scale of its own.
fn level_affine(doc: &Document, chain: &[&Shape], idx: usize, page_h: f64) -> Affine {
    let s = chain[idx];
    let env = shape_env(doc, s, &chain[..=idx]);
    let get = |n: &str| -> Option<f64> {
        doc.resolve_cell(s, &chain[..=idx], n)
            .and_then(|c| cell_num(&c, &env))
    };
    let width = get("Width").unwrap_or(1.0);
    let height = get("Height").unwrap_or(1.0);
    let (px, py) = (get("PinX").unwrap_or(0.0), get("PinY").unwrap_or(0.0));
    let (lx, ly) = (
        get("LocPinX").unwrap_or(width / 2.0),
        get("LocPinY").unwrap_or(height / 2.0),
    );
    let deg = -get("Angle").unwrap_or(0.0).to_degrees();
    let t = Affine::translate(px, if idx == 0 { page_h - py } else { -py })
        .then(&Affine::rotate_deg(deg));
    // Flip scale exists only at the page root (group-local rendering
    // applies no flip of its own).
    let t = if idx == 0 {
        let fx = if get("FlipX").unwrap_or(0.0) != 0.0 {
            -1.0
        } else {
            1.0
        };
        let fy = if get("FlipY").unwrap_or(0.0) != 0.0 {
            -1.0
        } else {
            1.0
        };
        t.then(&Affine::scale(fx, fy))
    } else {
        t
    };
    t.then(&Affine::translate(-lx, ly))
}

/// The composed SVG-space affine mapping the LAST chain entry's
/// PARENT-LOCAL frame to page SVG space: `T(root) · … · T(parent)`,
/// the same nesting the renderer emits. An empty/root chain yields
/// identity (a top-level shape's parent frame IS the page).
fn ancestors_affine(doc: &Document, chain: &[&Shape], page_h: f64) -> Affine {
    let mut m = Affine::IDENTITY;
    for idx in 0..chain.len().saturating_sub(1) {
        m = m.then(&level_affine(doc, chain, idx, page_h));
    }
    m
}

/// A shape's bounding box in PAGE coordinates (inches, y-up, origin
/// bottom-left): the shape's parent-local rect mapped through the
/// ancestor XForm chain — the same math the renderer composes, so
/// hit-testing and selection frames land on the drawn pixels.
/// `chain` is root-first and ends with the shape itself.
pub fn shape_page_rect(
    doc: &Document,
    chain: &[&Shape],
    page_h: f64,
) -> Option<(f64, f64, f64, f64)> {
    let s = *chain.last()?;
    let env = shape_env(doc, s, chain);
    let get = |n: &str| -> Option<f64> {
        doc.resolve_cell(s, chain, n)
            .and_then(|c| cell_num(&c, &env))
    };
    let (pin_x, pin_y) = (get("PinX")?, get("PinY")?);
    let w = get("Width").unwrap_or(1.0);
    let h = get("Height").unwrap_or(1.0);
    let lx = get("LocPinX").unwrap_or(w / 2.0);
    let ly = get("LocPinY").unwrap_or(h / 2.0);
    let m = ancestors_affine(doc, chain, page_h);
    // Parent-local y-up → SVG (y-down) → page y-up. With at least one
    // ancestor group the parent frame is a group-local y-down frame
    // (a y-up value enters it as -y); with none the parent frame IS
    // the page, whose own SVG conversion applies on the way in.
    let nested = chain.len() >= 2;
    let corner = |x: f64, y: f64| -> (f64, f64) {
        let iy = if nested { -y } else { page_h - y };
        let (sx, sy) = m.apply(x, iy);
        (sx, page_h - sy)
    };
    let pts = [
        corner(pin_x - lx, pin_y - ly),
        corner(pin_x - lx + w, pin_y - ly),
        corner(pin_x - lx, pin_y - ly + h),
        corner(pin_x - lx + w, pin_y - ly + h),
    ];
    let xs = pts.iter().map(|p| p.0);
    let ys = pts.iter().map(|p| p.1);
    let (min_x, max_x) = (
        xs.clone().fold(f64::INFINITY, f64::min),
        xs.fold(f64::NEG_INFINITY, f64::max),
    );
    let (min_y, max_y) = (
        ys.clone().fold(f64::INFINITY, f64::min),
        ys.fold(f64::NEG_INFINITY, f64::max),
    );
    Some((min_x, min_y, max_x - min_x, max_y - min_y))
}

/// Map a PAGE point (inches, y-up) into the last chain entry's
/// PARENT-LOCAL y-up frame — the inverse of [`shape_page_rect`]'s
/// mapping, used to write PinX/PinY of a nested shape during drags
/// (a member's Pin cells are parent-local by definition).
pub fn page_point_to_parent_local(
    doc: &Document,
    chain: &[&Shape],
    page_h: f64,
    p: (f64, f64),
) -> (f64, f64) {
    let inv = ancestors_affine(doc, chain, page_h).invert();
    let (x, y) = inv.apply(p.0, page_h - p.1);
    if chain.len() >= 2 {
        (x, -y)
    } else {
        (x, page_h - y)
    }
}

/// Map a point in the last chain entry's PARENT-LOCAL y-up frame to
/// PAGE inches y-up — the forward direction of
/// [`page_point_to_parent_local`], used to read a nested member's pin
/// into page space during drags.
pub fn parent_local_point_to_page(
    doc: &Document,
    chain: &[&Shape],
    page_h: f64,
    p: (f64, f64),
) -> (f64, f64) {
    let m = ancestors_affine(doc, chain, page_h);
    let iy = if chain.len() >= 2 { -p.1 } else { page_h - p.1 };
    let (sx, sy) = m.apply(p.0, iy);
    (sx, page_h - sy)
}

/// Render a shape in absolute page coordinates: the XForm transform
/// wraps geometry, master mirroring, sub-shape recursion, and the
/// text block.
#[allow(clippy::too_many_arguments)] // the walk carries its whole environment
fn render_shape<'a>(
    doc: &'a Document,
    s: &'a Shape,
    page_h: f64,
    chain: &[&'a Shape],
    part: &str,
    skip_text: Option<i64>,
    greek: Option<&GreekCtx>,
    cull: Option<&InkCull>,
    fields: Option<&crate::fields::FieldsCtx<'_>>,
    // The page's own jump radius (`geom::jump_radius_in`) in page
    // inches — the stored-jump evidence (#294; 0 = never strip: master
    // contact sheets carry no page context, and an authored arc that
    // matches no radius is preserved).
    jump_radius: (f64, f64),
    out: &mut String,
) {
    let mut chain: Vec<&'a Shape> = chain.to_vec();
    chain.push(s);
    let env = shape_env(doc, s, &chain);
    let get = |n: &str| -> Option<f64> {
        doc.resolve_cell(s, &chain, n)
            .and_then(|c| cell_num(&c, &env))
    };
    let (pin_x, pin_y) = (get("PinX").unwrap_or(0.0), get("PinY").unwrap_or(0.0));
    let width = get("Width").unwrap_or(1.0);
    let height = get("Height").unwrap_or(1.0);
    let (loc_x, loc_y) = (
        get("LocPinX").unwrap_or(width / 2.0),
        get("LocPinY").unwrap_or(height / 2.0),
    );
    let angle = get("Angle").unwrap_or(0.0);
    let flip_x = get("FlipX").unwrap_or(0.0) != 0.0;
    let flip_y = get("FlipY").unwrap_or(0.0) != 0.0;
    let (sx, sy) = (
        if flip_x { -1.0 } else { 1.0 },
        if flip_y { -1.0 } else { 1.0 },
    );

    let transform = format!(
        "translate({px:.6},{py:.6}) rotate({deg:.6}) scale({sx},{sy}) translate({lx:.6},{ly:.6})",
        px = pin_x,
        py = page_h - pin_y,
        deg = -angle.to_degrees(),
        lx = -loc_x,
        ly = loc_y,
    );

    let _ = writeln!(out, r#"<g transform="{t}">"#, t = escape_attr(&transform));

    // A multi-root master (several shapes on the master canvas, e.g. a
    // 13-shape header cluster) is drawn entirely by the mirroring below;
    // rendering the instance's own/effective geometry too would double
    // the first root.
    let multi_root = multi_root_master(doc, s);
    if !multi_root {
        render_all_geometry(doc, s, &chain, width, height, jump_radius, out);
        // Embedded image (ForeignData → media part via this part's rels).
        if let Some(uri) = foreign_image_uri(doc, part, s, &chain) {
            let _ = writeln!(
                out,
                r#"<image x="0" y="{y:.5}" width="{w:.5}" height="{h:.5}" href="{u}" preserveAspectRatio="none"/>"#,
                y = -height,
                w = width,
                h = height,
                u = escape_attr(&uri),
            );
        } else if let Some(pic) = foreign_emf_svg(doc, part, s, &chain, width, height) {
            out.push_str(&pic);
        }
    }
    // Sub-shapes of a group live in the parent's local frame.
    for sub in &s.sub_shapes {
        // #236: a sub whose subtree box misses the cull rect paints
        // nothing inside the patch — skip it and its whole subtree.
        if cull.is_some_and(|c| c.skips(sub.id)) {
            continue;
        }
        render_shape_local(doc, sub, &chain, part, skip_text, greek, cull, fields, out);
    }
    mirror_master(doc, s, &chain, width, height, greek, fields, out);

    emit_text(
        doc,
        s,
        &chain,
        width,
        height,
        sx,
        sy,
        skip_text == Some(s.id),
        greek,
        fields,
        out,
    );
    let _ = writeln!(out, "</g>");
}

/// Render a shape whose XForm is relative to its parent's local frame
/// (used for sub-shapes of groups).
#[allow(clippy::too_many_arguments)] // the render walk's context rides one call (#236 cull)
fn render_shape_local<'a>(
    doc: &'a Document,
    s: &'a Shape,
    chain: &[&'a Shape],
    part: &str,
    skip_text: Option<i64>,
    greek: Option<&GreekCtx>,
    cull: Option<&InkCull>,
    fields: Option<&crate::fields::FieldsCtx<'_>>,
    out: &mut String,
) {
    let mut chain: Vec<&'a Shape> = chain.to_vec();
    chain.push(s);
    let env = shape_env(doc, s, &chain);
    let get = |n: &str| -> Option<f64> {
        doc.resolve_cell(s, &chain, n)
            .and_then(|c| cell_num(&c, &env))
    };
    let width = get("Width").unwrap_or(1.0);
    let height = get("Height").unwrap_or(1.0);
    let transform = format!(
        "translate({px:.6},{py:.6}) rotate({deg:.6}) translate({lx:.6},{ly:.6})",
        px = get("PinX").unwrap_or(0.0),
        // group-local Y is already flipped by the parent group's scale
        py = -get("PinY").unwrap_or(0.0),
        deg = -get("Angle").unwrap_or(0.0).to_degrees(),
        lx = -get("LocPinX").unwrap_or(width / 2.0),
        ly = get("LocPinY").unwrap_or(height / 2.0),
    );
    let _ = writeln!(out, r#"<g transform="{}">"#, escape_attr(&transform));
    let multi_root = multi_root_master(doc, s);
    if !multi_root {
        // Group children aren't in the jump overlay's crossing set
        // (page_jumps collects top-level connectors only, #294
        // review): their stored jump arcs keep rendering as ink.
        render_all_geometry(doc, s, &chain, width, height, (0.0, 0.0), out);
        if let Some(uri) = foreign_image_uri(doc, part, s, &chain) {
            let _ = writeln!(
                out,
                r#"<image x="0" y="{y:.5}" width="{w:.5}" height="{h:.5}" href="{u}" preserveAspectRatio="none"/>"#,
                y = -height,
                w = width,
                h = height,
                u = escape_attr(&uri),
            );
        } else if let Some(pic) = foreign_emf_svg(doc, part, s, &chain, width, height) {
            out.push_str(&pic);
        }
    }
    for sub in &s.sub_shapes {
        // #236: the cull test, at every subtree level.
        if cull.is_some_and(|c| c.skips(sub.id)) {
            continue;
        }
        render_shape_local(doc, sub, &chain, part, skip_text, greek, cull, fields, out);
    }
    mirror_master(doc, s, &chain, width, height, greek, fields, out);
    // Group-local rendering applies no flip scale of its own, so the text
    // needs no flip compensation here.
    emit_text(
        doc,
        s,
        &chain,
        width,
        height,
        1.0,
        1.0,
        skip_text == Some(s.id),
        greek,
        fields,
        out,
    );
    let _ = writeln!(out, "</g>");
}

/// True when `s` is an instance (no inline children) of a master whose
/// canvas carries several root shapes — the whole master drawing must be
/// mirrored into the instance rect.
fn multi_root_master(doc: &Document, s: &Shape) -> bool {
    s.sub_shapes.is_empty()
        && s.master
            .and_then(|id| doc.master(id))
            .map(|m| m.shapes.len() > 1)
            .unwrap_or(false)
}

/// Master mirroring for an instance with no inline children.
/// - multi-root master: every root shape renders, positioned by its own
///   master-canvas Xform, with the union bbox mapped into the instance
///   rect (Visio draws the master's whole canvas).
/// - single-root master: the instance/effective sheet already draws the
///   root's own shape; mirror the root's sub-shape tree. Each sub-shape's
///   XForm is re-evaluated against the *instance* group size (Visio
///   semantics: cells whose formulas reference the parent stretch with
///   it; `GUARD(4MM)`-style absolute cells keep their size — issue #15),
///   emitting a corrective transform per child. Cells we cannot
///   evaluate fall back to the uniform instance/master scale, so masters
///   without authored formulas keep their historical rendering.
#[allow(clippy::too_many_arguments)] // the render walk's context rides one flat set
fn mirror_master(
    doc: &Document,
    s: &Shape,
    chain: &[&Shape],
    width: f64,
    height: f64,
    greek: Option<&GreekCtx>,
    fields: Option<&crate::fields::FieldsCtx<'_>>,
    out: &mut String,
) {
    if !s.sub_shapes.is_empty() {
        return;
    }
    let Some(m) = s.master.and_then(|id| doc.master(id)) else {
        return;
    };
    let mpart = &m.part;
    if m.shapes.len() > 1 {
        // Union bbox (y-up master canvas) over all roots.
        let rect = |r: &Shape| {
            let c = |n: &str, d: f64| r.sheet.cell(n).and_then(|x| x.numeric()).unwrap_or(d);
            let w = c("Width", 1.0).max(1e-6);
            let h = c("Height", 1.0).max(1e-6);
            let px = c("PinX", 0.0);
            let py = c("PinY", 0.0);
            let lx = c("LocPinX", w / 2.0);
            let ly = c("LocPinY", h / 2.0);
            (px - lx, py - ly, w, h)
        };
        let mut bbox: Option<(f64, f64, f64, f64)> = None;
        for r in &m.shapes {
            let (x0, y0, w, h) = rect(r);
            bbox = Some(match bbox {
                None => (x0, y0, w, h),
                Some((bx, by, bw, bh)) => {
                    let x1 = (bx + bw).max(x0 + w);
                    let y1 = (by + bh).max(y0 + h);
                    let nx = bx.min(x0);
                    let ny = by.min(y0);
                    (nx, ny, x1 - nx, y1 - ny)
                }
            });
        }
        let Some((bx, by, bw, bh)) = bbox else { return };
        let kx = width / bw.max(1e-6);
        let ky = height / bh.max(1e-6);
        // Root emitted coords are (x, -py) (local y-flip); this wrapper
        // maps the bbox to the instance rect (0..width, 0..height).
        let _ = writeln!(
            out,
            r#"<g transform="scale({kx:.6},{ky:.6}) translate({tx:.6},{ty:.6})">"#,
            tx = -bx,
            ty = -(by + bh),
        );
        for r in &m.shapes {
            // Master-canvas roots of a multi-root master: the union
            // bbox maps into the instance rect, which the node's own
            // ink box bounds — no cull of their own (#236).
            render_shape_local(doc, r, &[], mpart, None, greek, None, fields, out);
        }
        let _ = writeln!(out, "</g>");
    } else if let Some(root) = m.shapes.first() {
        let mw = root
            .sheet
            .cell("Width")
            .and_then(|c| c.numeric())
            .filter(|v| v.abs() > 1e-9);
        let mh = root
            .sheet
            .cell("Height")
            .and_then(|c| c.numeric())
            .filter(|v| v.abs() > 1e-9);
        let (kx, ky) = (
            mw.map_or(1.0, |mw| width / mw),
            mh.map_or(1.0, |mh| height / mh),
        );
        let env = mirror_env(root, s, width, height);
        for sub in &root.sub_shapes {
            let wrapper = mirror_child_transform(sub, &env, (kx, ky));
            if let Some(t) = &wrapper {
                let _ = writeln!(out, r#"<g transform="{}">"#, escape_attr(t));
            }
            // Mirrored master children map into the instance's local
            // frame, bounded by the node's own ink box — no cull of
            // their own (#236).
            render_shape_local(doc, sub, chain, mpart, None, greek, None, fields, out);
            if wrapper.is_some() {
                let _ = writeln!(out, "</g>");
            }
        }
    }
}

/// Evaluation environment for re-evaluating a master root's sub-shape
/// XForms at instance size (issue #15). Shared implementation in
/// `vsdx::model::instance_env` — the editor's cache refresh (dc-ui,
/// issue #59) computes from the same environment, so a refreshed cache
/// and a mirrored placement agree.
fn mirror_env(root: &Shape, instance: &Shape, width: f64, height: f64) -> vsdx::formula::Env {
    vsdx::model::instance_env(root, instance, width, height)
}

/// Corrective transform for one mirrored sub-shape: maps where
/// [`render_shape_local`] places it from its master XForm (`M`) to where
/// Visio places it at the instance's group size (`E`, from re-evaluated
/// formulas, falling back to uniform `k`·master per axis). Result:
/// `Some("identity-like scale")` / `Some(matrix)` / `None` when no
/// correction is needed.
fn mirror_child_transform(
    sub: &Shape,
    root_env: &vsdx::formula::Env,
    (kx, ky): (f64, f64),
) -> Option<String> {
    let num = |n: &str| sub.sheet.cell(n).and_then(|c| c.numeric());
    let m_w = num("Width").filter(|v| *v > 1e-9).unwrap_or(1.0);
    let m_h = num("Height").filter(|v| *v > 1e-9).unwrap_or(1.0);
    let m = |n: &str, d: f64| num(n).unwrap_or(d);
    let (m_px, m_py) = (m("PinX", 0.0), m("PinY", 0.0));
    let (m_lx, m_ly) = (m("LocPinX", m_w / 2.0), m("LocPinY", m_h / 2.0));
    let ang = m("Angle", 0.0);

    // Effective values: formula first (Visio re-evaluates against the
    // instance group), then the uniform-scale fallback.
    // Seed the env with only the root's QUALIFIED `Sheet.N!` spellings
    // (`vsdx::model::qualified_env`): a bare name in a child formula
    // means the child's own cell, so the root's unqualified
    // Width/Height/User.* must not leak in — without the filter a bare
    // reference silently reads the group's value and skips the per-axis
    // fallback below. (Child sheets are not in this env; bare
    // child-cell references fail to resolve and fall back.)
    let mut env: vsdx::formula::Env = vsdx::model::qualified_env(root_env);
    let eff = |name: &str, axis: f64, env: &vsdx::formula::Env| -> Option<f64> {
        let c = sub.sheet.cell(name)?;
        if let Some(f) = c.f.as_deref() {
            if f != "Inh" && f != "No Formula" {
                if let Some(v) = vsdx::formula::eval(f, env)
                    .ok()
                    .and_then(|v| v.as_f64())
                    .filter(|v| v.is_finite())
                {
                    return Some(v);
                }
            }
        }
        c.numeric().map(|v| v * axis)
    };
    let e_w = eff("Width", kx, &env).unwrap_or(kx * m_w);
    let e_h = eff("Height", ky, &env).unwrap_or(ky * m_h);
    // Pin/LocPin formulas may reference the shape's own effective size.
    env.insert("Width".into(), e_w);
    env.insert("Height".into(), e_h);
    let e_px = eff("PinX", kx, &env).unwrap_or(0.0);
    let e_py = eff("PinY", ky, &env).unwrap_or(0.0);
    let e_lx = eff("LocPinX", kx, &env).unwrap_or(e_w / 2.0);
    let e_ly = eff("LocPinY", ky, &env).unwrap_or(e_h / 2.0);

    // Matrices in SVG space (y-down), row-vector convention:
    // (a b c d e f) ≡ x' = a·x + c·y + e ; y' = b·x + d·y + f.
    let ident = Mat::identity();
    let rot = Mat::rotation(-ang);
    // Where render_shape_local places the shape from master values.
    let placed = Mat::translate(m_px, -m_py)
        .mul(&rot)
        .mul(&Mat::translate(-m_lx, m_ly));
    // Where Visio places it at instance size: same pin convention, the
    // effective LocPin, and the local frame scaled to the effective size.
    let wanted = Mat::translate(e_px, -e_py)
        .mul(&rot)
        .mul(&Mat::translate(-e_lx, e_ly))
        .mul(&Mat::scale(e_w / m_w, e_h / m_h));
    let w = wanted.mul(&placed.inv()?);
    const EPS: f64 = 1e-9;
    if w.near(&ident, EPS) {
        return None;
    }
    if w.0[1].abs() < EPS && w.0[2].abs() < EPS && w.0[4].abs() < EPS && w.0[5].abs() < EPS {
        return Some(format!("scale({:.6},{:.6})", w.0[0], w.0[3]));
    }
    Some(format!(
        "matrix({:.6},{:.6},{:.6},{:.6},{:.6},{:.6})",
        w.0[0], w.0[1], w.0[2], w.0[3], w.0[4], w.0[5]
    ))
}

/// 2D affine transform as SVG `matrix(a,b,c,d,e,f)`.
#[derive(Debug, Clone)]
struct Mat([f64; 6]);

impl Mat {
    fn identity() -> Mat {
        Mat([1.0, 0.0, 0.0, 1.0, 0.0, 0.0])
    }
    fn translate(tx: f64, ty: f64) -> Mat {
        Mat([1.0, 0.0, 0.0, 1.0, tx, ty])
    }
    fn scale(sx: f64, sy: f64) -> Mat {
        Mat([sx, 0.0, 0.0, sy, 0.0, 0.0])
    }
    fn rotation(rad: f64) -> Mat {
        let (s, c) = rad.sin_cos();
        Mat([c, s, -s, c, 0.0, 0.0])
    }
    /// self ∘ other (apply `other` first).
    fn mul(&self, other: &Mat) -> Mat {
        let a = &self.0;
        let b = &other.0;
        Mat([
            a[0] * b[0] + a[2] * b[1],
            a[1] * b[0] + a[3] * b[1],
            a[0] * b[2] + a[2] * b[3],
            a[1] * b[2] + a[3] * b[3],
            a[0] * b[4] + a[2] * b[5] + a[4],
            a[1] * b[4] + a[3] * b[5] + a[5],
        ])
    }
    fn inv(&self) -> Option<Mat> {
        let m = &self.0;
        let det = m[0] * m[3] - m[1] * m[2];
        if det.abs() < 1e-12 {
            return None;
        }
        let inv00 = m[3] / det;
        let inv01 = -m[1] / det;
        let inv10 = -m[2] / det;
        let inv11 = m[0] / det;
        Some(Mat([
            inv00,
            inv01,
            inv10,
            inv11,
            -(inv00 * m[4] + inv10 * m[5]),
            -(inv01 * m[4] + inv11 * m[5]),
        ]))
    }
    fn near(&self, other: &Mat, eps: f64) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .all(|(a, b)| (a - b).abs() <= eps)
    }
}

/// Text: laid out inside the TxtPin/TxtLocPin/TxtWidth/TxtHeight block —
/// word-wrapped to the block width, horizontally aligned per the Paragraph
/// row's HorizAlign, vertically per VertAlign, with the first Character
/// row's size/color/bold. Rotated by TxtAngle around the pin (Visio
/// vertical sidebars use TxtAngle 90).
///
/// `sx`/`sy` are the flip factors the caller's shape transform applied
/// (±1). Visio flips mirror geometry but never the text block: the block
/// keeps its TxtPin placement inside the flipped frame, yet the glyphs
/// must stay un-mirrored, and a TxtAngle rotation inside a mirrored frame
/// inverts its page-space sense — the connector masters author
/// `TxtAngle = IF(BITXOR(FlipX,FlipY),1,-1)*Angle` against exactly that,
/// so the page-space net is `Angle + sx*sy*TxtAngle` (0 = horizontal
/// labels, issue #19).
// The eight parameters are the full text-emission context; grouping
// them would just move the long call sites into a struct nobody else
// shares.
#[allow(clippy::too_many_arguments)]
/// A shape's resolved text block: placement in the shape's LOCAL frame
/// (inches, y-up) plus the styling the renderer paints it with — the
/// single source of truth shared by [`emit_text`] (the SVG backend),
/// the GUI's in-place editor overlay (GitHub #22), and tests, so the
/// editing view cannot drift from the rendered result.
///
/// Placement follows the sheet's `Txt*` cells with the same defaults
/// `emit_text` applies: pin at the shape's center, size = the shape's
/// Width/Height, LocPin centered in the block. Styling resolves
/// through the instance → master chain exactly like the renderer
/// (Character `Font`/`Size`/`Color`/`Style`, Paragraph `HorzAlign`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextBlock {
    /// `TxtPinX`/`TxtPinY` — the block's pin in the shape's local frame.
    pub pin: (f64, f64),
    /// `TxtWidth`/`TxtHeight` (clamped to ≥ 0.02 in like `emit_text`).
    pub size: (f64, f64),
    /// `TxtLocPinX`/`TxtLocPinY` — the pin's offset from the block's
    /// lower-left corner.
    pub loc_pin: (f64, f64),
    /// `TxtAngle` (radians, local).
    pub angle: f64,
    /// Text-block insets (left, right, top, bottom) — Visio defaults
    /// 0.1 in / 0.1 in / 0.0555 in / 0.0555 in.
    pub insets: (f64, f64, f64, f64),
    /// Line height in inches (`LineSpacing` × font size, default 1.2).
    pub line_height: f64,
    /// Character `Font` (family name) if any cell resolves one.
    pub font: Option<String>,
    /// Character `Size` in inches (0.13889 in = 10 pt); default 10 pt.
    pub font_size: f64,
    /// Resolved Character `Color` as `#rrggbb` (palette/theme aware).
    pub color: String,
    /// Character `Style` bit 1.
    pub bold: bool,
    /// Character `Style` bit 2.
    pub italic: bool,
    /// Paragraph `HorzAlign`: 0 left, 1 center (Visio default), 2
    /// right, 3 justify.
    pub halign: i32,
    /// `VertAlign`: 1 top, 2 middle (default), 3 bottom.
    pub valign: i32,
    /// Character `ComplexScriptFont` family (#257 text-direction):
    /// the face complex-script (RTL-base) paragraphs paint with; the
    /// spec's zero/absent semantics resolve to `None` and the Latin
    /// `Font` chain stays. Paragraph granularity — the per-run `<cp>`
    /// model stays out until a fixture shows one (see
    /// Analysis/rtl-text-direction.md).
    pub cs_font: Option<String>,
    /// Effective `ComplexScriptSize` in inches (the spec's `0 → Size`,
    /// `<0 → |v|·Size/100` semantics already applied; `None` = Size).
    pub cs_size: Option<f64>,
    /// Character `Pos` of the first row (0 normal, 1 superscript, 2
    /// subscript — #230). Run-granular Pos (`<cp>` markers, stripped
    /// by the parser) rides the run work in #267; the whole block
    /// follows the first row like every other first-row cell.
    pub pos: i32,
    /// Paragraph `IndFirst` in inches (first-line indent; negative =
    /// hanging indent, #231). Corpus default 0.
    pub ind_first: f64,
    /// Paragraph `IndLeft` in inches — wrapped lines start here
    /// (#231). Corpus default 0.
    pub ind_left: f64,
    /// #231 bullet layout (first Paragraph row granularity);
    /// `None` = not bulleted.
    pub bullet: Option<BulletSpec>,
    /// #229 text background: the resolved fill color and its alpha
    /// (0–255, from TextBkgndTrans's fractional transparency). `None`
    /// paints nothing — TextBkgnd 0/255, or 100 % transparency.
    pub text_bkgnd: Option<(String, u8)>,
}

/// A bulleted block's glyph spec (#231): the built-in round styles
/// 1–7 draw as paths (viewer-proof — a text glyph changes face with
/// the viewer's fonts); a custom bullet string draws as text in its
/// own face.
#[derive(Debug, Clone, PartialEq)]
pub enum BulletGlyph {
    /// Built-in style 1–7 (Word's round set, drawn as • o ▪ ❑ ❖ ➢ ✓).
    Round(u8),
    /// A custom bullet string (drawn as a text glyph).
    Str(String),
}

/// The cells behind one bulleted block (#231, first Paragraph row).
#[derive(Debug, Clone, PartialEq)]
pub struct BulletSpec {
    pub glyph: BulletGlyph,
    /// BulletFont — the bullet's own face family; `None` inherits the
    /// paragraph font (the measured rule: BulletFont 0 inherits).
    pub font: Option<String>,
    /// Bullet size in inches: BulletFontSize < 0 is a percentage of
    /// the text size (default −100 % = the text size); ≥ 0 is an
    /// absolute size in Char.Size inches (the > 100 % case stays an
    /// open question on #231 — unwitnessed).
    pub size: f64,
    /// `TextPosAfterBullet` in inches, ABSOLUTE from the line start —
    /// Visio 2021 does not add it to IndFirst despite the documented
    /// formula (measured on #231: IndFirst 0.25 + TextPosAfterBullet
    /// 0.1 starts the text at the 0.5 tab stop, not 0.35).
    pub pos_after: f64,
    /// `DefaultTabStop` (document sheet; 0.5 in default) — the
    /// tab-stop grid the after-bullet text snaps to.
    pub tab_stop: f64,
}

/// Resolve one shape's [`TextBlock`] through the instance → master
/// chain (GitHub #22): the same cell reads and defaults `emit_text`
/// performs, hoisted so the GUI overlay and the renderer agree.
pub fn text_block(doc: &Document, page: &vsdx::Page, shape_id: i64) -> Option<TextBlock> {
    let chain = dc_core::scope::shape_chain(doc, page.id, shape_id).ok()?;
    let s = *chain.last()?;
    let env = shape_env(doc, s, &chain);
    let get = |n: &str| -> Option<f64> {
        doc.resolve_cell(s, &chain, n)
            .and_then(|c| cell_num(&c, &env))
    };
    let width = get("Width").unwrap_or(1.0);
    let height = get("Height").unwrap_or(1.0);
    Some(resolve_text_block(doc, s, &chain, width, height))
}

/// The shape's text block in PAGE coordinates (inches, y-up, origin
/// bottom-left — [`shape_page_rect`]'s convention): the block's local
/// rect mapped through the ancestor XForm chain. The shape's own
/// `Angle`/`TxtAngle` are not applied (v1 of the in-place editor,
/// GitHub #22 — a rotated shape yields the axis-aligned box).
pub fn text_block_page_rect(
    doc: &Document,
    page: &vsdx::Page,
    shape_id: i64,
) -> Option<(f64, f64, f64, f64)> {
    let page_h = num(page.sheet().and_then(|s| s.cell("PageHeight"))).unwrap_or(11.0);
    let chain = dc_core::scope::shape_chain(doc, page.id, shape_id).ok()?;
    let s = *chain.last()?;
    let env = shape_env(doc, s, &chain);
    let get = |n: &str| -> Option<f64> {
        doc.resolve_cell(s, &chain, n)
            .and_then(|c| cell_num(&c, &env))
    };
    let width = get("Width").unwrap_or(1.0);
    let height = get("Height").unwrap_or(1.0);
    let tb = resolve_text_block(doc, s, &chain, width, height);
    // Shape-local → parent-local (v1, unrotated: the same
    // pin-anchored placement `shape_page_rect` uses for the shape
    // rect) → page through the ancestor chain.
    let (pin_x, pin_y) = (get("PinX").unwrap_or(0.0), get("PinY").unwrap_or(0.0));
    let (loc_x, loc_y) = (
        get("LocPinX").unwrap_or(width / 2.0),
        get("LocPinY").unwrap_or(height / 2.0),
    );
    let m = ancestors_affine(doc, &chain, page_h);
    let nested = chain.len() >= 2;
    let corner = |x: f64, y: f64| -> (f64, f64) {
        let iy = if nested { -y } else { page_h - y };
        let (sx, sy) = m.apply(x, iy);
        (sx, page_h - sy)
    };
    let (tx, ty) = tb.pin;
    let (tw, th) = tb.size;
    let (lpx, lpy) = tb.loc_pin;
    let bx = pin_x - loc_x + tx - lpx;
    let by = pin_y - loc_y + ty - lpy;
    let pts = [
        corner(bx, by),
        corner(bx + tw, by),
        corner(bx, by + th),
        corner(bx + tw, by + th),
    ];
    let xs = pts.iter().map(|p| p.0);
    let ys = pts.iter().map(|p| p.1);
    let (min_x, max_x) = (
        xs.clone().fold(f64::INFINITY, f64::min),
        xs.fold(f64::NEG_INFINITY, f64::max),
    );
    let (min_y, max_y) = (
        ys.clone().fold(f64::INFINITY, f64::min),
        ys.fold(f64::NEG_INFINITY, f64::max),
    );
    Some((min_x, min_y, max_x - min_x, max_y - min_y))
}

/// The complex-script size an RTL line's glyphs actually paint with
/// (#257 text-direction, PR #269 review): the resolved
/// ComplexScriptSize (negative-percent semantics already applied)
/// when it EXCEEDS the Latin size — a smaller CS size keeps the
/// block's Latin slot (Visio's LineSpacing is a block cell; only
/// larger glyphs need a taller slot or adjacent lines overlap).
pub(crate) fn effective_cs_size(tb: &TextBlock) -> f64 {
    tb.cs_size.unwrap_or(tb.font_size).max(tb.font_size)
}

/// One wrapped line's vertical slot: the block's line height, scaled
/// by the line's effective size when its glyphs paint larger than the
/// Latin size (RTL lines under a bigger ComplexScriptSize — PR #269
/// review: default spacing is 1.2 em, so 2 em glyphs in 1.2 em slots
/// overlapped).
pub(crate) fn line_slot_height(tb: &TextBlock, rtl: bool) -> f64 {
    if rtl {
        tb.line_height * (effective_cs_size(tb) / tb.font_size)
    } else {
        tb.line_height
    }
}

/// The first baseline's offset below the text-block's TOP edge, in
/// the block's local inches (y-down) — the single placement authority
/// shared by `emit_text` and the GUI's in-place editor overlay, so
/// the editing caret sits on the very line the renderer draws (the
/// #22 review's "no translation between edit and render"). Subsequent
/// lines sit their [`line_slot_height`] lower. `total_h` is the
/// wrapped text's height (the SUM of the line slots), and
/// `first_line_size` is the first line's effective glyph size — a
/// first line of larger complex-script glyphs ascends from a higher
/// baseline than the Latin ascent.
pub fn text_first_baseline(tb: &TextBlock, total_h: f64, first_line_size: f64) -> f64 {
    let (_ins_l, _ins_r, ins_t, ins_b) = tb.insets;
    let th = tb.size.1;
    match tb.valign {
        1 => ins_t + first_line_size * 0.8,                // top
        3 => th - ins_b - total_h + first_line_size * 0.8, // bottom
        _ => (th - total_h) / 2.0 + first_line_size * 0.8, // middle
    }
}

/// The cell resolution behind [`TextBlock`] — shared by `emit_text`
/// (which paints from it) and the public [`text_block`] wrapper.
pub(crate) fn resolve_text_block(
    doc: &Document,
    s: &Shape,
    chain: &[&Shape],
    width: f64,
    height: f64,
) -> TextBlock {
    let env = shape_env(doc, s, chain);
    let get = |n: &str| -> Option<f64> {
        doc.resolve_cell(s, chain, n)
            .and_then(|c| cell_num(&c, &env))
    };
    let tx = get("TxtPinX").unwrap_or(width / 2.0);
    let ty = get("TxtPinY").unwrap_or(height / 2.0);
    let tw = get("TxtWidth").unwrap_or(width).max(0.02);
    let th = get("TxtHeight").unwrap_or(height).max(0.02);
    let angle = get("TxtAngle").unwrap_or(0.0);
    // Char.Size lives in the Character section's first row, stored in
    // inches (0.13889 in = 10 pt); the viewBox unit is inches, so the
    // value is the SVG font-size directly.
    let font_size = char_size(doc, s, chain).unwrap_or(0.13888).max(0.02);
    // First Character row extras: color (palette/theme aware) and the
    // Style bitmask (1 = bold, 2 = italic). Both resolve per-cell through
    // `char_cell`, so a LangID-only instance row picks them up from the
    // master/style rows instead of silently taking the built-in defaults.
    let color = char_cell(doc, s, chain, "Color")
        .map(|c| color_value(doc, c.v.as_deref().unwrap_or(""), c.f.as_deref()))
        .unwrap_or_else(|| "#000000".into());
    let style_bits = char_cell(doc, s, chain, "Style")
        .and_then(|c| c.numeric())
        .unwrap_or(0.0) as u32;

    // Paragraph alignment: Visio HorzAlign 0 = left, 1 = center, 2 = right,
    // 3 = justify (rendered as left). Absent cell → Visio default 1 (center).
    // Sections inherit separately from cells: a sub-shape that carries e.g.
    // Character but no Paragraph still takes Paragraph from its master shape
    // (`MasterShape` link) — effective_sheet stops at the instance sheet
    // once it has any sections, so consult the source explicitly.
    let para_row = s
        .sheet
        .sections_named("Paragraph")
        .into_iter()
        .next()
        .or_else(|| {
            doc.instance_source(s, chain)
                .and_then(|src| src.sheet.sections_named("Paragraph").into_iter().next())
        })
        .and_then(|sec| sec.rows.first());
    let halign = para_row
        .and_then(|r| r.cell("HorzAlign"))
        .and_then(|c| c.numeric())
        .unwrap_or(1.0) as i32;
    // The ShapeSheet cell is `VerticalAlign` (Text Block Format
    // section): 0 top, 1 middle (the Normal-style default), 2 bottom.
    // The block keeps its own 1 top / 2 middle / 3 bottom encoding,
    // so shift by one. (There is no `VertAlign` cell — reading that
    // name always fell back to middle.)
    let valign = get("VerticalAlign").map_or(2, |v| (v.round() as i32).clamp(0, 2) + 1);

    // Text-block insets (Visio defaults: 0.1 in left/right, 0.0555 in
    // top/bottom) and 120% line spacing.
    let ins_l = get("LeftMargin").unwrap_or(0.1);
    let ins_r = get("RightMargin").unwrap_or(0.1);
    let ins_t = get("TopMargin").unwrap_or(0.0555);
    let ins_b = get("BottomMargin").unwrap_or(0.0555);
    let line_height = get("LineSpacing")
        .map(|l| l * font_size)
        .unwrap_or(font_size * 1.2)
        .max(font_size);

    // Complex-script character cells (#257 text-direction, paragraph
    // granularity — see Analysis/rtl-text-direction.md). Size
    // semantics per [MS-VSDX] §2.4.4.62: >0 = the value, 0 = the
    // sibling Size, <0 = |v|·Size/100.
    let cs_font = char_cell(doc, s, chain, "ComplexScriptFont")
        .and_then(|c| c.v.clone())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty() && v.parse::<f64>().is_err());
    let cs_size_raw = char_cell(doc, s, chain, "ComplexScriptSize").and_then(|c| c.numeric());
    let cs_size = cs_size_raw.map(|v| {
        if v > 0.0 {
            v
        } else if v < 0.0 {
            v.abs() * font_size / 100.0
        } else {
            font_size
        }
    });

    // Character.Pos (#230): first-row granularity — the parser strips
    // the `<cp>` run markers, so per-run Pos arrives with the run work
    // (#267) and the whole block follows row 0 like color/bold/italic.
    let pos = char_cell(doc, s, chain, "Pos")
        .and_then(|c| c.numeric())
        .unwrap_or(0.0) as i32;

    // Paragraph indents (#231) resolved PER CELL through the
    // instance → master inheritance (PR #284 review): an instance
    // Paragraph row carrying only HorzAlign must not shadow the
    // master's IndFirst/IndLeft/bullet cells, the way a whole-row
    // pick would.
    let para_cell = |name: &str| -> Option<Cell> {
        // The FIRST row of the FIRST Paragraph section per side (PR
        // #284 review, round 2): later sections/rows belong to OTHER
        // paragraphs (the <pp> markers) — scanning them would blend
        // paragraph 2's bullet into paragraph 0's settings. Per-cell
        // instance -> master inheritance is preserved, then the
        // TEXT-STYLE stylesheet cascade follows — the same walk
        // `char_cell` gives Character cells (local CodeRabbit round
        // 3: a text style's IndLeft/Bullet must reach paragraphs the
        // same way its Font/Size reach characters). INHERITED cells
        // (F="Inh") never satisfy a level — their cached V is a
        // snapshot that can be stale against the parent, so the walk
        // skips them exactly like `resolve_cell` does (Greptile PR
        // #284 round 7) and the live parent value wins.
        let first_cell = |sheet: &Sheet| {
            sheet
                .sections_named("Paragraph")
                .first()
                .and_then(|sec| sec.rows.first())
                .and_then(|r| r.cell(name))
                .filter(|c| !c.is_inherited())
                .cloned()
        };
        if let Some(c) = first_cell(&s.sheet) {
            return Some(c);
        }
        let source = doc.instance_source(s, chain);
        if let Some(src) = source {
            if let Some(c) = first_cell(&src.sheet) {
                return Some(c);
            }
        }
        let style_id = s
            .text_style
            .or_else(|| source.and_then(|src| src.text_style))?;
        let mut cur = doc.stylesheet(style_id)?;
        for _ in 0..8 {
            if let Some(c) = first_cell(&cur.sheet) {
                return Some(c);
            }
            let next_id = cur
                .attrs
                .iter()
                .find(|(k, _)| k == "TextStyle")
                .and_then(|(_, v)| v.parse::<i64>().ok())
                .or_else(|| {
                    cur.sheet
                        .cell("TextStyle")
                        .and_then(|c| c.numeric())
                        .map(|v| v as i64)
                })?;
            if next_id == cur.id {
                return None;
            }
            cur = doc.stylesheet(next_id)?;
        }
        None
    };
    let ind_first = para_cell("IndFirst")
        .and_then(|c| c.numeric())
        .unwrap_or(0.0);
    let ind_left = para_cell("IndLeft")
        .and_then(|c| c.numeric())
        .unwrap_or(0.0);

    // #231 bullet machinery: a Bullet cell 1–7 (Word's round set) or a
    // non-numeric string (custom bullet) arms it; 0/empty stays off.
    let bullet = para_cell("Bullet").and_then(|b| {
        let v = b.v.as_deref().unwrap_or("").trim().to_string();
        if v.is_empty() || v == "0" {
            return None;
        }
        // A custom BulletString takes PRECEDENCE over the numeric
        // style ([MS-VSDX], PR #284 review round 3): the string
        // can ride along a nonzero numeric Bullet, and the round
        // set must not swallow it.
        let custom = para_cell("BulletString")
            .and_then(|c| c.v.clone())
            .map(|st| st.trim().to_string())
            .filter(|st| !st.is_empty());
        // Numeric forms classify BEFORE the custom-string
        // fallback (CodeRabbit local review): "256" overflows u8
        // and "1.0"/"0.0" fail an integer parse — either way the
        // old path painted the literal as a custom bullet.
        // Integral 1–7 → the round set; any other NUMERIC value
        // (final CodeRabbit pass: including negatives like "-1"
        // and fractionals like "1.5" — a number that failed the
        // style filter is an unsupported style, not custom text)
        // → no bullet; genuinely non-numeric text → a custom
        // bullet.
        let parsed = v.parse::<f64>().ok();
        let glyph = match (custom, parsed) {
            (Some(st), _) => BulletGlyph::Str(st),
            (None, Some(n)) if n.fract() == 0.0 && (1.0..=7.0).contains(&n) => {
                BulletGlyph::Round(n as u8)
            }
            (None, Some(_)) => return None, // unsupported numerics: no bullet
            (None, None) => BulletGlyph::Str(v),
        };
        // BulletFont: a family name, else inherit the paragraph
        // font (numeric legacy indexes and 0 both inherit).
        let font = para_cell("BulletFont")
            .and_then(|c| c.v.clone())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty() && v.parse::<f64>().is_err());
        // BulletFontSize: negative = percent of the text size
        // (default −100 %), ≥ 0 = absolute inches.
        let pct = para_cell("BulletFontSize")
            .and_then(|c| c.numeric())
            .unwrap_or(-100.0);
        let size = if pct < 0.0 {
            pct.abs() / 100.0 * font_size
        } else if pct == 0.0 {
            // 0 = the size of the first character after the bullet
            // ([MS-VSDX] BulletFontSize — PR #284 review round 3):
            // the paragraph's own size, not a 0.02 in sliver.
            font_size
        } else {
            pct.max(0.02)
        };
        let pos_after = para_cell("TextPosAfterBullet")
            .and_then(|c| c.numeric())
            .unwrap_or(0.0);
        // The tab-stop grid lives on the document sheet.
        let tab_stop = doc
            .sheet
            .cell("DefaultTabStop")
            .and_then(|c| c.numeric())
            .unwrap_or(0.5)
            .max(0.01);
        Some(BulletSpec {
            glyph,
            font,
            size,
            pos_after,
            tab_stop,
        })
    });

    // #229 TextBkgnd: 0 and 255 paint nothing; 1–24 select palette
    // entries 0–23; larger values index the document palette through
    // the +1 encoding; non-numeric values go through the shared color
    // mapping (theme formulas fall to the cached V, the usual
    // convention). TextBkgndTrans is a fraction like every *Trans
    // cell: the file stores 50 % as V='0.5' (Visio 2021 witness:
    // 0 / 0.5 / 0.8 paint opaque / half / mostly see-through), which
    // renders as 128/255 alpha.
    let text_bkgnd = doc.resolve_cell(s, chain, "TextBkgnd").and_then(|c| {
        let v = c.v.as_deref().unwrap_or("");
        let color = match v.trim().parse::<f64>() {
            Ok(n) => {
                let n = n as i64;
                if n <= 0 || n >= 255 {
                    return None;
                }
                doc.color(n - 1).cloned().unwrap_or_else(|| palette(n - 1))
            }
            Err(_) => color_value(doc, v, c.f.as_deref()),
        };
        let trans = doc
            .resolve_cell(s, chain, "TextBkgndTrans")
            .and_then(|c| c.numeric())
            .unwrap_or(0.0);
        let alpha = ((1.0 - trans).clamp(0.0, 1.0) * 255.0).round() as i64;
        if alpha <= 0 {
            None
        } else {
            Some((color, alpha.min(255) as u8))
        }
    });

    TextBlock {
        pin: (tx, ty),
        size: (tw, th),
        loc_pin: (
            get("TxtLocPinX").unwrap_or(tw / 2.0),
            get("TxtLocPinY").unwrap_or(th / 2.0),
        ),
        angle,
        insets: (ins_l, ins_r, ins_t, ins_b),
        line_height,
        font: char_font(doc, s, chain),
        font_size,
        color,
        bold: style_bits & 1 != 0,
        italic: style_bits & 2 != 0,
        halign,
        valign,
        cs_font,
        cs_size,
        pos,
        ind_first,
        ind_left,
        bullet,
        text_bkgnd,
    }
}

/// The greedy wrap both the renderer (`emit_text`) and the
/// `TEXTHEIGHT` measurement (#147) run — one wrap, so block-size
/// caches and the rendered lines can never disagree.
///
/// Visio stores line breaks as CRLF: breaking on each of \r and \n
/// separately would insert an empty paragraph between every pair of
/// lines, so normalize CRLF (and lone CR) to one break first.
/// Character-exact lines (#144): each paragraph is reconstructed
/// verbatim — `split(' ')` words joined back with their separator
/// spaces — so a paragraph-leading space run (empty first words)
/// survives into the first line, interior runs stay, and a
/// whitespace-only paragraph emits its spaces instead of an empty
/// line. `started` distinguishes "no words consumed yet" from "an
/// empty word consumed" (a pending leading space): a bare
/// `cur.is_empty()` test cannot tell them apart, which is how the
/// leading run used to vanish.
/// One wrapped line plus its paragraph's base direction (#257
/// text-direction): alignment and emission order follow the base, so
/// the wrap must carry it per line (a block can mix LTR and RTL
/// paragraphs). `first_of_para` tags each paragraph's opening line —
/// the #231 bullet and the IndFirst indent ride it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct WrapLine {
    pub text: String,
    pub rtl: bool,
    pub first_of_para: bool,
}

/// The Right-to-Left Text toggle's transform (#257 text-direction,
/// PR #269 review): `to_rtl` prepends an explicit U+202B (RLE) to
/// every paragraph; the reverse strips the leading direction marks
/// and then pins paragraphs whose first-strong scan still reads RTL
/// with an explicit U+202A (LRE) — "off" must really read LTR even
/// on naturally-RTL text, or the toggle is a visual no-op for it.
/// CRLF input normalizes to LF like the renderer's own wrap.
pub fn toggle_rtl_marks(text: &str, to_rtl: bool) -> String {
    let paras = text.replace("\r\n", "\n").replace('\r', "\n");
    // A text with NO content at all still takes the mark — it
    // PRE-SETS the direction for the first paragraph typed into an
    // empty shape (PR #269 review); only the trailing empty of a
    // NON-empty text stays bare.
    let pre_empty = paras.is_empty();
    let mut out: Vec<String> = Vec::new();
    for l in paras.split('\n') {
        // Both directions REPLACE the leading mark — stacking an RLE
        // onto an existing LRE pin would leave two overrides.
        let stripped = l.trim_start_matches(['\u{202A}', '\u{202B}', '\u{202C}']);
        if stripped.is_empty() && !pre_empty {
            // An EMPTY paragraph takes no mark: a mark on the trailing
            // terminator's empty paragraph is content to the wrapper
            // (a direction mark is not whitespace, so the renderer's
            // `trim_end` keeps it) and "hello\r\n" would gain a second
            // TEXTHEIGHT line — the corpus's trailing-break-adds-no-
            // line gold (#148) must survive a direction toggle (PR
            // #269 review).
            out.push(String::new());
        } else if to_rtl {
            out.push(format!("\u{202B}{stripped}"));
        } else {
            out.push(if paragraph_base_rtl(stripped) {
                format!("\u{202A}{stripped}")
            } else {
                stripped.to_string()
            });
        }
    }
    out.join("\n")
}

/// A paragraph's base direction (Analysis/rtl-text-direction.md):
/// explicit U+202A/U+202B overrides first, then the first strong
/// bidi character (R/AL → RTL, L → LTR, neutrals and numbers
/// skipped); no strong character → LTR. Public for dc-ui's
/// Right-to-Left Text toggle, which needs the renderer's exact rule
/// when it pins a stripped paragraph LTR (PR #269 review).
pub fn paragraph_base_rtl(para: &str) -> bool {
    let mut chars = para.chars().peekable();
    // Leading explicit embedding marks decide outright.
    while let Some(&c) = chars.peek() {
        match c {
            '\u{202B}' => return true,  // RLE
            '\u{202A}' => return false, // LRE
            '\u{200E}' | '\u{200F}' | '\u{202C}' => {
                chars.next();
            }
            _ => break,
        }
    }
    for c in chars {
        use unicode_bidi::BidiClass as BC;
        match unicode_bidi::bidi_class(c) {
            BC::R | BC::AL => return true,
            BC::L => return false,
            _ => {} // neutrals, EN/AN, marks: keep scanning
        }
    }
    false
}

/// The greedy wrap both the renderer (`emit_text`) and the
/// `TEXTHEIGHT` measurement (#147) run — one wrap, so block-size
/// caches and the rendered lines can never disagree.
///
/// Visio stores line breaks as CRLF: breaking on each of \r and \n
/// separately would insert an empty paragraph between every pair of
/// lines, so normalize CRLF (and lone CR) to one break first.
/// Character-exact lines (#144): each paragraph is reconstructed
/// verbatim — `split(' ')` words joined back with their separator
/// spaces — so a paragraph-leading space run (empty first words)
/// survives into the first line, interior runs stay, and a
/// whitespace-only paragraph emits its spaces instead of an empty
/// line. `started` distinguishes "no words consumed yet" from "an
/// empty word consumed" (a pending leading space): a bare
/// `cur.is_empty()` test cannot tell them apart, which is how the
/// leading run used to vanish. Since #282 the "words" are
/// `textmetrics::wrap_tokens` tokens — the same space-separated
/// words plus CJK break opportunities (see `wrap_block_text`).
/// One wrapped line plus its paragraph's base direction (#257
/// paragraph paints with — an RTL-base paragraph resolves the
/// complex-script cells (`cs_face`/`cs_size`, the Latin pair when
/// absent), so the wrap decision and the raster agree by construction
/// per paragraph, not just per block.
///
/// #282 zh-Hans: BOTH directions measure run-by-run through
/// `fb_chain` — usvg's per-cluster fallback paints a run the primary
/// face lacks with the first chain face covering it (Vazirmatn for
/// the Arabic script, Noto Sans SC for Han), so an LTR paragraph
/// carrying CJK must measure those runs with the same advances or
/// the wrap and the raster disagree. Latin the primary face covers
/// never splits (the chain fast path keeps those widths
/// byte-identical). The wrap itself tokenizes through
/// `textmetrics::wrap_tokens` — spaces as before, plus break
/// opportunities between ideographs under the kinsoku rules.
///
/// #231 paragraph indents: a paragraph's FIRST line wraps at
/// `usable_w − first_off` (the after-bullet tab offset for bulleted
/// paragraphs, `max(IndFirst, 0)` otherwise) and its wrapped lines at
/// `usable_w − left_off` (IndLeft). #235 terminal-NBSP hang: a
/// trailing no-break space never wraps its word — the overflow checks
/// measure the line without the terminal NBSP's advance while the
/// NBSP stays in the line's character-exact content.
#[allow(clippy::too_many_arguments)] // the pair faces/sizes + the chain + the two indent offsets
pub(crate) fn wrap_block_text(
    face: &crate::textmetrics::ResolvedFace,
    text: &str,
    size: f64,
    usable_w: f64,
    cs_face: &crate::textmetrics::ResolvedFace,
    cs_size: f64,
    fb_chain: &[&crate::textmetrics::ResolvedFace],
    first_off: f64,
    left_off: f64,
) -> Vec<WrapLine> {
    wrap_block_text_scaled(
        face, text, size, usable_w, cs_face, cs_size, fb_chain, first_off, left_off, None,
    )
}

/// Character runs for the wrap (see [`wrap_block_text_scaled`]): per
/// character of the text, the run's size over the block's and the
/// run's face (`None` = the block face).
pub(crate) struct WrapRuns {
    pub factor: Vec<f64>,
    pub face: Vec<Option<usize>>,
    pub faces: Vec<crate::textmetrics::ResolvedFace>,
}

/// [`wrap_block_text`] with character runs. An LTR paragraph that
/// carries runs measures each stretch of equal size and face with that
/// run's face at that run's size, so a label whose tail is set smaller
/// fits where Visio fits it (the network LLD's P2MP labels: 10 pt +
/// 8 pt measure 3.55 in against a 3.58 in block; measured all at 10 pt
/// they wrapped), a paragraph set wholly in another size wraps at that
/// size, and a bold run measures bold (Greptile on PR #350). Without
/// runs, or in a paragraph where every character keeps the block's
/// size and face, the measurement is exactly as before.
#[allow(clippy::too_many_arguments)] // wrap_block_text's arguments + the runs
pub(crate) fn wrap_block_text_scaled(
    face: &crate::textmetrics::ResolvedFace,
    text: &str,
    size: f64,
    usable_w: f64,
    cs_face: &crate::textmetrics::ResolvedFace,
    cs_size: f64,
    fb_chain: &[&crate::textmetrics::ResolvedFace],
    first_off: f64,
    left_off: f64,
    runs: Option<&WrapRuns>,
) -> Vec<WrapLine> {
    let mut lines: Vec<WrapLine> = Vec::new();
    // CRLF / lone CR normalize to one break; the run keys follow the
    // characters they belong to.
    let mut norm = String::with_capacity(text.len());
    let mut keys: Vec<(f64, Option<usize>)> = Vec::new();
    let mut chars = text.chars().enumerate().peekable();
    while let Some((i, c)) = chars.next() {
        let key = runs.map_or((1.0, None), |r| {
            (
                r.factor.get(i).copied().unwrap_or(1.0),
                r.face.get(i).copied().flatten(),
            )
        });
        if c == '\r' {
            if chars.peek().is_some_and(|&(_, n)| n == '\n') {
                continue;
            }
            norm.push('\n');
        } else {
            norm.push(c);
        }
        keys.push(key);
    }
    let same =
        |a: (f64, Option<usize>), b: (f64, Option<usize>)| (a.0 - b.0).abs() <= 1e-9 && a.1 == b.1;
    let mut para_off = 0usize;
    for para in norm.split('\n') {
        let para_len = para.chars().count();
        let para_keys = &keys[para_off.min(keys.len())..(para_off + para_len).min(keys.len())];
        para_off += para_len + 1;
        let styled = runs.is_some() && para_keys.iter().any(|k| !same(*k, (1.0, None)));
        let para_chars: Vec<char> = if styled {
            para.chars().collect()
        } else {
            Vec::new()
        };
        // Where the current line starts its search: lines are exact,
        // in-order substrings of the paragraph.
        let cursor = std::cell::Cell::new(0usize);
        let locate = |s: &str| -> Option<usize> {
            let sc: Vec<char> = s.chars().collect();
            (cursor.get()..=para_chars.len().checked_sub(sc.len())?)
                .find(|&k| para_chars[k..k + sc.len()] == sc[..])
        };
        let advance = |s: &str| {
            if styled {
                if let Some(p) = locate(s) {
                    cursor.set(p + s.chars().count());
                }
            }
        };
        let rtl = paragraph_base_rtl(para);
        // Run-by-run measurement with the faces the raster paints
        // with (PR #269 review for RTL; #282 extended it to LTR for
        // the CJK runs): the paragraph's own face/size pair over the
        // shared fallback chain. A pure-script paragraph the primary
        // face covers measures as one run through the same single-
        // face path as before.
        let measure = |k: (f64, Option<usize>), seg: &str| {
            let f =
                k.1.and_then(|i| runs.and_then(|r| r.faces.get(i)))
                    .unwrap_or(face);
            f.width_in_runs(fb_chain, seg, size * k.0)
        };
        let width_in = |s: &str| {
            if s.is_empty() {
                // Nothing to measure — and no position to look up: an
                // emitted whole token leaves the cursor at the paragraph
                // end (Greptile on PR #350).
                0.0
            } else if rtl {
                cs_face.width_in_runs(fb_chain, s, cs_size)
            } else if let Some(p) = styled.then(|| locate(s)).flatten() {
                // Equal-key stretches, each with its own face and size.
                let mut total = 0.0;
                let mut seg = String::new();
                let mut seg_k = para_keys[p];
                for (k, c) in s.chars().enumerate() {
                    let key = para_keys[p + k];
                    if !same(key, seg_k) {
                        total += measure(seg_k, &seg);
                        seg.clear();
                        seg_k = key;
                    }
                    seg.push(c);
                }
                total + measure(seg_k, &seg)
            } else {
                face.width_in_runs(fb_chain, s, size)
            }
        };
        // #235: the hang measure — a terminal no-break space's
        // advance is excluded (it paints past the margin); anything
        // else measures whole.
        fn hang_trim(s: &str) -> &str {
            s.strip_suffix('\u{a0}').unwrap_or(s)
        }
        // Per-line usable width: the first line takes first_off (the
        // bullet tab offset / first-line indent), wrapped lines take
        // left_off (the hanging indent) — #231.
        let mut wrapped = false;
        let line_usable = |wrapped: bool| usable_w - if wrapped { left_off } else { first_off };
        let mut cur = String::new();
        let mut started = false;
        // #282 kinsoku: the emergency splitter can emit a whole
        // token as its own (overflowing) line, leaving `cur` empty —
        // the paragraph's closing push must not then add a phantom
        // EMPTY line (it would inflate TEXTHEIGHT by a slot). An
        // empty paragraph still emits its one empty line (below).
        let mut para_has_lines = false;
        for token in crate::textmetrics::wrap_tokens(para) {
            let candidate = if started {
                format!("{cur}{}{}", token.sep, token.tok)
            } else {
                token.tok.to_string()
            };
            let usable = line_usable(wrapped);
            if started && width_in(hang_trim(&candidate)) > usable {
                if cur.is_empty() {
                    // The break lands on a pending separator space (an
                    // authored space run or a leading indent too wide
                    // to share a line with the word): the space is
                    // content (#144 review) — it becomes a line of its
                    // own, never an empty line, never dropped. An
                    // EMPTY separator (a CJK boundary) follows a token
                    // the hard split below already emitted whole —
                    // pushing it would add a phantom empty line
                    // (`图，`, ``, `上`, `色。`) (CodeRabbit PR
                    // review).
                    if !token.sep.is_empty() {
                        advance(token.sep);
                        lines.push(WrapLine {
                            text: token.sep.to_string(),
                            rtl,
                            first_of_para: !wrapped,
                        });
                        para_has_lines = true;
                    }
                } else {
                    advance(&cur);
                    lines.push(WrapLine {
                        text: std::mem::take(&mut cur),
                        rtl,
                        first_of_para: !wrapped,
                    });
                    para_has_lines = true;
                }
                cur = token.tok.to_string();
                wrapped = true;
            } else {
                cur = candidate;
            }
            started = true;
            // A single token wider than the line: hard-split at the
            // longest character prefix that fits (always ≥1 char, so
            // the loop makes progress). #282 kinsoku: the split never
            // leaves a line-head-forbidden char to OPEN the rest —
            // the kept prefix extends over consecutive forbidden
            // chars (oikomi-style; the token may then overflow its
            // band rather than break before 。，）… and a token that
            // is all forbidden-headed tail emits whole).
            let mut usable = line_usable(wrapped);
            while width_in(hang_trim(&cur)) > usable && cur.chars().count() > 1 {
                let mut split_at = cur
                    .char_indices()
                    .nth(1)
                    .map(|(i, _)| i)
                    .unwrap_or(cur.len());
                for (i, _) in cur.char_indices().skip(1) {
                    if width_in(hang_trim(&cur[..i])) <= usable {
                        split_at = i;
                    } else {
                        break;
                    }
                }
                // The boundary rules run to a FIXPOINT, not in a
                // fixed order: glue (U+00A0 and friends — a boundary
                // adjacent to glue is never a grapheme edge, keeping
                // `图⍽表` intact even when the joined token then
                // overflows its line, CodeRabbit PR review) can move
                // the split PAST the position the forbidden-head
                // pass already cleared, stranding a closer at the
                // head of the rest (`表⍽，！` must not open with `！`,
                // CodeRabbit/Greptile round 5). A combining mark at
                // the boundary attaches to the kept prefix — the
                // emergency splitter never splits `カ` from its
                // dakuten (CodeRabbit round 5) — WITHOUT consuming
                // the following base, so a normal break before `ク`
                // stays available.
                loop {
                    let mut moved = false;
                    while split_at < cur.len() {
                        match cur[split_at..].chars().next() {
                            Some(c) if crate::textmetrics::line_head_forbidden(c) => {
                                split_at += c.len_utf8();
                                moved = true;
                            }
                            _ => break,
                        }
                    }
                    if split_at < cur.len() {
                        let prev_is_glue = cur[..split_at]
                            .chars()
                            .next_back()
                            .is_some_and(crate::textmetrics::is_glue_pub);
                        let next_is_glue = cur[split_at..]
                            .chars()
                            .next()
                            .is_some_and(crate::textmetrics::is_glue_pub);
                        if prev_is_glue || next_is_glue {
                            split_at += cur[split_at..]
                                .chars()
                                .next()
                                .map_or(1, char::len_utf8);
                            moved = true;
                        }
                        if cur[split_at..]
                            .chars()
                            .next()
                            .is_some_and(crate::textmetrics::is_combining_pub)
                        {
                            split_at += cur[split_at..]
                                .chars()
                                .next()
                                .map_or(1, char::len_utf8);
                            moved = true;
                        }
                    }
                    if !moved {
                        break;
                    }
                }
                let rest = cur.split_off(split_at);
                advance(&cur);
                lines.push(WrapLine {
                    text: std::mem::replace(&mut cur, rest),
                    rtl,
                    first_of_para: !wrapped,
                });
                para_has_lines = true;
                wrapped = true;
                usable = line_usable(wrapped);
            }
        }
        if !cur.is_empty() || !para_has_lines {
            lines.push(WrapLine {
                text: cur,
                rtl,
                first_of_para: !wrapped,
            });
        }
    }
    lines
}

#[allow(clippy::too_many_arguments)] // placement + flip axes + the #22 skip + #124 greek ctx + #267 fields
fn emit_text(
    doc: &Document,
    s: &Shape,
    chain: &[&Shape],
    width: f64,
    height: f64,
    sx: f64,
    sy: f64,
    skip: bool,
    greek: Option<&GreekCtx>,
    fields: Option<&crate::fields::FieldsCtx<'_>>,
    out: &mut String,
) {
    if skip {
        return;
    }
    // Authored leading spacing is content (#144): trim only the END —
    // the first paragraph's leading space run must survive into its
    // line. The trailing trim is now GOLD-BACKED (#148, 2026-10-30):
    // the shipped corpus carries Visio's own cached
    // `TEXTHEIGHT(TheText,TxtWidth)` values, and 25 one-visual-line
    // texts ending in the corpus-standard CRLF (DBUML_M/U "PK\r\n",
    // TITLE_M/U "\r\n", ELETEL_M, OFFEQP…) cache exactly ONE line
    // height (1.2003–1.2004 em at the canonical 120% spacing) — Visio
    // does NOT count the trailing break as a line, so dropping it
    // here keeps the block's line count and vertical centering where
    // Visio puts them. The trim set is explicit breaks/spaces/tab
    // (#235): a trailing NO-BREAK SPACE is content — it composes and
    // hangs past the margin in Visio, so the blanket `str::trim_end`
    // (whose whitespace set includes U+00A0) must not eat it.
    const TRAILING_TRIM: [char; 4] = ['\r', '\n', ' ', '\t'];
    // #267: a `<fld>`-bearing text renders its SUBSTITUTED string —
    // evaluated, picture-formatted, laid out. The fast path returns
    // None for the marker-free shape, keeping its plain text (and
    // the marker-free packages) byte-identical.
    let substituted = crate::fields::substituted_text(doc, s, chain, fields);
    let Some(text) = substituted
        .as_deref()
        .or(s.sheet.text.as_deref())
        .map(|t| t.trim_end_matches(TRAILING_TRIM))
        .filter(|t| !t.is_empty())
    else {
        return;
    };
    let tb = resolve_text_block(doc, s, chain, width, height);
    // Character runs (`<cp IX='n'/>` markers → Character rows) and the
    // layout text: U+2028 is a line break, a tab advances like four
    // spaces (Visio 2021 witness c03 — tab stops are not modeled).
    // Blocks without differing runs keep their exact former output.
    let runs = if substituted.is_none() {
        text_runs(doc, s, chain, &tb)
    } else {
        None
    };
    let layout_text: String;
    let text = if text.contains(['\u{2028}', '\t']) {
        layout_text = text.replace('\u{2028}', "\n").replace('\t', "    ");
        layout_text.as_str()
    } else {
        text
    };
    let (tx, ty) = tb.pin;
    let (tw, th) = tb.size;
    let angle = tb.angle;
    let size = tb.font_size;
    let color = tb.color.clone();
    let style_bits = (tb.bold as u32) | ((tb.italic as u32) << 1);
    // Character.Pos (#230): the measured reduced em + baseline shift
    // for the block's family (None = unwitnessed family → nominal
    // draw). The paint size shrinks; the LINE slots and the baseline
    // ascent math stay nominal — Visio's TEXTHEIGHT retains the
    // nominal size and the shift rides the nominal baseline.
    // Character.Pos metrics come from the family that PAINTS each
    // direction (CodeRabbit local review): LTR lines paint the Latin
    // font, RTL lines the complex-script font (or the Latin chain
    // when no CS font resolves) — an Arial block with a Times CS
    // font must script with Times's numbers on RTL lines, and an
    // unwitnessed CS family keeps the nominal draw there.
    let script_latin = script_metrics(tb.font.as_deref(), tb.pos);
    let script_l = script_latin;
    let reduce = script_latin.map_or(1.0, |(r, _)| r);
    let paint_size = size * reduce;
    let font_attrs = format!(
        r#"font-size="{s:.4}"{ff} fill="{c}"{b}{i}"#,
        s = paint_size,
        ff = font_family_attr(tb.font.as_deref()),
        // Document-supplied color text (e.g. a `#…` value) is attribute
        // content — escape it like every other attribute.
        c = escape_attr(&color),
        b = if style_bits & 1 != 0 {
            r#" font-weight="bold""#
        } else {
            ""
        },
        i = if style_bits & 2 != 0 {
            r#" font-style="italic""#
        } else {
            ""
        },
    );
    let halign = tb.halign;
    let valign = tb.valign;
    let (ins_l, ins_r, ins_t, ins_b) = tb.insets;
    let line_h = tb.line_height;

    // Greedy word wrap against REAL font metrics (GitHub #21): runs
    // are measured as the sum of per-glyph advances from the face the
    // rasterizer will draw (system font where installed, vendored
    // Carlito otherwise — `textmetrics`), replacing the ~0.5 em
    // average-advance guess that re-wrapped lines the author had
    // fitted to the block.
    let usable_w = (tw - ins_l - ins_r).max(0.02);
    let face = crate::textmetrics::resolve(tb.font.as_deref(), tb.bold, tb.italic);
    // The complex-script pair (#257 text-direction): an RTL-base
    // paragraph measures and paints with ComplexScriptFont/Size —
    // falling back to the Latin chain/size per the spec's zero
    // semantics — so wrap and raster agree per paragraph. The vendored
    // Vazirmatn (dc::register_complex_script_faces) covers the glyphs when the
    // document names no CS face.
    let cs_chain = tb.cs_font.as_deref().or(tb.font.as_deref());
    let cs_face = crate::textmetrics::resolve(cs_chain, tb.bold, tb.italic);
    // The per-cluster fallback CHAIN (#282 zh-Hans grew the #257 pair
    // face to a chain): the vendored Vazirmatn for the Arabic script,
    // then the vendored CJK faces in `dc::CJK_FACE_ORDER` — SC for the
    // Han glyphs the Latin and Arabic faces lack, JP for the kana/JIS
    // Kanji the SC subset lacks, KR for Hangul — the same order
    // `dc::register_complex_script_faces` loads them into the raster
    // fontdb, which is the order usvg's per-cluster fallback picks a
    // face (see the wrap's probe note). Unregistered (the dc unit-test
    // binaries), every entry resolves like the Latin default and the
    // probe never fires.
    let mut fallback_faces = Vec::new();
    fallback_faces.push(crate::textmetrics::resolve(Some("Vazirmatn"), tb.bold, tb.italic));
    for family in crate::CJK_FACE_ORDER {
        fallback_faces.push(crate::textmetrics::resolve(Some(family), tb.bold, tb.italic));
    }
    let fb_chain: Vec<&crate::textmetrics::ResolvedFace> = fallback_faces.iter().collect();
    let cs_size = tb.cs_size.unwrap_or(size);
    let script_cs = script_metrics(cs_chain, tb.pos);
    let script_r = script_cs;
    let cs_paint_size = cs_size * script_cs.map_or(1.0, |(r, _)| r);

    // Line-band edges before the wrap — the #231 bullet layout needs
    // them for its tab-stop arithmetic (the first line's text starts
    // at the stop, so the wrap must reserve that offset).
    let left_x = tx - tb.loc_pin.0 + ins_l;
    let right_x = left_x + (tw - ins_l - ins_r);
    // #231 bullet layout: the bullet's own advance, its line-start
    // position, and the tab stop the after-bullet text snaps to. The
    // bullet itself draws at bullet_x; the measured placement rule:
    // TextPosAfterBullet when it lies past the bullet's start, else
    // the hanging indent (IndLeft when IndFirst is negative) or the
    // next DefaultTabStop multiple — whichever comes first, never
    // before the bullet's end.
    struct BulletLayout {
        spec: crate::svg::BulletSpec,
        x: f64,
        text_x: f64,
    }
    let bullet_layout = tb.bullet.as_ref().map(|b| {
        // Positive IndFirst indents the bullet; negative outdents it
        // (the hanging-indent case) past the margin. The stop rule
        // lives in `bullet_stop_offset_in` — shared with the
        // TEXTHEIGHT measure, which wraps at the same first-line
        // offset (PR #284 review) — and returns a TEXT-AREA offset in
        // INCHES; this function emits in the same inch space.
        let bullet_off = tb.ind_first;
        let stop_off = bullet_stop_offset_in(&tb).unwrap_or(bullet_off);
        BulletLayout {
            spec: b.clone(),
            x: left_x + bullet_off,
            text_x: left_x + stop_off,
        }
    });
    // Paragraph indents for the wrap: a bulleted paragraph's first
    // line starts at the tab stop; otherwise at max(IndFirst, 0).
    // Wrapped lines take IndLeft (#231). RTL paragraphs carry the same
    // offsets (unwitnessed — #231's open questions).
    let (first_off, left_off) = match &bullet_layout {
        Some(bl) => ((bl.text_x - left_x).max(0.0), tb.ind_left),
        // The unbulleted first line reserves the COMBINED indent
        // (IndLeft base + IndFirst adjustment — PR #284 review round
        // 4): the paint band and the TEXTHEIGHT measure already use
        // `unbulleted_first_offset_in`; wrapping at less would let
        // text run past the right margin and split measured line
        // counts from laid-out ones.
        None => (unbulleted_first_offset_in(&tb), tb.ind_left),
    };
    // Greeking (#124 item 2): a block whose on-screen em height falls
    // under TEXT_GREEK_PX renders as gray bars — per line, shaped only
    // at paragraph granularity (one width_in per paragraph, no greedy
    // word wrap and no <text> node for usvg to turn into glyph
    // outlines). Every met size is recorded regardless, so the layer
    // cache can tell when a ppi-bucket change flips the decision.
    let greeked = greek.is_some_and(|g| {
        g.sizes.borrow_mut().push(size);
        g.greeks(size)
    });
    let (lines, bar_widths) = if greeked {
        let mut bars = Vec::new();
        for para in text.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
            let w = face.width_in_runs(&fb_chain, para, size);
            if w <= usable_w {
                bars.push(w.max(usable_w * 0.2));
            } else {
                // Chunk the paragraph across an estimated wrap: full
                // bars of the usable width plus one remainder bar.
                let full = (w / usable_w).floor() as usize; // ≥ 1: w > usable_w
                let rem = w - full as f64 * usable_w;
                bars.extend(std::iter::repeat_n(usable_w, full));
                if rem > usable_w * 0.05 {
                    bars.push(rem);
                }
            }
        }
        (Vec::new(), bars)
    } else {
        // Character runs measure with their own size and face (the
        // raster paints them so).
        let wrap_runs: Option<WrapRuns> = runs.as_ref().map(|r| {
            let mut faces: Vec<crate::textmetrics::ResolvedFace> = Vec::new();
            let mut face_of: Vec<Option<usize>> = Vec::new();
            for st in &r.styles {
                let (b, i) = (st.bold.unwrap_or(tb.bold), st.italic.unwrap_or(tb.italic));
                face_of.push(if (b, i) == (tb.bold, tb.italic) {
                    None
                } else {
                    faces.push(crate::textmetrics::resolve(tb.font.as_deref(), b, i));
                    Some(faces.len() - 1)
                });
            }
            let n = text.chars().count();
            WrapRuns {
                factor: (0..n)
                    .map(|i| {
                        r.styles
                            .get(r.style_at(i))
                            .and_then(|st| st.size)
                            .filter(|z| *z > 0.0 && size > 0.0)
                            .map_or(1.0, |z| z / size)
                    })
                    .collect(),
                face: (0..n)
                    .map(|i| face_of.get(r.style_at(i)).copied().flatten())
                    .collect(),
                faces,
            }
        });
        (
            wrap_block_text_scaled(
                &face,
                text,
                paint_size,
                usable_w,
                &cs_face,
                cs_paint_size,
                &fb_chain,
                first_off,
                left_off,
                wrap_runs.as_ref(),
            ),
            Vec::new(),
        )
    };
    let n_lines = if greeked {
        bar_widths.len()
    } else {
        lines.len()
    } as f64;
    // Per-line slots (PR #269 review): an RTL line under a bigger
    // ComplexScriptSize needs a proportionally taller slot — the flat
    // n × line_h overlapped larger glyphs onto their neighbors, and
    // TEXTHEIGHT (textfunc) measured the same too-small block. Slots
    // stay NOMINAL under Character.Pos (#230: TEXTHEIGHT retains the
    // nominal size).
    // Each wrapped line's character range in the layout text (lines are
    // exact substrings, in order; the separators a break consumed are
    // skipped) — only needed when the block carries runs.
    let line_ranges: Vec<Option<(usize, usize)>> = if runs.is_some() {
        let chars: Vec<char> = text.chars().collect();
        let mut cursor = 0usize;
        lines
            .iter()
            .map(|l| {
                let lc: Vec<char> = l.text.chars().collect();
                let found = (cursor..=chars.len().saturating_sub(lc.len()))
                    .find(|&k| chars[k..k + lc.len()] == lc[..])?;
                cursor = found + lc.len();
                Some((found, found + lc.len()))
            })
            .collect()
    } else {
        Vec::new()
    };
    // A run line's segments: each style stretch with its face, paint
    // size and width — the one measure for placement, alignment and the
    // #229 background (Greptile on PR #350: the block-style width
    // misplaced centered and right-aligned mixed-size lines).
    let run_segs = |r: &TextRuns, content: &str, lo: usize| {
        let chars: Vec<char> = content.chars().collect();
        let mut segs: Vec<(String, usize, f64, f64)> = Vec::new();
        let mut i = 0;
        while i < chars.len() {
            let st = r.style_at(lo + i);
            let mut j = i + 1;
            while j < chars.len() && r.style_at(lo + j) == st {
                j += 1;
            }
            let seg: String = chars[i..j].iter().collect();
            let rs = &r.styles[st];
            let fs = rs.size.map_or(paint_size, |z| z * reduce);
            let seg_face = crate::textmetrics::resolve(
                tb.font.as_deref(),
                rs.bold.unwrap_or(tb.bold),
                rs.italic.unwrap_or(tb.italic),
            );
            let w = seg_face.width_in_runs(&fb_chain, &seg, fs);
            segs.push((seg, st, fs, w));
            i = j;
        }
        segs
    };
    // A run line's height factor: its largest run over the block size,
    // never below the block's own (Greptile on PR #350: a 30 pt run in
    // a 10 pt block overlapped its neighbours at 12 pt spacing).
    let run_scale = |line_no: usize| -> f64 {
        match (&runs, line_ranges.get(line_no).copied().flatten()) {
            (Some(r), Some((lo, hi))) if !lines[line_no].rtl && size > 0.0 => (lo..hi)
                .map(|i| r.styles[r.style_at(i)].size.map_or(1.0, |z| z / size))
                .fold(1.0, f64::max),
            _ => 1.0,
        }
    };
    let slots: Vec<f64> = if greeked {
        vec![line_h; n_lines as usize]
    } else {
        lines
            .iter()
            .enumerate()
            .map(|(i, l)| line_slot_height(&tb, l.rtl) * run_scale(i))
            .collect()
    };
    let total_h: f64 = slots.iter().sum();
    // The first line's ascent rides its own effective size (nominal
    // under Pos — the shift applies to the baseline, not the slot).
    let first_size = lines.first().map_or(size, |l| {
        if l.rtl {
            effective_cs_size(&tb)
        } else {
            size * run_scale(0)
        }
    });

    // Text-block rect from the sheet: the pin (TxtPinX/TxtPinY, y-up local)
    // sits TxtLocPinX/TxtLocPinY from the block's lower-left corner, so that
    // corner is (tx - lpx, ty - lpy) — masters can pin off-center, so no
    // centered-pin assumption. In the emitted y-negated frame the top edge
    // is -(ty - lpy + th).
    let lpy = tb.loc_pin.1;
    let top_edge = -(ty - lpy + th);
    let first_baseline = top_edge + text_first_baseline(&tb, total_h, first_size);
    let _ = (valign, ins_t, ins_b);
    // Line x per alignment: left edge of the text area, center, or right.
    // #257 text-direction: an RTL-base line reads 0/2 as start/end
    // relative to its READING order (Visio's RTL editions), so the
    // visual left edge moves with the line's base. The anchor stays
    // "start" (usvg lays glyphs from the visual left under its
    // hardcoded LTR base); the x is the line's computed visual left.
    let (anchor, x) = match halign {
        0 | 3 => ("start", left_x),
        2 => ("end", right_x),
        _ => ("middle", left_x + (tw - ins_l - ins_r) / 2.0),
    };

    let mut body = String::new();
    // #231 path bullets live OUTSIDE the <text> element (PR #284
    // review: SVG's content model allows only character data and
    // tspan/textPath inside text — usvg drops a <path> there, so the
    // built-in bullets reserved their gap but painted nothing). They
    // are emitted just before the <text>, inside the same transform
    // group; custom-string bullets stay in `body` (tspans are legal).
    let mut bullet_paths = String::new();
    // Baselines ride each line's OWN ascent: the running coordinate is
    // the next line's slot TOP (the first baseline already sits
    // `first_size × 0.8` below the block's band top), and line i's
    // baseline is its slot top plus ITS effective ascent — a larger
    // complex-script line following a Latin one rises by the larger
    // ascent instead of overlapping it (PR #269 review; uniform for
    // greek bars, whose Latin ascent/slots make this byte-identical
    // to the old advance-by-slot form).
    let mut top = first_baseline - first_size * 0.8;
    // The #229 background rect spans the LINES, not the block: its y
    // starts at the first line's slot top (PR review, wave 4 — the
    // block top would hang the rect above middle/top-aligned text).
    // Saved before the emission loop advances `top`.
    let bkgnd_top = top;
    if greeked {
        // Greek bars (#124 item 2): one neutral-gray rect per line,
        // anchored like the tspan it replaces (left/center/right).
        // Height sits in the x-height band of the line's em box.
        for (w, slot) in bar_widths.iter().zip(&slots) {
            let (bx, bw) = match anchor {
                "start" => (x, *w),
                "end" => (x - *w, *w),
                _ => (x - *w / 2.0, *w),
            };
            let _ = write!(
                body,
                r##"<rect x="{bx:.4}" y="{y:.4}" width="{bw:.4}" height="{h:.4}" fill="#7f7f7f"/>"##,
                y = top + size * 0.8 - size * 0.72,
                h = size * 0.62,
            );
            top += slot;
        }
    }
    // #229 text-background extents: the union of the laid-out lines'
    // [left, right] spans — the rect spans the widest laid-out line
    // and the union of the line heights (margins position the text
    // but add NO background padding). Accumulated during emission;
    // the rect itself is written just before the <text> element
    // (above shape geometry, below text).
    let mut bkgnd_span: Option<(f64, f64)> = None;
    let mut any_rtl = false;
    let mut deco = String::new();
    for (line_no, (line, slot)) in lines.iter().zip(&slots).enumerate() {
        let range = line_ranges.get(line_no).copied().flatten();
        let (content, line_x, line_attrs, pre, y) = if line.rtl {
            any_rtl = true;
            // The RTL line paints with the complex-script pair; its
            // visual LEFT edge per the start/end mapping, computed
            // from the line's own shaped width. The face/size ride
            // the tspan — a block may mix direction lines and the
            // element-level attrs stay Latin. (Bullets and paragraph
            // indents stay unwitnessed on RTL lines — #231 open
            // questions — so RTL keeps the plain band placement.)
            let lw = cs_face.width_in_runs(&fb_chain, &line.text, cs_paint_size);
            let content = format!("\u{2067}{}\u{2069}", line.text);
            let vx = match halign {
                0 | 3 => right_x - lw, // start (reading order) = visual right edge
                2 => left_x,           // end (reading order) = visual left edge
                _ => x,                // center: the shared center
            };
            // PR #269 review: a right-aligned (halign 2) block anchors
            // the ELEMENT "end", but this line sits at the block's
            // visual LEFT edge — the inherited "end" would swing its
            // glyphs further left, outside the block. It anchors
            // itself "start" there. `font_family_attr` returns the
            // COMPLETE attribute (leading space included) — wrapping
            // it in another font-family="" nested the quotes and
            // emitted malformed XML.
            let attrs = format!(
                "{}{}",
                if halign == 2 {
                    r#" text-anchor="start""#
                } else {
                    ""
                },
                if (cs_paint_size - paint_size).abs() > 1e-9 {
                    format!(
                        r#" font-size="{cs_paint_size:.4}"{}"#,
                        font_family_attr(cs_chain)
                    )
                } else {
                    font_family_attr(cs_chain)
                }
            );
            let line_size = effective_cs_size(&tb);
            let y = top + line_size * 0.8 - script_r.map_or(0.0, |(_, shift)| shift * line_size);
            // The RTL line's laid-out extent joins the #229 background
            // union too (PR review, wave 4) — an all-RTL block must
            // still paint its text background. The extent follows the
            // line's EFFECTIVE anchor (CodeRabbit local review): a
            // centered RTL line anchors middle at vx, so its extent
            // runs half a line width to each side; start/end anchors
            // place vx at an edge.
            let span = match halign {
                // Centered: vx is the line's middle.
                1 => (vx - lw / 2.0, vx + lw / 2.0),
                _ => (vx, vx + lw),
            };
            bkgnd_span = Some(match bkgnd_span {
                Some((a, b)) => (a.min(span.0), b.max(span.1)),
                None => span,
            });
            (content, vx, attrs, String::new(), y)
        } else {
            // This line's baseline FIRST (the bullet glyph hangs off
            // it): its slot top plus ITS OWN effective ascent
            // (0.8 × effective size — the same factor
            // `text_first_baseline` gives the first line).
            // Character.Pos (#230) then shifts the baseline by the
            // measured em offset (positive = raised) — independent of
            // the reduced glyph em, so the shift base is the line's
            // NOMINAL size.
            let line_size = size * run_scale(line_no);
            let y = top + line_size * 0.8 - script_l.map_or(0.0, |(_, shift)| shift * line_size);
            // LTR line placement: every line lays out in an indented
            // band — a bulleted paragraph's first line carries
            // bullet+gap+text as ONE unit anchored at the tab stop
            // (left-anchored override there: the stop is an absolute
            // x), other first lines take IndFirst and wrapped lines
            // take IndLeft (#231, PR review wave 4: the wrap reserves
            // exactly these offsets, so the paint must honor them).
            // Under the inherited `end` anchor a tspan's x is where
            // the glyphs END — halign 2 keeps the anchor and derives
            // the left edge from the line width.
            // Measure through the same fallback chain the LTR wrap
            // used: the primary face gives a missing-glyph estimate
            // for CJK spans it lacks, mis-placing centered/right
            // lines, bullet units, and TextBkgnd spans (CodeRabbit
            // PR review). Latin-only lines keep their width — the
            // chain falls back to `width_in` when the face covers
            // the whole line.
            let line_w = match (&runs, range) {
                (Some(r), Some((lo, _))) => run_segs(r, &line.text, lo).iter().map(|g| g.3).sum(),
                _ => face.width_in_runs(&fb_chain, &line.text, paint_size),
            };
            let bulleted = line.first_of_para && bullet_layout.is_some();
            let band_off = if bulleted {
                // The unit math below places the line outright.
                0.0
            } else if line.first_of_para {
                unbulleted_first_offset_in(&tb)
            } else {
                tb.ind_left
            };
            let band_lo = left_x + band_off;
            let (lx, line_left) = if bulleted {
                let bl = bullet_layout.as_ref().unwrap();
                // Centered/right-aligned first lines align
                // bullet+gap+text as ONE unit: shift the whole
                // [bullet .. text] run so the unit sits in the band
                // (measured rule, #231).
                let unit_w = (bl.text_x - bl.x) + line_w;
                let unit_x = match halign {
                    // Left/justify: the bullet sits where the indents
                    // put it — no unit shift.
                    0 | 3 => bl.x,
                    2 => right_x - unit_w,
                    _ => left_x + ((tw - ins_l - ins_r) - unit_w) / 2.0,
                };
                (unit_x + (bl.text_x - bl.x), unit_x)
            } else {
                match halign {
                    0 | 3 => (band_lo, band_lo),
                    2 => (right_x, right_x - line_w),
                    // Centered: the anchor is the band's midpoint —
                    // the line's LEFT edge (what the #229 background
                    // spans) sits half a line width left of it (PR
                    // #284 review: recording band_lo painted the
                    // background at the margin, leaving connector
                    // lines visible through the centered text).
                    _ => {
                        let cx = (band_lo + right_x) / 2.0;
                        (cx, cx - line_w / 2.0)
                    }
                }
            };
            // The bullet glyph itself — path bullets for the built-in
            // round set (viewer-proof, #231), a text glyph for custom
            // strings. Only LTR first lines draw one (RTL bullets are
            // an open #231 question); the bullet centers slightly
            // above the baseline like a text glyph of its own size.
            let pre = if bulleted {
                let bl = bullet_layout.as_ref().unwrap();
                let by = y - 0.28 * bl.spec.size;
                // The bullet rides the unit shift: centered and
                // right-aligned first lines re-place the whole
                // [bullet .. text] run (PR #284 review — the text
                // moved via unit_x while the bullet stayed at the
                // unshifted bl.x, stranding it at the margin).
                let bullet_x = line_left;
                match &bl.spec.glyph {
                    // Paths go outside <text> (see `bullet_paths`).
                    BulletGlyph::Round(kind) => {
                        bullet_paths.push_str(&bullet_glyph_svg(
                            *kind,
                            bullet_x,
                            by,
                            bl.spec.size,
                            &color,
                        ));
                        String::new()
                    }
                    BulletGlyph::Str(glyph) => format!(
                        // The bullet tspan anchors ITSELF "start" at
                        // bullet_x (PR #284 review round 4): left
                        // open, it would inherit the element's
                        // middle/end anchor and shift the glyph left
                        // by half (or all) of its width away from the
                        // laid-out bullet position.
                        r#"<tspan x="{bx:.4}" y="{by:.4}" text-anchor="start" font-size="{fs:.4}"{ff}>{g}</tspan>"#,
                        bx = bullet_x,
                        by = y,
                        fs = bl.spec.size,
                        // BulletFont absent → the PARAGRAPH face paints
                        // the bullet — the same face the advance was
                        // measured with (PR review, wave 4): passing
                        // bare None here would paint the Calibri
                        // default chain over a Courier New paragraph.
                        ff = font_family_attr(bl.spec.font.as_deref().or(tb.font.as_deref())),
                        g = escape_text(glyph),
                    ),
                }
            } else {
                String::new()
            };
            // The laid-out line spans bullet .. gap .. text: a
            // bulleted first line's right edge is the TEXT's end (the
            // anchor), not bullet_x + width (PR #284 review, round 2 —
            // the gap and the tab offset were left outside the #229
            // background).
            let span = if bulleted {
                (line_left, lx + line_w)
            } else {
                (line_left, line_left + line_w)
            };
            bkgnd_span = Some(match bkgnd_span {
                Some((a, b)) => (a.min(span.0), b.max(span.1)),
                None => span,
            });
            let attrs = if bulleted {
                r#" text-anchor="start""#.to_string()
            } else {
                String::new()
            };
            (line.text.clone(), lx, attrs, pre, y)
        };
        let _ = write!(body, "{pre}");
        // A bulleted first line anchors "start" at its text x (its only
        // line attribute); it keeps its runs too (Greptile on PR #350).
        let run_line = match (&runs, range) {
            (Some(r), Some(rg)) if !line.rtl && content == line.text => Some((r, rg)),
            _ => None,
        };
        if let Some((r, (lo, _))) = run_line {
            // One positioned chunk per run (its own fill/weight even in
            // the PDF export, which paints a chunk in one style);
            // underline/strike-through as real lines after the text.
            let segs = run_segs(r, &content, lo);
            let line_w: f64 = segs.iter().map(|g| g.3).sum();
            let line_anchor = if line_attrs.is_empty() {
                anchor
            } else {
                "start"
            };
            let mut x = match line_anchor {
                "start" => line_x,
                "end" => line_x - line_w,
                _ => line_x - line_w / 2.0,
            };
            for (seg, st, fs, seg_w) in &segs {
                let rs = &r.styles[*st];
                let _ = write!(
                    body,
                    r#"<tspan x="{x:.4}" y="{y:.4}" text-anchor="start"{}>{}</tspan>"#,
                    rs.attrs(reduce),
                    escape_text(seg)
                );
                let ink = rs.color.as_deref().unwrap_or(&color);
                for (on, dy) in [(rs.underline, 0.11), (rs.strike, -0.27)] {
                    if on && !seg.trim().is_empty() {
                        let _ = write!(
                            deco,
                            r#"<line x1="{x:.4}" y1="{yy:.4}" x2="{x2:.4}" y2="{yy:.4}" stroke="{c}" stroke-width="{w:.4}"/>"#,
                            yy = y + dy * fs,
                            x2 = x + seg_w,
                            c = escape_attr(ink),
                            w = floored_stroke(fs * 0.06),
                        );
                    }
                }
                x += seg_w;
            }
        } else {
            let _ = write!(
                body,
                r#"<tspan x="{line_x:.4}" y="{y:.4}"{}>{}</tspan>"#,
                line_attrs,
                escape_text(&content)
            );
        }
        top += slot;
    }
    // Flip-aware block rotation (issue #19): the counter-scale keeps the
    // glyphs un-mirrored inside the caller's flipped frame, and the
    // rotation composes with flip parity so the page-space net is
    // Angle + sx*sy*TxtAngle.
    let flipped = sx < 0.0 || sy < 0.0;
    let net_txt_deg = -(sx * sy) * angle.to_degrees();
    // The block's tspan coordinates are pin-absolute (TxtPin-anchored),
    // so the counter-scale/rotation must pivot AT the pin: the inverse
    // translate after them keeps the text anchor fixed there. Without
    // it they would swing the whole block around the local origin.
    let mut transform = format!("translate({x:.5},{y:.5})", x = tx, y = -ty);
    if flipped {
        let _ = write!(transform, " scale({sx:.0},{sy:.0})", sx = sx, sy = sy);
    }
    if net_txt_deg.abs() > 1e-6 {
        let _ = write!(transform, " rotate({deg:.5})", deg = net_txt_deg);
    }
    if flipped || net_txt_deg.abs() > 1e-6 {
        let _ = write!(transform, " translate({x:.5},{y:.5})", x = -tx, y = ty);
    }
    // xml:space="preserve" (#144): SVG's default whitespace handling
    // strips leading/trailing spaces of text-node content, so the
    // spaces the splitter now preserves would be dropped again at
    // raster time. usvg (resvg 0.50) honors the attribute — inherited
    // by every tspan — and skips its stripping only for preserve, so
    // the authored spacing reaches the pixels. Greeked blocks (#124)
    // carry plain rects in `body`, never glyph nodes: they emit as a
    // bare <g> (a <text> wrapper's content model has no place for
    // shapes, and no font attributes are needed).
    if greeked {
        if flipped || net_txt_deg.abs() > 1e-6 {
            let _ = writeln!(out, r#"<g transform="{}">"#, escape_attr(&transform));
            let _ = writeln!(out, r#"<g>{b}</g>"#, b = body);
            let _ = writeln!(out, "</g>");
        } else {
            let _ = writeln!(out, r#"<g>{b}</g>"#, b = body);
        }
        return;
    }
    // #257 text-direction: the RTL lines' isolates order the runs
    // under ANY base; `direction="ltr"` pins the element base for
    // viewers that honor the attribute (usvg ignores it) so the
    // visual-left start anchor reads the same everywhere. LTR-only
    // blocks keep the pre-#257 byte form.
    let dir_attr = if any_rtl { r#" direction="ltr""# } else { "" };
    // #229 text background: ONE rectangle spanning the logical line
    // extents (widest laid-out line × the union of line heights),
    // margins position the text but add no padding. It uses the text
    // transform (same group), paints above shape geometry and below
    // text, and stays a rectangle in SVG. Greeking replaces the text
    // with bars, so the background stays off there.
    let bkgnd_rect = match (tb.text_bkgnd, bkgnd_span) {
        (Some((fill, alpha)), Some((lo, hi))) if hi > lo => {
            let (r, g, b) = hex_rgb(&fill);
            format!(
                r##"<rect x="{x:.4}" y="{y:.4}" width="{w:.4}" height="{h:.4}" fill="#{r:02x}{g:02x}{b:02x}" fill-opacity="{o:.3}" stroke="none"/>"##,
                x = lo,
                y = bkgnd_top,
                w = hi - lo,
                h = total_h,
                o = alpha as f64 / 255.0,
            )
        }
        _ => String::new(),
    };
    if flipped || net_txt_deg.abs() > 1e-6 {
        let _ = writeln!(out, r#"<g transform="{}">"#, escape_attr(&transform));
        let _ = write!(out, "{bkgnd_rect}");
        let _ = write!(out, "{bullet_paths}");
        let _ = writeln!(
            out,
            r#"<text{d} text-anchor="{a}" xml:space="preserve" {f}>{b}</text>"#,
            d = dir_attr,
            a = anchor,
            f = font_attrs,
            b = body
        );
        let _ = write!(out, "{deco}");
        let _ = writeln!(out, "</g>");
    } else {
        let _ = write!(out, "{bkgnd_rect}");
        let _ = write!(out, "{bullet_paths}");
        let _ = writeln!(
            out,
            r#"<text{d} text-anchor="{a}" xml:space="preserve" {f}>{b}</text>"#,
            d = dir_attr,
            a = anchor,
            f = font_attrs,
            b = body
        );
        let _ = write!(out, "{deco}");
    }
}

/// One Character row's look where it differs from the block (its
/// first row): `None` = as the block.
#[derive(Debug, Clone, PartialEq, Default)]
struct RunStyle {
    color: Option<String>,
    bold: Option<bool>,
    italic: Option<bool>,
    size: Option<f64>,
    underline: bool,
    strike: bool,
}

impl RunStyle {
    /// `reduce` is the Character.Pos size reduction the run was measured
    /// with: a super- or subscript run draws at the same reduced size
    /// (Greptile on PR #350).
    fn attrs(&self, reduce: f64) -> String {
        let mut a = String::new();
        if let Some(c) = &self.color {
            let _ = write!(a, r#" fill="{}""#, escape_attr(c));
        }
        if let Some(b) = self.bold {
            a.push_str(if b {
                r#" font-weight="bold""#
            } else {
                r#" font-weight="normal""#
            });
        }
        if let Some(i) = self.italic {
            a.push_str(if i {
                r#" font-style="italic""#
            } else {
                r#" font-style="normal""#
            });
        }
        if let Some(z) = self.size {
            let _ = write!(a, r#" font-size="{:.4}""#, z * reduce);
        }
        a
    }
}

/// A block's character runs: the style index of every layout-text
/// character (tabs count four, like the layout) and the styles.
struct TextRuns {
    per_char: Vec<usize>,
    styles: Vec<RunStyle>,
}

impl TextRuns {
    fn style_at(&self, i: usize) -> usize {
        self.per_char.get(i).copied().unwrap_or(0)
    }
}

/// Parse the raw text's `<cp IX='n'/>` markers into per-character
/// Character-row indices and resolve each row against the block's
/// first-row look (Visio 2021 witness c03: strike-through, red bold
/// and underlined runs authored through the Characters object).
/// `None` when no run differs (the block renders as before), when the
/// raw text does not reproduce the plain text, or it carries fields.
fn text_runs(doc: &Document, s: &Shape, chain: &[&Shape], tb: &TextBlock) -> Option<TextRuns> {
    let raw = s.sheet.text_raw.as_deref()?;
    if !raw.contains("<cp") || raw.contains("<fld") {
        return None;
    }
    let mut plain = String::new();
    let mut idx: Vec<usize> = Vec::new();
    let mut cur = 0usize;
    let mut rest = raw;
    while let Some(c) = rest.chars().next() {
        if c == '<' {
            let end = rest.find('>')?;
            let tag = &rest[..=end];
            if tag.starts_with("<cp") {
                let ix = tag
                    .split("IX=")
                    .nth(1)
                    .and_then(|v| v.trim_start_matches(['\'', '"']).split(['\'', '"']).next())
                    .and_then(|v| v.parse::<usize>().ok())?;
                cur = ix;
            }
            rest = &rest[end + 1..];
            continue;
        }
        let (ch, len) = if c == '&' {
            let end = rest.find(';')?;
            let ent = &rest[1..end];
            let ch = match ent {
                "amp" => '&',
                "lt" => '<',
                "gt" => '>',
                "quot" => '"',
                "apos" => '\'',
                e if e.starts_with("#x") => char::from_u32(u32::from_str_radix(&e[2..], 16).ok()?)?,
                e if e.starts_with('#') => char::from_u32(e[1..].parse().ok()?)?,
                _ => return None,
            };
            (ch, end + 1)
        } else {
            (c, c.len_utf8())
        };
        plain.push(ch);
        // A tab lays out as four spaces.
        let n = if ch == '\t' { 4 } else { 1 };
        idx.extend(std::iter::repeat_n(cur, n));
        rest = &rest[len..];
    }
    let expected = s.sheet.text.as_deref().unwrap_or("");
    if plain.replace("\r\n", "\n") != expected.replace("\r\n", "\n") {
        return None;
    }
    // Only the IX values the text references get a style, compacted to
    // 0..n: a document-controlled `<cp IX='1000000000'/>` must not size
    // an allocation (Greptile on PR #350). Index 0 stays the first
    // referenced row, so `style_at`'s fallback is a real style.
    let mut used: Vec<usize> = idx.clone();
    used.sort_unstable();
    used.dedup();
    for i in idx.iter_mut() {
        *i = used.binary_search(i).unwrap_or(0);
    }
    let row_cell = |ix: usize, name: &str| -> Option<Cell> {
        let find = |sh: &Sheet| {
            sh.sections_named("Character")
                .into_iter()
                .next()
                .and_then(|sec| sec.rows.iter().find(|r| r.ix == ix as i64))
                .and_then(|r| r.cell(name))
                .filter(|c| !c.is_inherited() && !c.is_themed())
                .cloned()
        };
        find(&s.sheet).or_else(|| doc.instance_source(s, chain).and_then(|m| find(&m.sheet)))
    };
    let styles: Vec<RunStyle> = used
        .iter()
        .map(|&ix| {
            let color = row_cell(ix, "Color")
                .map(|c| color_value(doc, c.v.as_deref().unwrap_or(""), c.f.as_deref()))
                .filter(|c| !c.eq_ignore_ascii_case(&tb.color));
            let bits = row_cell(ix, "Style")
                .and_then(|c| c.numeric())
                .map(|v| v as u32);
            let bold = bits.map(|b| b & 1 != 0).filter(|b| *b != tb.bold);
            let italic = bits.map(|b| b & 2 != 0).filter(|i| *i != tb.italic);
            let underline = bits.is_some_and(|b| b & 4 != 0);
            let strike = row_cell(ix, "Strikethru")
                .and_then(|c| c.numeric())
                .is_some_and(|v| v != 0.0)
                || row_cell(ix, "DoubleStrikethrough")
                    .and_then(|c| c.numeric())
                    .is_some_and(|v| v != 0.0);
            let size = row_cell(ix, "Size")
                .and_then(|c| c.numeric())
                .filter(|z| (z - tb.font_size).abs() > 1e-6);
            RunStyle {
                color,
                bold,
                italic,
                size,
                underline,
                strike,
            }
        })
        .collect();
    if styles.iter().all(|st| *st == RunStyle::default()) {
        return None;
    }
    Some(TextRuns {
        per_char: idx,
        styles,
    })
}

/// `#rrggbb` (or `#rgb`) → (r, g, b) bytes; black on anything else —
/// the TextBkgnd path emits the fill as explicit channels plus a
/// fill-opacity (#229), so it never leaks an unescaped attribute.
fn hex_rgb(color: &str) -> (u8, u8, u8) {
    let h = color.trim().trim_start_matches('#');
    let v = |s: &str| u8::from_str_radix(s, 16).unwrap_or(0);
    match h.len() {
        6 => (
            v(h.get(0..2).unwrap_or("00")),
            v(h.get(2..4).unwrap_or("00")),
            v(h.get(4..6).unwrap_or("00")),
        ),
        3 => {
            let (r, g, b) = (
                h.get(0..1).unwrap_or("0"),
                h.get(1..2).unwrap_or("0"),
                h.get(2..3).unwrap_or("0"),
            );
            let d = |s: &str| v(&format!("{s}{s}"));
            (d(r), d(g), d(b))
        }
        _ => (0, 0, 0),
    }
}

/// The UNBULLETED first-line indent in inches (PR #284 review round
/// 3, per the IndFirst/IndLeft docs): IndLeft is the base indent for
/// every line and IndFirst adjusts the FIRST line relative to it —
/// floored at 0. Consumed by the wrap (`first_off`), the paint band
/// and the TEXTHEIGHT measure together, so all three stay on one
/// layout.
pub(crate) fn unbulleted_first_offset_in(tb: &TextBlock) -> f64 {
    (tb.ind_left + tb.ind_first).max(0.0)
}

/// The measured #231 tab-stop rule, shared by the renderer and the
/// TEXTHEIGHT measure (PR #284 review — the measure must wrap at the
/// same first-line width the renderer lays out): TextPosAfterBullet
/// when it lies past the bullet's start (outright — Visio does not
/// take it as a minimum with the tab grid), else the hanging indent
/// (IndLeft when IndFirst is negative) and the next DefaultTabStop
/// multiple, whichever comes first; a bullet reaching past the stop
/// pushes the text to its end. Returns the stop as a TEXT-AREA
/// offset in INCHES, `None` for a non-bulleted block.
/// The bullet glyph's advance in inches — shared by the tab-stop
/// rule and the ink bounds (an outdented bullet hangs LEFT of the
/// text frame by its origin; the advance bounds the glyph itself).
pub(crate) fn bullet_advance_in(tb: &TextBlock, b: &BulletSpec) -> f64 {
    match &b.glyph {
        // The built-in round bullets draw as paths in a fixed em box
        // (see `bullet_glyph_svg`); the advance is that box's width —
        // a documented approximation.
        BulletGlyph::Round(_) => 0.36 * b.size,
        BulletGlyph::Str(glyph) => {
            let bface = crate::textmetrics::resolve(
                b.font.as_deref().or(tb.font.as_deref()),
                tb.bold,
                tb.italic,
            );
            bface.width_in(glyph, b.size)
        }
    }
}

pub(crate) fn bullet_stop_offset_in(tb: &TextBlock) -> Option<f64> {
    let b = tb.bullet.as_ref()?;
    let bullet_off = tb.ind_first;
    let adv = bullet_advance_in(tb, b);
    let bullet_end_off = bullet_off + adv;
    let stop = if b.pos_after > bullet_off {
        b.pos_after
    } else {
        let mut s = f64::INFINITY;
        if tb.ind_first < 0.0 {
            s = s.min(tb.ind_left);
        }
        let tab = (bullet_end_off / b.tab_stop).ceil() * b.tab_stop;
        s.min(tab)
    };
    Some(stop.max(bullet_end_off))
}

/// The built-in round bullet's drawn glyph as a PATH (#231): Word's
/// round set (styles 1–7) draws as •, "o", ▪, ❑, ❖, ➢, ✓ — emitted
/// as geometric paths in the TEXT fill color so no viewer font
/// substitution can change their size or style (the custom-string
/// bullets stay text). Each glyph is constructed in a 1-em local
/// frame centered on (x, y) and scaled by `size`; our own cleanroom
/// geometry, sized to read like Visio's marks.
fn bullet_glyph_svg(kind: u8, x: f64, y: f64, size: f64, color: &str) -> String {
    // The measured Visio marks, drawn from scratch (cleanroom: never
    // a copy of the Wingdings/Symbol outlines):
    let d = match kind {
        // 1 • filled dot
        1 => "M-0.14 0a0.14 0.14 0 1 0 0.28 0a0.14 0.14 0 1 0 -0.28 0Z",
        // 2 "o" — a ring (Courier New's lowercase o reads as a ring)
        2 => "M0 -0.15a0.15 0.15 0 1 0 0.001 0ZM0 -0.08a0.08 0.08 0 1 1 -0.001 0Z",
        // 3 ▪ filled square
        3 => "M-0.13 -0.13h0.26v0.26h-0.26Z",
        // 4 ❑ box with an inner void (the shadow-box reading of
        // Wingdings q)
        4 => "M-0.15 -0.15h0.30v0.30h-0.30ZM-0.08 -0.08h0.16v0.16h-0.16Z",
        // 5 ❖ four-diamond star
        5 => {
            "M0 -0.26l0.10 0.12l-0.10 0.12l-0.10 -0.12Z\
              M0 0.02l0.10 0.12l-0.10 0.12l-0.10 -0.12Z\
              M-0.19 -0.12l0.10 0.12l-0.10 0.12l-0.10 -0.12Z\
              M0.19 -0.12l0.10 0.12l-0.10 0.12l-0.10 -0.12Z"
        }
        // 6 ➢ concave arrowhead pointing right
        6 => "M-0.14 -0.15l0.29 0.15l-0.29 0.15l0.09 -0.15Z",
        // 7 ✓ a stroked check
        _ => "",
    };
    let scale = format!("translate({x:.4},{y:.4}) scale({s:.5})", s = size);
    if kind == 7 {
        format!(
            r#"<path transform="{t}" d="M-0.15 0.01l0.11 -0.13l0.21 0.23" fill="none" stroke="{c}" stroke-width="0.09" stroke-linecap="round" stroke-linejoin="round"/>"#,
            t = scale,
            c = escape_attr(color),
        )
    } else {
        format!(
            r#"<path transform="{t}" d="{d}" fill="{c}" fill-rule="evenodd"/>"#,
            t = scale,
            c = escape_attr(color),
        )
    }
}

/// Data URI for a shape's embedded image (`ForeignData`), resolved through
/// the owning part's relationships to the media part. EMF/WMF cannot be
/// embedded in SVG and are skipped.
fn foreign_image_uri(doc: &Document, part: &str, s: &Shape, chain: &[&Shape]) -> Option<String> {
    // A master instance carries no ForeignData of its own: the picture
    // lives on the master shape, its media relationship on the MASTER
    // part (the network LLDs' icons — every one of them).
    let (src_shape, part) = foreign_data_owner(doc, part, s, chain)?;
    let s = src_shape;
    let part = part.as_str();
    let attr = |n: &str| -> Option<&str> {
        s.raw_children
            .iter()
            .find(|r| r.name == "ForeignData")?
            .attrs
            .iter()
            .find(|(k, _)| k == n)
            .map(|(_, v)| v.as_str())
    };
    if attr("ForeignType") != Some("Bitmap") {
        return None; // OLE objects / metafiles: not image parts
    }
    let mime = match attr("CompressionType") {
        Some("PNG") => "image/png",
        Some("JPEG") | Some("JPG") => "image/jpeg",
        Some("GIF") => "image/gif",
        // No CompressionType: a plain BMP (Visio 2021 witness b03) —
        // re-encoded as PNG below, SVG viewers cannot show BMP.
        None | Some("BMP") | Some("DIB") => "image/bmp",
        _ => return None, // TIFF and unknowns stay unrendered
    };
    let fd = s.raw_children.iter().find(|r| r.name == "ForeignData")?;
    // The rel id lives either on ForeignData itself or in a <Rel r:id=…/>
    // child (the shipped layout).
    let rid = fd
        .attrs
        .iter()
        .find(|(k, _)| k == "r:id")
        .map(|(_, v)| v.clone())
        .or_else(|| extract_rid(&fd.inner))?;
    // Runtime-inserted media (#113) resolves through `extra_media`
    // by the owning page's part — no retained package needed; the
    // package relationship wins when it carries this rid.
    let target: String = doc
        .package
        .as_ref()
        .and_then(|pkg| {
            pkg.relationships(part)
                .into_iter()
                .find(|(id, ty, _)| id == &rid && ty.ends_with("/image"))
                .map(|(_, _, t)| t)
        })
        .or_else(|| {
            let page_id = doc.pages.iter().find(|p| p.part == part).map(|p| p.id)?;
            doc.extra_media
                .iter()
                .find(|m| m.page == page_id && m.rid == rid)
                .map(|m| m.part.clone())
        })?;
    // `relationships` returns the target already resolved to an absolute
    // package part name. Runtime-inserted media (#113) lives in
    // `extra_media`, not the retained package.
    let name = target.trim_start_matches('/');
    let bytes = doc
        .package
        .as_ref()
        .and_then(|pkg| pkg.part(name))
        .or_else(|| {
            doc.extra_media
                .iter()
                .find(|m| m.part == name)
                .map(|m| m.bytes.as_slice())
        })?;
    if mime == "image/bmp" {
        let png = crate::emf::bmp_to_png(bytes)?;
        return Some(format!("data:image/png;base64,{}", base64(&png)));
    }
    Some(format!("data:{mime};base64,{}", base64(bytes)))
}

/// A shape's embedded EMF picture (`ForeignType='EnhMetaFile'`) replayed
/// as SVG elements in the box `(0,-h)…(w,0)` — network-stencil icons and
/// pasted clip art (Visio 2021 witness b03; ~150 icons in the network
/// LLDs rendered empty).
fn foreign_emf_svg(
    doc: &Document,
    part: &str,
    s: &Shape,
    chain: &[&Shape],
    w: f64,
    h: f64,
) -> Option<String> {
    let (s, part) = foreign_data_owner(doc, part, s, chain)?;
    let part = part.as_str();
    let fd = s.raw_children.iter().find(|r| r.name == "ForeignData")?;
    let ty = fd
        .attrs
        .iter()
        .find(|(k, _)| k == "ForeignType")
        .map(|(_, v)| v.as_str());
    if ty != Some("EnhMetaFile") {
        return None;
    }
    let rid = fd
        .attrs
        .iter()
        .find(|(k, _)| k == "r:id")
        .map(|(_, v)| v.clone())
        .or_else(|| extract_rid(&fd.inner))?;
    let pkg = doc.package.as_ref()?;
    let target = pkg
        .relationships(part)
        .into_iter()
        .find(|(id, ty, _)| id == &rid && ty.ends_with("/image"))
        .map(|(_, _, t)| t)?;
    let bytes = pkg.part(target.trim_start_matches('/'))?;
    // Legacy stencils store WMF bytes under the EnhMetaFile type (the
    // network LLDs' bridge/ONT icons).
    crate::emf::emf_to_svg(bytes, w, h).or_else(|| crate::emf::wmf_to_svg(bytes, w, h))
}

/// The shape that owns a picture's `ForeignData` and the package part
/// whose relationships resolve its media: the shape itself on `part`, or
/// — for a master instance without its own ForeignData — the master
/// shape on the master's part.
fn foreign_data_owner<'a>(
    doc: &'a Document,
    part: &str,
    s: &'a Shape,
    chain: &[&'a Shape],
) -> Option<(&'a Shape, String)> {
    if s.raw_children.iter().any(|r| r.name == "ForeignData") {
        return Some((s, part.to_string()));
    }
    let src = doc.instance_source(s, chain)?;
    if !src.raw_children.iter().any(|r| r.name == "ForeignData") {
        return None;
    }
    let mid = s
        .master
        .or_else(|| chain.iter().rev().find_map(|a| a.master))?;
    let m = doc.master(mid)?;
    Some((src, m.part.clone()))
}

/// `<Rel r:id='rId9'/>` → `rId9` (both quote styles).
fn extract_rid(inner: &str) -> Option<String> {
    let i = inner.find("r:id")?;
    let rest = &inner[i + 5..];
    let q = rest.chars().next()?;
    let end = rest[1..].find(q)? + 1;
    Some(rest[1..end].to_string())
}

pub(crate) fn base64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Per-section master inheritance (Visio semantics): geometry comes from
/// the instance's own Geometry sections when present (authored in
/// instance units), else from the master source shape's Geometry (master-
/// local units, scaled to the instance size). The instance often carries
/// other inline sections (Character/Paragraph) — that must not mask the
/// inherited geometry.
fn render_all_geometry(
    doc: &Document,
    s: &Shape,
    chain: &[&Shape],
    width: f64,
    height: f64,
    jump_radius: (f64, f64),
    out: &mut String,
) {
    // Groups usually don't self-draw (children + the master drawing
    // carry the visuals) but container groups DO carry a visible rect,
    // so group geometry renders too; degenerate leftover rows collapse
    // to invisible zero-area paths after the row-cell merge below.
    let env = shape_env(doc, s, chain);
    // #228: both frames below accumulate into ONE shape-level paint —
    // the alternate-fill union flushes under the per-section strokes.
    let mut paint = ShapePaint::default();
    // #294: a connector's stored jump ArcTo rows straighten — the
    // jump overlay owns the one arc at the current crossing, and a
    // stored arc stops lingering after a move. `strip_jumps` arrives
    // from the caller (the overlay covers exactly the top-level
    // shapes of a full-page render — #294 review); connector-ness
    // gates the shape kind.
    // Connector-ness gates at the strip site (render_geometry): artwork
    // arcs never strip even with a resolved radius.
    let own: Vec<&vsdx::Section> = s.sheet.sections_named("Geometry");
    if !own.is_empty() {
        // Row-cell inheritance: inline copies often keep only the Y cells
        // (F='Inh') and inherit X/A/B/C/D from the master source's
        // corresponding row. Inherited values are master-local and are
        // scaled into instance units here.
        let src = doc.instance_source(s, chain);
        let mut merged: Vec<vsdx::Section> = Vec::with_capacity(own.len());
        for (si, geo) in own.iter().enumerate() {
            let mut m = (*geo).clone();
            if let Some(src) = src {
                let src_secs = src.sheet.sections_named("Geometry");
                let src_geo = src_secs.get(si);
                // Section-level cells (Geometry NoFill/NoLine/NoShow)
                // inherit from the master section just like rows do:
                // inline copies keep only row caches, and a lost NoShow
                // draws sections Visio hides — e.g. every heading-band
                // geometry of the "Plain" container style (#12).
                if let Some(sg) = src_geo {
                    for (name, cell) in &sg.cells {
                        if !m.cells.iter().any(|(n, _)| n == name) {
                            m.cells.push((name.clone(), cell.clone()));
                        }
                    }
                }
                let mw = src
                    .sheet
                    .cell("Width")
                    .and_then(|c| c.numeric())
                    .filter(|v| v.abs() > 1e-9);
                let mh = src
                    .sheet
                    .cell("Height")
                    .and_then(|c| c.numeric())
                    .filter(|v| v.abs() > 1e-9);
                let (sx, sy) = (
                    mw.map_or(1.0, |mw| width / mw),
                    mh.map_or(1.0, |mh| height / mh),
                );
                let scaled = |name: &str, v: f64| match name {
                    "X" | "A" | "C" => v * sx,
                    "Y" | "B" | "D" => v * sy,
                    _ => v,
                };
                // EllipticalArcTo's C is the major-axis ANGLE and D the
                // axis RATIO — neither is a coordinate, so an inherited
                // (omitted = unchanged) value never scales with the
                // instance size (a stretched U-Turn arrow turned D=1
                // into a 9:1 ratio and its quarter circles into blobs).
                let not_coord = |t: Option<&str>, name: &str| {
                    matches!(t, Some("EllipticalArcTo") | Some("RelEllipticalArcTo"))
                        && matches!(name, "C" | "D")
                };
                // Pair each inline row with the master row of the SAME
                // IX. Inline copies drop rows whose cells are fully
                // inherited-identical, keeping the survivors' original
                // IX values — so occurrence pairing can match the wrong
                // row (diagonal wedges, giant arcs). Occurrence within a
                // row type is the fallback when IX is absent/ambiguous.
                let mut src_occ: std::collections::HashMap<String, usize> =
                    std::collections::HashMap::new();
                // Rows the inline copy dropped entirely (all cells
                // inherited-identical) are re-inserted from the master,
                // scaled, at their IX position — otherwise the path
                // shortcuts across the gap (diagonal wedges).
                if let (Some(sg), true) = (src_geo, m.rows.iter().any(|r| r.ix > 0)) {
                    let have: Vec<i64> = m.rows.iter().map(|r| r.ix).collect();
                    let mut inserted: Vec<(usize, Row)> = Vec::new();
                    for src_row in &sg.rows {
                        if src_row.ix > 0
                            && !have.contains(&src_row.ix)
                            && src_row.t.is_some()
                            && !src_row.is_deleted()
                        {
                            let mut nr = src_row.clone();
                            // scale per cell name
                            // The same rule as the omitted-CELL fallback
                            // below (Greptile on PR #343): a size formula
                            // evaluates in the instance env; one the env
                            // cannot evaluate keeps the master's cache as
                            // is, since an omitted row is unchanged.
                            let bound = 3.0 * width.max(height).max(0.02);
                            for (cname, cell) in nr.cells.iter_mut() {
                                let proportional =
                                    cell.f.as_deref().map(refs_own_size).unwrap_or(true);
                                let has_formula = cell
                                    .f
                                    .as_deref()
                                    .is_some_and(|f| f != "Inh" && f != "No Formula");
                                if let Some(v) = cell.numeric() {
                                    let placed = if !proportional
                                        || not_coord(src_row.t.as_deref(), cname)
                                    {
                                        v
                                    } else if has_formula {
                                        cell.f
                                            .as_deref()
                                            .and_then(|f| vsdx::formula::eval(f, &env).ok())
                                            .and_then(|e| e.as_f64())
                                            .filter(|e| e.is_finite() && e.abs() <= bound)
                                            .unwrap_or(v)
                                    } else {
                                        scaled(cname, v)
                                    };
                                    cell.v = Some(format!("{placed}"));
                                }
                                cell.f = None;
                            }
                            // position: after the last own row with smaller IX
                            let pos = m
                                .rows
                                .iter()
                                .rposition(|r| r.ix > 0 && r.ix < src_row.ix)
                                .map(|i| i + 1)
                                .unwrap_or(0);
                            inserted.push((pos, nr));
                        }
                    }
                    for (pos, nr) in inserted.into_iter().rev() {
                        let pos = pos.min(m.rows.len());
                        m.rows.insert(pos, nr);
                    }
                }
                for row in m.rows.iter_mut() {
                    let Some(t) = row.t.clone() else { continue };
                    let by_ix =
                        src_geo.and_then(|sg| sg.rows.iter().find(|r| r.ix == row.ix && r.ix > 0));
                    let occ = {
                        let c = src_occ.entry(t.clone()).or_insert(0);
                        *c += 1;
                        *c
                    };
                    let by_occ = src_geo.and_then(|sg| {
                        sg.rows
                            .iter()
                            .filter(|r| r.t.as_deref() == Some(t.as_str()))
                            .nth(occ - 1)
                    });
                    let src_row = by_ix.or(by_occ);
                    let Some(src_row) = src_row else { continue };
                    let bound = 3.0 * width.max(height).max(0.02);
                    let mut extra: Vec<(String, Cell)> = Vec::new();
                    for (cname, scell) in &src_row.cells {
                        // A PolylineTo's A cell is formula TEXT, not a
                        // number: the numeric merge below would drop it
                        // and the row would draw a chord. An instance row
                        // without its own POLYLINE text takes the
                        // master's; absolute axes (type 1) are master
                        // units and scale like X/Y, fractional ones stay
                        // (CodeRabbit on PR #329). Only a PolylineTo
                        // instance row takes it: an instance that turned
                        // the row into an ArcTo keeps its own numeric A
                        // (Greptile on PR #329).
                        if cname == "A" && src_row.t.as_deref() == Some("PolylineTo") {
                            if row.t.as_deref() != Some("PolylineTo") {
                                continue;
                            }
                            let own_text = row.cells.iter().any(|(n, c)| {
                                n == "A"
                                    && c.v
                                        .as_deref()
                                        .or(c.f.as_deref())
                                        .is_some_and(|t| t.trim_start().starts_with("POLYLINE"))
                            });
                            let master_text = scell
                                .v
                                .as_deref()
                                .or(scell.f.as_deref())
                                .unwrap_or_default();
                            if !own_text {
                                if let Some((xt, yt, verts)) =
                                    crate::curve::parse_polyline(master_text)
                                {
                                    let mut text = format!("POLYLINE({xt}, {yt}");
                                    for (vx, vy) in verts {
                                        let vx = if xt == 0.0 { vx } else { scaled("X", vx) };
                                        let vy = if yt == 0.0 { vy } else { scaled("Y", vy) };
                                        let _ = write!(text, ", {vx},{vy}");
                                    }
                                    text.push(')');
                                    let cell = Cell {
                                        v: Some(text),
                                        ..Cell::default()
                                    };
                                    match row.cells.iter_mut().find(|(n, _)| n == "A") {
                                        Some((_, c)) => *c = cell,
                                        None => extra.push(("A".to_string(), cell)),
                                    }
                                }
                            }
                            continue;
                        }
                        if !matches!(cname.as_str(), "X" | "Y" | "A" | "B" | "C" | "D") {
                            continue;
                        }
                        // Master cells proportional to Width/Height scale
                        // into instance units; User.*-driven cells (corner
                        // radii and the like) stay absolute — instances
                        // keep those unscaled while the body stretches.
                        let proportional = scell.f.as_deref().map(refs_own_size).unwrap_or(true);
                        let placed = |v: f64| {
                            if proportional && !not_coord(src_row.t.as_deref(), cname) {
                                scaled(cname, v)
                            } else {
                                v
                            }
                        };
                        // Formula-authoritative for PROPORTIONAL master
                        // cells (the same rule the row walk uses):
                        // corpus connector masters carry stale zero
                        // caches (`Y V=0 F=Height/2` — utilisation
                        // master3) that would collapse the inherited
                        // arcs to flat lines (#202). The formula
                        // evaluates against the INSTANCE dims — for a
                        // proportional formula that IS the
                        // master→instance scaling. Non-proportional
                        // formulas (`Controls.Row_1`, `User.*`) have no
                        // instance meaning; their cached value (if any)
                        // stands, unfilled otherwise.
                        // Formula-authoritative cells evaluate against
                        // the INSTANCE env — the value is already in
                        // instance units and must NOT be placed-scaled
                        // again (#203 review). Cached master-unit
                        // numerics are master-unit and keep placed().
                        // A value-only master cell (no formula — the
                        // common plain-geometry case) or a formula the
                        // instance env can't evaluate falls back to the
                        // scaled cache; skipping would let row_num
                        // default the axis to 0 and collapse the
                        // inherited geometry (#185 review).
                        // A master FORMULA the instance env cannot
                        // evaluate (`Height-User.BarWidth` with the
                        // master's User row out of reach) keeps the
                        // master's cached value AS IS: Visio writes every
                        // instance cell whose value changed (F='Inh' plus
                        // the new V), so an omitted cell is unchanged —
                        // scaling it bent the U-Turn arrow's return leg
                        // into a blob (Visio 2021 witness, the network
                        // LLD's U-Turn arrows stretched to 9.7 × 1 in).
                        let has_formula = scell
                            .f
                            .as_deref()
                            .is_some_and(|f| f != "Inh" && f != "No Formula");
                        let sv = if proportional {
                            scell
                                .f
                                .as_deref()
                                .and_then(|f| vsdx::formula::eval(f, &env).ok())
                                .and_then(|v| v.as_f64())
                                .filter(|v| v.is_finite() && v.abs() <= bound)
                                .or_else(|| {
                                    let v = scell.numeric()?;
                                    let v = if has_formula { v } else { placed(v) };
                                    Some(v).filter(|v| v.abs() <= bound)
                                })
                        } else {
                            scell.numeric().map(placed).filter(|v| v.abs() <= bound)
                        };
                        let Some(sv) = sv else {
                            continue;
                        };
                        let patched = Cell::value(format!("{sv}"));
                        match row.cells.iter_mut().find(|(n, _)| n == cname) {
                            Some((_, cell)) => {
                                // A local numeric V wins even under F='Inh':
                                // Visio refreshes cached values at save, so
                                // e.g. a legend member's flattened arc B=0 is
                                // the live evaluation, not a stale snapshot.
                                if cell.v.is_none()
                                    || (cell.is_inherited() && cell.numeric().is_none())
                                {
                                    *cell = patched;
                                }
                            }
                            None => extra.push((cname.clone(), patched)),
                        }
                    }
                    row.cells.extend(extra);
                }
            }
            merged.push(m);
        }
        for (i, geo) in merged.iter().enumerate() {
            // Instance frame: formulas and cached values are both in
            // instance units.
            let gname = format!("Geometry{}", i + 1);
            render_geometry(
                doc,
                s,
                chain,
                geo,
                (1.0, 1.0),
                Frame::Instance,
                &gname,
                jump_radius,
                out,
                &mut paint,
            );
        }
        flush_shape_paint(&mut paint, out);
        return;
    }
    let Some(src) = doc.instance_source(s, chain) else {
        return;
    };
    let mw = src
        .sheet
        .cell("Width")
        .and_then(|c| c.numeric())
        .filter(|v| v.abs() > 1e-9);
    let mh = src
        .sheet
        .cell("Height")
        .and_then(|c| c.numeric())
        .filter(|v| v.abs() > 1e-9);
    let scale = (
        mw.map_or(1.0, |mw| width / mw),
        mh.map_or(1.0, |mh| height / mh),
    );
    // The master env builds ONCE for all sections (knee rows
    // cross-reference sibling sections) and RE-EVALUATES the master's
    // Scratch/Controls against the instance's glued Begin/End — their
    // cached V is the authored pose, and the band masters derive
    // their whole vertical span from them (`Scratch.Y2 =
    // ABS(BeginY-EndY)`, #249).
    let mut menv = shape_env(doc, src, &[]);
    for n in ["BeginX", "BeginY", "EndX", "EndY"] {
        if let Some(v) = doc.resolve_cell(s, chain, n).and_then(|c| c.numeric()) {
            menv.insert(n.to_string(), v);
        }
    }
    for sec_name in ["Scratch", "Controls"] {
        for sec in src.sheet.sections_named(sec_name) {
            for row in &sec.rows {
                for (name, cell) in &row.cells {
                    let key = match sec_name {
                        "Scratch" => format!("Scratch.{}{}", name, row.ix + 1),
                        _ => {
                            let rn = row.n.clone().unwrap_or_else(|| format!("Row_{}", row.ix));
                            format!("Controls.{}.{}", rn, name)
                        }
                    };
                    let v = cell
                        .f
                        .as_deref()
                        .filter(|f| *f != "Inh" && *f != "No Formula")
                        .and_then(|f| vsdx::formula::eval(f, &menv).ok())
                        .and_then(|v| v.as_f64())
                        .filter(|v| v.is_finite())
                        .or_else(|| cell.numeric());
                    if let Some(v) = v {
                        menv.insert(key, v);
                    }
                }
            }
        }
    }
    for (i, geo) in src.sheet.sections_named("Geometry").into_iter().enumerate() {
        // Master frame: rows are master-local; their formulas reference
        // the MASTER's dims and must not be re-evaluated in instance
        // units (that skews rectangles into triangles).
        render_geometry(
            doc,
            s,
            chain,
            geo,
            scale,
            Frame::Master(&mut menv),
            &format!("Geometry{}", i + 1),
            jump_radius,
            out,
            &mut paint,
        );
    }
    flush_shape_paint(&mut paint, out);
}

/// Which frame geometry rows live in — picks the formula environment.
/// The master variant carries the PREPARED env (master cells, the
/// instance's glued Begin/End seeded in).
enum Frame<'a> {
    Instance,
    /// The PREPARED env (master cells, the instance's glued Begin/End
    /// seeded in), SHARED by reference across every Geometry section
    /// of the frame (#218 review): render_geometry feeds evaluated row
    /// coordinates back into it (`Geometry1.X2`), and per-section
    /// clones discarded those writes — a Geometry2 referencing
    /// Geometry1 fell back to the authored pose.
    Master(&'a mut vsdx::formula::Env),
}

#[allow(clippy::too_many_arguments)] // the walk's natural inputs (+gname, #249)
fn render_geometry(
    doc: &Document,
    s: &Shape,
    chain: &[&Shape],
    section: &vsdx::Section,
    scale: (f64, f64),
    frame: Frame<'_>,
    gname: &str,
    jump_radius: (f64, f64),
    out: &mut String,
    paint: &mut ShapePaint,
) {
    let rows = &section.rows;
    let (width, height) = shape_size(doc, s, chain);
    // Formulas evaluate in the geometry's own frame: instance dims for
    // inline geometry, the master source's dims for inherited geometry.
    // The cached V already lives in that frame; evaluating a formula in
    // the WRONG frame (e.g. master rows against instance Width) skews
    // rectangles into triangles.
    let inherited_frame = matches!(frame, Frame::Master(_));
    let mut inst_env;
    let env: &mut vsdx::formula::Env = match frame {
        Frame::Instance => {
            inst_env = shape_env_for(doc, s, chain, rows);
            &mut inst_env
        }
        Frame::Master(env) => env,
    };
    // Section-level cells carry NoFill/NoLine/NoShow. Formula first,
    // cached V as fallback (the row-cell convention): the master's
    // cache reflects its default style while the formula re-evaluates
    // per instance — NoShow=OR(User.hidden,User.CStyle<>7) gates the
    // container heading's geometry bands by container style.
    let flag = |name: &str| -> bool {
        section.cell(name).is_some_and(|c| {
            let by_formula =
                c.f.as_deref()
                    .and_then(|f| {
                        if f == "Inh" || f == "No Formula" {
                            None
                        } else {
                            vsdx::formula::eval(f, env).ok().and_then(|v| v.as_f64())
                        }
                    })
                    .filter(|v| v.is_finite());
            by_formula.or_else(|| c.numeric()).is_some_and(|v| v != 0.0)
        })
    };
    let no_fill = flag("NoFill");
    let no_show = flag("NoShow");
    if no_show {
        return;
    }
    // Visio hides the outline when LinePattern = 0 (e.g. legend label
    // boxes carry THEMEGUARD(0)); the geometry still exists for hit
    // testing but must not stroke.
    let line_pattern = doc
        .resolve_cell(s, chain, "LinePattern")
        .and_then(|c| cell_num(&c, env));
    let no_line = flag("NoLine") || line_pattern.is_some_and(|p| p == 0.0);
    fn row_num(env: &vsdx::formula::Env, r: &Row, c: &str) -> Option<f64> {
        r.cell(c).and_then(|cell| {
            // Geometry rows: the formula is authoritative (Visio evaluates
            // F at load; page copies can carry a stale cached V — e.g.
            // V='0' with F='Width*1' after a resize). Fall back to the
            // cache when there is no evaluable formula.
            let by_formula = cell
                .f
                .as_deref()
                .and_then(|f| {
                    if f == "Inh" || f == "No Formula" {
                        None
                    } else {
                        vsdx::formula::eval(f, env).ok().and_then(|v| v.as_f64())
                    }
                })
                .filter(|v| v.is_finite());
            by_formula.or_else(|| cell.numeric())
        })
    }

    let fill_color = themed_color_string(doc, s, chain, "FillForegnd");
    let line_color = themed_color_string(doc, s, chain, "LineColor");
    let fill_bkgnd = themed_color_string(doc, s, chain, "FillBkgnd");
    let line_weight = doc
        .resolve_cell(s, chain, "LineWeight")
        .and_then(|c| cell_num(&c, env))
        .unwrap_or(0.0104);
    // A 1-D shape's OPEN path (a line, an arc) has no interior: some
    // line masters carry decorative FillPattern cells (28/30) that must
    // be ignored, or the arc-chord region paints over everything. Its
    // CLOSED sections fill like any 2-D shape's — the block-arrow
    // masters (Simple/Notched/Double Arrow) are 1-D shapes whose one
    // closed outline Visio paints with FillForegnd (Visio 2021
    // witness, t04). The open/closed gate sits at the fill join below.
    let one_d = doc.resolve_cell_explicit(s, chain, "BeginX").is_some()
        || doc.resolve_cell_explicit(s, chain, "EndX").is_some();
    let fill_pattern = doc
        .resolve_cell_explicit(s, chain, "FillPattern")
        .and_then(|c| cell_num(&c, env))
        .unwrap_or(1.0);
    // Gradient fills: Visio theme fill patterns 24-40 are gradients
    // (FillForegnd → FillBkgnd along FillGradientAngle, default 90°
    // top-to-bottom). Everything else patterned >1.5 is a hatch: the
    // background dominates, so it renders as the background color.
    // Visio 2013+ gradients (FillGradientEnabled): the FillGradient
    // stop rows, FillGradientDir (0 linear, 1–7 radial, 8–13
    // rectangular) and FillGradientAngle — see `gradient_paint`. They
    // win over the legacy pattern gradients below.
    let fill_gradient = if no_fill || fill_pattern == 0.0 {
        None
    } else {
        gradient_paint(
            doc,
            s,
            chain,
            env,
            "Fill",
            (width, height),
            (&fill_color, &fill_bkgnd),
            out,
        )
    };
    let fill = if no_fill || fill_pattern == 0.0 {
        "none".to_string()
    } else if let Some(g) = &fill_gradient {
        match g {
            GradientPaint::Url(u) => u.clone(),
            // Rectangular: the solid color stands in on the path; the
            // clipped tiles replace it at flush time.
            GradientPaint::Tiles(_) => fill_color.clone(),
        }
    } else if (24.0..=40.0).contains(&fill_pattern) {
        let angle_deg = doc
            .resolve_cell_explicit(s, chain, "FillGradientAngle")
            .and_then(|c| cell_num(&c, env))
            .map(|rad| rad.to_degrees())
            .unwrap_or(90.0);
        let id = format!(
            "dc-grad{:x}",
            fnv(&format!("{fill_color}|{fill_bkgnd}|{angle_deg:.0}"))
        );
        if !out.contains(&format!(r#"id="{id}""#)) {
            // Angle from north, clockwise → SVG gradient vector.
            let rad = angle_deg.to_radians();
            let (x1, y1) = (0.5 - rad.sin() / 2.0, 0.5 + rad.cos() / 2.0);
            let (x2, y2) = (0.5 + rad.sin() / 2.0, 0.5 - rad.cos() / 2.0);
            let _ = writeln!(
                out,
                r#"<defs><linearGradient id="{id}" x1="{x1:.4}" y1="{y1:.4}" x2="{x2:.4}" y2="{y2:.4}"><stop offset="0" stop-color="{fg}"/><stop offset="1" stop-color="{bg}"/></linearGradient></defs>"#,
                fg = escape_attr(&fill_color),
                bg = escape_attr(&fill_bkgnd),
            );
        }
        format!("url(#{id})")
    } else if fill_pattern > 1.5 {
        match hatch_fill(fill_pattern, &fill_color, &fill_bkgnd, (width, height)) {
            Some(Hatch::Mixed(c)) => c,
            // Line patterns: the background color on the path; the
            // clipped lines replace it at flush time (`hatch_tiles`).
            _ => fill_bkgnd.clone(),
        }
    } else {
        fill_color.clone()
    };
    let hatch_tiles = if no_fill || fill_gradient.is_some() {
        None
    } else {
        match hatch_fill(fill_pattern, &fill_color, &fill_bkgnd, (width, height)) {
            Some(Hatch::Lines(t)) => Some(t),
            _ => None,
        }
    };
    // Fill/line transparency: every *Trans cell is a fraction (the
    // file stores 50 % as V='0.5'). A solid fill takes
    // FillForegndTrans, a pattern's dominant background color
    // FillBkgndTrans; the stroke (and its arrowheads) LineColorTrans.
    // Fully transparent paint is dropped outright — the network
    // stencils' magenta helper boxes carry Trans 100 % and must not
    // show (Visio 2021 witness).
    let trans = |n: &str| -> f64 {
        doc.resolve_cell(s, chain, n)
            .and_then(|c| cell_num(&c, env))
            .filter(|v| v.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0)
    };
    let fill_alpha = 1.0
        - if fill_gradient.is_some() {
            0.0 // per-stop GradientStopColorTrans instead
        } else if fill_pattern > 1.5 && !(24.0..=40.0).contains(&fill_pattern) {
            trans("FillBkgndTrans")
        } else {
            trans("FillForegndTrans")
        };
    let line_alpha = 1.0 - trans("LineColorTrans");
    // LineGradientEnabled: the stroke paints with the LineGradient
    // stops (rectangular directions have no stroke form and fall back
    // to linear); arrowheads keep the solid LineColor.
    let stroke_paint = match gradient_paint(
        doc,
        s,
        chain,
        env,
        "Line",
        (width, height),
        (&line_color, &line_color),
        out,
    ) {
        Some(GradientPaint::Url(u)) => u,
        _ => line_color.clone(),
    };
    let no_fill = no_fill || fill_alpha <= 0.0;
    let no_line = no_line || line_alpha <= 0.0;
    let fill = if no_fill { "none".to_string() } else { fill };
    let dash = line_pattern.and_then(|p| line_dasharray(p, line_weight));
    // #228: the section's own element is STROKE-only (fill
    // participation joins the shape's one even-odd path — Ellipse rows
    // included).
    // LineCap: 0 round (Visio's default — round ends AND round
    // corners), 1 flat at the endpoint, 2 square-extended (Visio 2021
    // witness, b04: 12 pt lines and corners).
    // A shape-level cell: its formula evaluates in the INSTANCE env
    // first, also for inherited master geometry, and the cache (the
    // master's, for an inherited cell) is the fallback — the Rounding
    // rule (Greptile on PR #342).
    let cap_own_env;
    let cap_env: &vsdx::formula::Env = if inherited_frame {
        cap_own_env = shape_env(doc, s, chain);
        &cap_own_env
    } else {
        env
    };
    let line_cap = doc
        .resolve_cell(s, chain, "LineCap")
        .and_then(|c| {
            c.f.as_deref()
                .filter(|f| *f != "Inh" && *f != "No Formula")
                .and_then(|f| vsdx::formula::eval(f, cap_env).ok())
                .and_then(|v| v.as_f64())
                .or_else(|| cell_num(&c, cap_env))
        })
        .map_or(0, |v| v.round() as i64);
    let stroke_a = stroke_attr(
        no_line,
        &stroke_paint,
        line_weight,
        dash.as_deref(),
        line_alpha,
    ) + cap_attr(line_cap, dash.is_some()).as_str();

    // Master-space (unscaled) path state: geometry rows are authored in the
    // master's local units and scaled to the instance afterwards.
    let mut d = String::new();
    let mut cur = (0.0f64, 0.0f64); // master-space current point
    let sc = |p: (f64, f64)| -> (f64, f64) { (p.0 * scale.0, p.1 * scale.1) };
    // Emit a path coordinate pair with the local Y flip (Visio Y-up → SVG Y-down).
    let emit_pt = |p: (f64, f64)| -> String {
        let p = sc(p);
        format!("{:.5} {:.5} ", p.0, -p.1)
    };
    // #188: the spline-family accumulator — a SplineStart row opens a
    // spline, SplineKnot rows extend it, and any other row (or the
    // section's end) flushes the evaluated curve. 16 samples per knot
    // span: smooth at print scale, and the hit walk flattens with the
    // same constant so hit-testing agrees with the ink.
    let mut spline_acc: Option<crate::curve::SplineAcc> = None;
    macro_rules! flush_spline {
        () => {
            if let Some(acc) = spline_acc.take() {
                if let Some(data) = acc.flush() {
                    let pts = data.flatten(16);
                    for p in pts.iter().skip(1) {
                        let _ = write!(d, "L {}", emit_pt(*p));
                    }
                    if let Some(last) = pts.last() {
                        cur = *last;
                    }
                }
            }
        };
    }

    // The section-end flush: no further rows follow, so the walk's
    // current point is not updated (that assignment would be dead).
    macro_rules! flush_spline_tail {
        () => {
            if let Some(acc) = spline_acc.take() {
                if let Some(data) = acc.flush() {
                    let pts = data.flatten(16);
                    for p in pts.iter().skip(1) {
                        let _ = write!(d, "L {}", emit_pt(*p));
                    }
                }
            }
        };
    }

    // Pre-evaluate every typed row's endpoint ONCE, in row order (the
    // env feeds keep their order — #249), so the stored-jump
    // classification can see the route's neighboring runs (#294
    // review).
    // Control cells (A/B/C/D) evaluate at the row's ORIGINAL point in
    // the ordered walk — each row sees exactly the feeds of the rows
    // before it, never a later row's evaluated coordinate (#294
    // review; #249 covers the back-reference case the feeds exist
    // for).
    // A walked row: the row, its type, the evaluated endpoint, and the
    // control cells captured at the row's own point in the walk.
    type EvaldRow<'a> = (&'a vsdx::Row, &'a str, (f64, f64), [Option<f64>; 4]);
    let evald: Vec<EvaldRow<'_>> = rows
        .iter()
        .filter(|r| r.t.is_some() && !r.is_deleted())
        .map(|row| {
            let t = row.t.as_deref().unwrap_or("");
            let (mx, my) = (
                row_num(env, row, "X").unwrap_or(0.0),
                row_num(env, row, "Y").unwrap_or(0.0),
            );
            let (x, y) = rel_adjust(t, mx, my, width, height);
            // Feed the evaluated coordinates back into the env (#249):
            // later rows reference earlier ones section-qualified
            // (`Geometry1.X2`), and the stale cached V would keep the
            // whole chain at the authored pose.
            env.insert(format!("{}.X{}", gname, row.ix), x);
            env.insert(format!("{}.Y{}", gname, row.ix), y);
            let ctrl = ["A", "B", "C", "D"].map(|n| row_num(env, row, n));
            (row, t, (x, y), ctrl)
        })
        .collect();
    for (ri, (row, t, xy, ctrl)) in evald.iter().enumerate() {
        let (x, y) = *xy;
        // Any row type except SplineKnot ends an open spline (#188,
        // #185 review): flush the accumulated curve BEFORE the row's
        // own command so the path keeps its drawn order — a LineTo
        // after a knot used to land in `d` first, jumping the pen
        // back when the spline finally flushed.
        if *t != "SplineKnot" {
            flush_spline!();
        }
        match *t {
            "MoveTo" | "RelMoveTo" => {
                let _ = write!(d, "M {}", emit_pt((x, y)));
                cur = (x, y);
            }
            "LineTo" | "RelLineTo" => {
                let _ = write!(d, "L {}", emit_pt((x, y)));
                cur = (x, y);
            }
            "ArcTo" => {
                // Circular arc: |A| = sagitta (height of arc above the
                // chord). Positive A bulges RIGHT of the travel direction —
                // verified against the Process master's rounded rect, which
                // traverses CCW (y-up, interior on the left) with positive-A
                // corner arcs bulging outward.
                let h = ctrl[0].unwrap_or(0.0);
                let (sx, sy) = cur;
                let chord = ((x - sx).powi(2) + (y - sy).powi(2)).sqrt();
                // Route context (#294 review): the run continues
                // collinear INTO the arc and OUT of it — Visio writes
                // jumps mid-run. Only straight LineTo neighbors count
                // (curved endpoint chords can lie about the route),
                // and zero-length degenerate rows are skipped without
                // crossing a MoveTo; an isolated authored semicircle
                // stays a curve.
                let kind_of = |tt: &str| -> u8 {
                    if tt == "MoveTo" || tt == "RelMoveTo" {
                        0
                    } else if tt == "LineTo" || tt == "RelLineTo" {
                        1
                    } else {
                        2
                    }
                };
                let mut prev_dir = None;
                let mut pi = ri as isize - 1;
                while pi >= 1 {
                    let (_, pt, pp, _) = evald[pi as usize];
                    if kind_of(pt) != 1 {
                        break;
                    }
                    let (_, _, p0, _) = evald[pi as usize - 1];
                    let pd = (pp.0 - p0.0, pp.1 - p0.1);
                    if pd.0.hypot(pd.1) > 1e-9 {
                        prev_dir = Some(pd);
                        break;
                    }
                    pi -= 1;
                }
                let mut next_dir = None;
                let mut ni = ri + 1;
                while ni < evald.len() {
                    let (_, nt, ne, _) = evald[ni];
                    if kind_of(nt) != 1 {
                        break;
                    }
                    let p = if ni == ri + 1 {
                        (x, y)
                    } else {
                        evald[ni - 1].2
                    };
                    let nd = (ne.0 - p.0, ne.1 - p.1);
                    if nd.0.hypot(nd.1) > 1e-9 {
                        next_dir = Some(nd);
                        break;
                    }
                    ni += 1;
                }
                // The radius needs no frame conversion: every geometry
                // frame is INCHES (master-local, instance-local, page —
                // the frame scales are geometric RESCALES applied after
                // authoring), and the stored arc's A was authored at
                // the page's radius.
                let radius_in_frame = jump_radius;
                // PR #299 review: BOTH radii gate the strip — the
                // classifier itself selects the arc's axis radius, so a
                // zero on one axis only kills that axis's strips.
                if (jump_radius.0 > 0.0 || jump_radius.1 > 0.0)
                    // Connector-ness gates the kind: artwork arcs never
                    // strip even with a resolved radius (#294 review).
                    && crate::geom::is_connector_in(doc, chain, s)
                    && crate::geom::is_stored_jump_arc(
                        h,
                        (x - sx, y - sy),
                        prev_dir,
                        next_dir,
                        radius_in_frame.0,
                        radius_in_frame.1,
                    )
                {
                    // #294: a stored jump arc straightens — the ink
                    // follows the run, and the jump overlay draws the
                    // ONE arc at the current crossing with the page's
                    // own radius. A stored arc stops lingering after a
                    // move.
                    let _ = write!(d, "L {}", emit_pt((x, y)));
                } else if h.abs() < 1e-9 || chord < 1e-9 {
                    let _ = write!(d, "L {}", emit_pt((x, y)));
                } else {
                    // On-arc point from the sagitta.
                    let mid = ((sx + x) / 2.0, (sy + y) / 2.0);
                    let dir = ((x - sx) / chord, (y - sy) / chord);
                    let right = (dir.1, -dir.0); // rot90 CW: right of travel
                    let m = (mid.0 + h * right.0, mid.1 + h * right.1);
                    let radius = (chord.powi(2) + 4.0 * h * h) / (8.0 * h.abs());
                    let large = h.abs() > chord / 2.0;
                    // cross > 0 (y-up frame) = CCW; path frame is mirrored → sweep 0.
                    let cross = (m.0 - sx) * (y - m.1) - (m.1 - sy) * (x - m.0);
                    let sweep = if cross > 0.0 { 0 } else { 1 };
                    let r = sc((radius, radius)).0.max(1e-6);
                    let e = emit_pt((x, y));
                    let _ = write!(
                        d,
                        "A {r:.5} {r:.5} 0 {la} {sw} {e}",
                        la = large as u8,
                        sw = sweep,
                        e = e.trim_end()
                    );
                }
                cur = (x, y);
            }
            "EllipticalArcTo" | "RelEllipticalArcTo" => {
                // (Authored unit codes for future writers: D carries
                // unit 32, C the angle unit 81 — MisterDiagram #233
                // rider; we only read today, preserve-bytes covers
                // round-trips.)
                let m = (
                    ctrl[0].unwrap_or((cur.0 + x) / 2.0),
                    ctrl[1].unwrap_or((cur.1 + y) / 2.0),
                );
                let c_ang = ctrl[2].unwrap_or(0.0);
                let d_cell = ctrl[3];
                // #233: an authored D=0 is a ZERO-WIDTH arc — the
                // minor axis collapses onto the major, so the arc IS
                // the endpoint chord. Visio draws the chord even when
                // the A/B control sits off it (MisterDiagram M5.50);
                // the old fallback's quadratic-through-m doubled back.
                // Corpus-common: 887 masters / 18,860 visible rows.
                if d_cell.is_some_and(|d| d.abs() < 1e-9) {
                    let _ = write!(d, "L {}", emit_pt((x, y)));
                    cur = (x, y);
                    continue;
                }
                // D is the ratio of the axis ALONG C to the axis across
                // it ([MS-VSDX] EllipticalArcTo; Visio 2021 witness t14:
                // through (0,.5)-(1,1)-(2,.5) in a 2×1 box, D=2 draws the
                // flat half-ellipse the box encloses, D=0.5 a tall one).
                // The solver takes across/along.
                let ratio = 1.0 / d_cell.unwrap_or(1.0).abs().max(1e-6);
                // Sane bounds: the arc must fit (loosely) inside the
                // shape's scaled extent — a solved ellipse or on-arc point
                // far outside means the numeric solver diverged on a tiny
                // or near-degenerate arc; fall back to the quadratic.
                let (ex, ey) = (x * scale.0, y * scale.1);
                let (mx, my) = (m.0 * scale.0, m.1 * scale.1);
                let bound_x = 3.0 * (width * scale.0).max(1e-6);
                let bound_y = 3.0 * (height * scale.1).max(1e-6);
                let in_bounds = |v: f64, b: f64| v.abs() <= b;
                match solve_ellipse_arc(cur, m, (x, y), c_ang, ratio, scale) {
                    Some((rx, ry, rot_deg, large, sweep))
                        if rx.max(ry)
                            <= 20.0
                                * ((x - cur.0).powi(2) + (y - cur.1).powi(2)).sqrt().max(1e-9)
                            && in_bounds(ex, bound_x)
                            && in_bounds(ey, bound_y)
                            && in_bounds(mx, bound_x)
                            && in_bounds(my, bound_y)
                            && rx.max(ry) <= bound_x.max(bound_y) =>
                    {
                        let e = emit_pt((x, y));
                        let _ = write!(
                            d,
                            "A {rx:.5} {ry:.5} {rot_deg:.4} {la} {sw} {e}",
                            la = large as u8,
                            sw = sweep,
                            e = e.trim_end()
                        );
                    }
                    _ => {
                        // The solver diverged on a genuinely 2-D arc
                        // (tiny or wild): quadratic Bézier through m.
                        // Authored zero-width (D=0) never reaches here
                        // — it short-circuits to the chord (#233).
                        let q = emit_pt(m);
                        let e = emit_pt((x, y));
                        let _ = write!(d, "Q {} {}", q.trim_end(), e.trim_end());
                    }
                }
                cur = (x, y);
            }
            "Ellipse" => {
                // (X,Y) center, (A,B) major-axis endpoint, (C,D) minor-axis endpoint.
                let a = ctrl[0].unwrap_or(x + 1.0);
                let b = ctrl[1].unwrap_or(y);
                let c2 = ctrl[2].unwrap_or(x);
                let d2 = ctrl[3].unwrap_or(y + 1.0);
                let rx = ((a - x).powi(2) + (b - y).powi(2)).sqrt().max(1e-6);
                let ry = ((c2 - x).powi(2) + (d2 - y).powi(2)).sqrt().max(1e-6);
                let ang = (b - y).atan2(a - x); // major-axis angle, y-up frame
                                                // Anisotropic instance scaling of the ellipse.
                let (rx2, ry2, ang2) =
                    transform_ellipse(rx.max(ry), (rx.min(ry)) / rx.max(ry), ang, scale);
                let (rx2, ry2) = if rx >= ry { (rx2, ry2) } else { (ry2, rx2) };
                let p = sc((x, y));
                // A full-ellipse row is one closed subpath (two half
                // arcs) of the section's path, so it joins the shape's
                // even-odd fill like any other closed contour — an
                // ellipse section inside a rectangle section punches a
                // hole, two concentric ellipse sections make a ring
                // (Visio 2021 witness, t07; the "no entry" signs of the
                // network stencils). The stroke stays per section.
                let (cx, cy) = (p.0, -p.1);
                let (rx, ry) = (rx2.max(1e-6), ry2.max(1e-6));
                let rot = -ang2; // mirror frame
                let (ex, ey) = (rx * rot.cos(), rx * rot.sin());
                let deg = rot.to_degrees();
                let _ = write!(
                    d,
                    "M {:.5} {:.5} A {rx:.5} {ry:.5} {deg:.4} 1 0 {:.5} {:.5} A {rx:.5} {ry:.5} {deg:.4} 1 0 {:.5} {:.5} Z ",
                    cx + ex,
                    cy + ey,
                    cx - ex,
                    cy - ey,
                    cx + ex,
                    cy + ey,
                );
                cur = (x, y);
            }
            "CubicBezTo" | "RelCubBezTo" => {
                // Controls (A,B) and (C,D); end (X,Y) — all possibly
                // relative fractions of Width/Height.
                let (a, b) = rel_adjust(
                    t,
                    ctrl[0].unwrap_or(cur.0),
                    ctrl[1].unwrap_or(cur.1),
                    width,
                    height,
                );
                let (c2, d2) = rel_adjust(
                    t,
                    ctrl[2].unwrap_or(x),
                    ctrl[3].unwrap_or(y),
                    width,
                    height,
                );
                let _ = write!(
                    d,
                    "C {}{}{}",
                    emit_pt((a, b)),
                    emit_pt((c2, d2)),
                    emit_pt((x, y)).trim_end()
                );
                cur = (x, y);
            }
            "QuadBezTo" | "RelQuadBezTo" => {
                let (a, b) = rel_adjust(
                    t,
                    ctrl[0].unwrap_or((cur.0 + x) / 2.0),
                    ctrl[1].unwrap_or((cur.1 + y) / 2.0),
                    width,
                    height,
                );
                let _ = write!(
                    d,
                    "Q {} {}",
                    emit_pt((a, b)).trim_end(),
                    emit_pt((x, y)).trim_end()
                );
                cur = (x, y);
            }
            "SplineStart" => {
                // #188: a SplineStart opens a spline (§2.4.2.15): X/Y
                // is the SECOND control point (the previous row's end
                // is the first); A/B/C/D are second/first/last knots
                // and the degree. Any non-SplineKnot row flushes it
                // (the loop-top flush).
                spline_acc = Some(crate::curve::SplineAcc::start(
                    cur,
                    (x, y),
                    ctrl[0].unwrap_or(0.0),
                    ctrl[1].unwrap_or(0.0),
                    ctrl[2].unwrap_or(1.0),
                    ctrl[3].unwrap_or(3.0),
                ));
                cur = (x, y);
            }
            "SplineKnot" => {
                match spline_acc.as_mut() {
                    Some(acc) => {
                        acc.knot((x, y), ctrl[0].unwrap_or(0.0));
                        cur = (x, y);
                    }
                    None => {
                        // A knot row without an opener degenerates to
                        // the chord (the v0.2 behavior).
                        let _ = write!(d, "L {}", emit_pt((x, y)));
                        cur = (x, y);
                    }
                }
            }
            "NURBSTo" => {
                // #188: one self-contained rational B-spline row
                // (§2.4.2.7). The E cell's NURBS() formula carries the
                // interior points/knots/weights; the row cells frame
                // them (C/D first knot/weight, A/B second-to-last
                // knot/last weight, X/Y the endpoint). The loop-top
                // flush already ended any open spline.
                let e_text = row
                    .cell("E")
                    .and_then(|c| c.v.clone().or_else(|| c.f.clone()))
                    .unwrap_or_default();
                let mut drew = false;
                if let Some(f) = crate::curve::parse_nurbs(&e_text) {
                    // Fractions are of the GEOMETRY frame's own size:
                    // the master's dims in the master frame (`width/
                    // scale` recovers them; `emit_pt` applies the
                    // instance scale exactly once), the instance's
                    // own dims inline (scale = 1). Scaling by the
                    // instance dims here too would square the stretch
                    // (#218 review round).
                    let fx = if f.x_type == 0.0 {
                        frame_dim(width, scale.0)
                    } else {
                        1.0
                    };
                    let fy = if f.y_type == 0.0 {
                        frame_dim(height, scale.1)
                    } else {
                        1.0
                    };
                    let mut points = vec![cur];
                    let mut knots = vec![ctrl[2].unwrap_or(0.0)];
                    let mut weights = vec![ctrl[3].unwrap_or(1.0)];
                    for (gx, gy, gk, gw) in &f.points {
                        points.push((gx * fx, gy * fy));
                        knots.push(*gk);
                        weights.push(*gw);
                    }
                    knots.push(ctrl[0].unwrap_or(0.0));
                    knots.push(f.last_knot);
                    weights.push(ctrl[1].unwrap_or(1.0));
                    points.push((x, y));
                    let built = crate::curve::SplineData {
                        degree: f.degree,
                        points,
                        weights,
                        knots,
                    }
                    .sanitized();
                    if let Some(data) = built {
                        let pts = data.flatten(16);
                        for p in pts.iter().skip(1) {
                            let _ = write!(d, "L {}", emit_pt(*p));
                        }
                        if let Some(last) = pts.last() {
                            cur = *last;
                        }
                        drew = true;
                    }
                }
                if !drew {
                    let _ = write!(d, "L {}", emit_pt((x, y)));
                    cur = (x, y);
                }
            }
            "PolylineTo" => {
                // The A cell's POLYLINE() formula carries the interior
                // vertices; fractions are of the geometry frame's own
                // size (master dims in the master frame — the same
                // per-axis rule as NURBSTo above), and the row's X/Y
                // is the final vertex. Unparseable text keeps the
                // straight chord.
                let a_text = row
                    .cell("A")
                    .and_then(|c| c.v.clone().or_else(|| c.f.clone()))
                    .unwrap_or_default();
                // Repeated points (a vertex on the previous point or on
                // the endpoint) would leave zero-length segments, and
                // `path_frames` takes no tangent from those: an open
                // connector lost its arrowheads (CodeRabbit on PR #329).
                let same = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).hypot(a.1 - b.1) < 1e-9;
                let mut last = cur;
                if let Some((xt, yt, pts)) = crate::curve::parse_polyline(&a_text) {
                    let fx = if xt == 0.0 {
                        frame_dim(width, scale.0)
                    } else {
                        1.0
                    };
                    let fy = if yt == 0.0 {
                        frame_dim(height, scale.1)
                    } else {
                        1.0
                    };
                    for (px, py) in pts {
                        let v = (px * fx, py * fy);
                        if same(v, last) {
                            continue;
                        }
                        let _ = write!(d, "L {}", emit_pt(v));
                        last = v;
                    }
                }
                // Any number of trailing endpoint copies collapse into
                // the endpoint (Greptile on PR #329).
                if !same(last, (x, y)) {
                    let _ = write!(d, "L {}", emit_pt((x, y)));
                }
                cur = (x, y);
            }
            "InfiniteLine" => {
                // v0.2: approximate infinite lines with a chord.
                let _ = write!(d, "L {}", emit_pt((x, y)));
                cur = (x, y);
            }
            _ => {}
        }
    }
    // A spline open through the section's end flushes here.
    flush_spline_tail!();
    // Rounding (Line Format): Visio rounds every corner of a straight
    // polyline walk with this radius — rounded boxes, pill-shaped
    // container headings, connector bends (Visio 2021 witness, t06).
    // `Rounding` is a shape-level cell, so a formula evaluates in the
    // INSTANCE env even when the rows come from the master: the path is
    // already in instance units (Greptile on PR #344). Rows that sample
    // a curve into `L` segments (NURBS, splines) are curves, not
    // corners, and stay as drawn.
    let own_env;
    let rounding_env: &vsdx::formula::Env = if inherited_frame {
        own_env = shape_env(doc, s, chain);
        &own_env
    } else {
        env
    };
    // Deleted rows draw nothing, so they never block the rounding
    // (Greptile on PR #344).
    let sampled_curve = rows.iter().any(|r| {
        !r.is_deleted()
            && matches!(
                r.t.as_deref(),
                Some("NURBSTo" | "SplineStart" | "SplineKnot")
            )
    });
    // Formula first: an inherited cell's cached value is the MASTER's
    // (`Width/10` cached as 0.1 on a 1 in master), so the instance env
    // decides and the cache is only the fallback (Greptile on PR #344).
    let rounding = doc
        .resolve_cell(s, chain, "Rounding")
        .and_then(|c| {
            c.f.as_deref()
                .filter(|f| *f != "Inh" && *f != "No Formula")
                .and_then(|f| vsdx::formula::eval(f, rounding_env).ok())
                .and_then(|v| v.as_f64())
                .or_else(|| cell_num(&c, rounding_env))
        })
        .filter(|r| r.is_finite() && *r > 0.0 && !sampled_curve);
    if let Some(r) = rounding {
        if let Some(rounded) = round_corners(&d, r) {
            d = rounded;
        }
    }
    if !d.is_empty() {
        // Connector arrowheads: BeginArrow/EndArrow cells (0 = none, ≥1 an
        // arrow; sizes 0–6, 2 = medium).
        let begin_arrow = doc
            .resolve_cell(s, chain, "BeginArrow")
            .and_then(|c| cell_num(&c, env))
            .unwrap_or(0.0);
        let end_arrow = doc
            .resolve_cell(s, chain, "EndArrow")
            .and_then(|c| cell_num(&c, env))
            .unwrap_or(0.0);
        let bsz = doc
            .resolve_cell(s, chain, "BeginArrowSize")
            .and_then(|c| cell_num(&c, env))
            .unwrap_or(2.0);
        let esz = doc
            .resolve_cell(s, chain, "EndArrowSize")
            .and_then(|c| cell_num(&c, env))
            .unwrap_or(2.0);
        // #185/#170: physical per-value arrow geometry. The head unit is
        // LineWeight + C[EndArrowSize] (C = [1.44, 1.8, 2.52, 3.24, 3.96,
        // 9, 18] pt — witnessed across all 45 types and sizes 0–6), each
        // gallery kind draws at its measured proportions in that unit
        // ([`GALLERY`]), and the shaft trims back by the per-kind
        // setback ([`arrow_setback`]) so hollow heads keep their
        // interiors clear — the open diamonds trim two pens short of
        // their rear vertex ([`diamond_shaft_trim`], #239). Evidence:
        // the owner-authored 600 dpi print (Sample/test files/
        // arrows.vsdx + arrows.png — the authoritative oracle)
        // cross-checked against the #223 parity witnesses; see
        // Analysis/arrow-gallery-visio2016.md.
        let kind = |a: f64| -> Option<i64> {
            if a < 0.5 || (46.0..=50.5).contains(&a) {
                None
            } else {
                Some(a.round() as i64)
            }
        };
        // Zero-weight exception (the SVG rule): the head draws with the
        // 0.75 pt nominal pen, so its unit is C[size] + 0.75 pt.
        let head_weight = if line_weight > 0.0 {
            line_weight
        } else {
            0.75 / 72.0
        };
        let head_unit = |sz: f64| -> f64 {
            let i = (sz.round() as i64).clamp(0, 6) as usize;
            head_weight + ARROW_SIZE_PT[i] / 72.0
        };
        let end_setback = kind(end_arrow).map_or(0.0, |k| {
            diamond_shaft_trim(k, arrow_setback(k) * head_unit(esz), head_weight)
        });
        let begin_setback = kind(begin_arrow).map_or(0.0, |k| {
            arrow_begin_setback(diamond_shaft_trim(
                k,
                arrow_setback(k) * head_unit(bsz),
                head_weight,
            ))
        });
        // The shaft stops at (or under) the head's rear; the heads
        // themselves anchor at the ORIGINAL endpoints, so the trim must
        // not move them. Closed loops and multi-subpath data trim
        // never (Visio gives them no heads either — #227 tracks that).
        let shaft = if end_setback > 0.0 || begin_setback > 0.0 {
            trim_path(&d, begin_setback, end_setback)
        } else {
            d.clone()
        };
        // #228: a fill-eligible section's `d` (UNtrimmed — setbacks
        // trim the shaft only) joins the shape's ONE alternate-fill
        // path; NoFill sections opt out here, NoLine changes nothing
        // for the fill, and the implicit close SVG already applies to
        // open subpaths leaves the stroke open per section.
        // 1-D shapes fill their closed subpaths only (see `one_d`
        // above); closure is judged per subpath, not per section
        // (Greptile on PR #328).
        let fill_part = if one_d {
            closed_subpaths(&d)
        } else {
            d.clone()
        };
        if !no_fill && fill != "none" && !fill_part.trim().is_empty() {
            paint.fill_d.push_str(&fill_part);
            paint.fill_d.push(' ');
            paint.fill = fill.clone();
            paint.fill_opacity = fill_alpha;
            paint.fill_tiles = match &fill_gradient {
                Some(GradientPaint::Tiles(t)) => Some(t.clone()),
                _ => hatch_tiles.clone(),
            };
        }
        if !no_line {
            let _ = writeln!(
                paint.strokes,
                r#"<path d="{}" fill="none" {stroke_a}/>"#,
                escape_attr(close_returning_path(shaft.trim()).trim())
            );
        }
        // Heads as inline translate·rotate·scale groups of the
        // unit-space parts (begin faces outward along the reversed
        // start tangent, end along the end tangent) — no marker defs,
        // so the size law holds without markerUnits scaling. #227: a
        // CLOSED walk gets no frames — and so no heads — at all. A
        // fully transparent line (LineColorTrans = 1) hides its heads
        // with the shaft (Greptile on PR #327).
        if let Some((begin, end)) = path_frames(&d).filter(|_| line_alpha > 0.0) {
            // LineColorTrans fades the heads with the shaft (the head
            // parts set no opacity of their own, so the group's
            // inherited fill-/stroke-opacity applies).
            let faded = line_alpha < 1.0
                && !no_line
                && (kind(begin_arrow).is_some() || kind(end_arrow).is_some());
            if faded {
                let _ = writeln!(
                    paint.strokes,
                    r#"<g fill-opacity="{line_alpha:.3}" stroke-opacity="{line_alpha:.3}">"#
                );
            }
            if let Some(k) = kind(begin_arrow) {
                emit_arrow_head(
                    &mut paint.strokes,
                    k,
                    bsz,
                    head_weight,
                    &line_color,
                    begin.0,
                    begin.1,
                );
            }
            if let Some(k) = kind(end_arrow) {
                emit_arrow_head(
                    &mut paint.strokes,
                    k,
                    esz,
                    head_weight,
                    &line_color,
                    end.0,
                    end.1,
                );
            }
            if faded {
                paint.strokes.push_str("</g>\n");
            }
        }
    }
}

/// One drawable piece of an arrow head (#199, #185): a path — filled
/// when `filled`, stroked (fill="none") otherwise — or a circle
/// (filled dot vs open ring). Composite heads (the dimension set's
/// tick + glyph rows) are just several parts in one head.
/// Coordinates live in MARKER UNITS with the tip at the origin, +x
/// pointing PAST the tip (the body extends into -x) and +y down; the
/// head frame is placed with a translate·rotate·scale group, so the
/// same table serves both ends without mirroring.
struct HeadPart {
    d: &'static str,
    circle: Option<(f64, f64, f64)>,
    filled: bool,
}

/// A filled path part.
const fn fp(d: &'static str) -> HeadPart {
    HeadPart {
        d,
        circle: None,
        filled: true,
    }
}

/// A stroked (open) path part.
const fn sp(d: &'static str) -> HeadPart {
    HeadPart {
        d,
        circle: None,
        filled: false,
    }
}

/// An open ring (stroke-only circle).
const fn ring(cx: f64, cy: f64, r: f64) -> HeadPart {
    HeadPart {
        d: "",
        circle: Some((cx, cy, r)),
        filled: false,
    }
}

/// A filled dot (circle).
const fn dot(cx: f64, cy: f64, r: f64) -> HeadPart {
    HeadPart {
        d: "",
        circle: Some((cx, cy, r)),
        filled: true,
    }
}

/// The Visio 2016 Begin/EndArrow gallery in unit space (#185): every
/// kind at its measured proportions in the head unit
/// LineWeight + C[EndArrowSize]. Pinned against the owner-authored
/// 600 dpi print (`Sample/test files/arrows.png` — the fixture
/// `arrows.vsdx` carries EndArrow = n per labeled line; the print is
/// the authoritative oracle) and cross-checked against the #223
/// parity witnesses; artwork ours, no Microsoft assets.
///
/// Geometry conventions: the tip sits at the origin, the body
/// extends into -x, +y is down. Per-kind proportions are the
/// measured ones — triangles 2 units tall with per-kind lengths
/// (2/15: 1, 3/4/16: 2, 12/13/14: 3), ticks/bars 1.5 tall, circles
/// r 0.75 (the 35–37 dots 0.825), the diamond 4.5 × 2.25 — and the
/// 5/6/8/18 rears are circular arcs (concave for 5/8, convex —
/// centered at the tip, R = √5 — for 6/18) emitted as exact cubic
/// approximations (k = 4/3·(√5−2)). Corrections the print forced
/// against the earlier table: 12 is ONE wide open V (len 3) and 43
/// is the double chevron (len 4); 10/11/20/21 straddle the tip
/// centered; 9's tick leans "/" through the tip while 23's leans
/// "\" behind it.
const GALLERY: &[(i64, &[HeadPart])] = &[
    // 1 — narrow open V (unfilled 2-prong), len 1.
    (1, &[sp("M-1,-1 L0,0 L-1,1")]),
    // 2 — small filled triangle, len 1.
    (2, &[fp("M0,0 L-1,-1 L-1,1 Z")]),
    // 3 — open wide V, len 2.
    (3, &[sp("M-2,-1 L0,0 L-2,1")]),
    // 4 — the classic filled triangle (the corpus default), len 2.
    (4, &[fp("M0,0 L-2,-1 L-2,1 Z")]),
    // 5 — filled triangle, concave circular rear (the stealth-dart
    // family; the native SVG alias 8→5 and the print agree).
    (
        5,
        &[fp("M0,0 L-2,-1 C-1.68524,-0.37049 -1.68524,0.37049 -2,1 Z")],
    ),
    // 6 — filled triangle with a convex rear arc centered at the
    // tip: reads "long" next to 4's flat base.
    (
        6,
        &[fp("M0,0 L-2,-1 C-2.31476,-0.37049 -2.31476,0.37049 -2,1 Z")],
    ),
    // 7 — open fork (curved barbs).
    (
        7,
        &[sp("M-2,-1 C-1.5,-0.3 -0.7,0 0,0 C-0.7,0 -1.5,0.3 -2,1")],
    ),
    // 8 — the stealth dart: the concave-rear dart (the 5 glyph).
    (
        8,
        &[fp("M0,0 L-2,-1 C-1.68524,-0.37049 -1.68524,0.37049 -2,1 Z")],
    ),
    // 9 — the slanted dimension tick: 45° through the tip ("/").
    (9, &[sp("M1,-1 L-1,1")]),
    // 10 — the filled circle, centered ON the endpoint.
    (10, &[dot(0.0, 0.0, 0.75)]),
    // 11 — the filled square, centered ON the endpoint.
    (
        11,
        &[fp("M-0.75,-0.75 L0.75,-0.75 L0.75,0.75 L-0.75,0.75 Z")],
    ),
    // 12 — ONE wide open V (len 3; the old "double chevron" label
    // was a print misread).
    (12, &[sp("M-3,-1 L0,0 L-3,1")]),
    // 13 — single filled triangle, plain base, len 3 (NOT a double —
    // the #169 report).
    (13, &[fp("M0,0 L-3,-1 L-3,1 Z")]),
    // 14–18 — the open-head set: CLOSED triangle outlines whose
    // interiors stay hollow (the shaft trims at the base).
    (14, &[sp("M0,0 L-3,-1 L-3,1 Z")]),
    (15, &[sp("M0,0 L-1,-1 L-1,1 Z")]),
    (16, &[sp("M0,0 L-2,-1 L-2,1 Z")]),
    // 17 — open triangle, concave rear (cubic through ±0.33).
    (17, &[sp("M0,0 L-2,-1 C-1.55,-0.33 -1.55,0.33 -2,1 Z")]),
    // 18 — open triangle, convex rear arc (centered at the tip).
    (
        18,
        &[sp("M0,0 L-2,-1 C-2.31476,-0.37049 -2.31476,0.37049 -2,1 Z")],
    ),
    // 19 — open barbed head: curved barbs closed by a concave rear.
    (
        19,
        &[sp(
            "M-2,-1 C-1.5,-0.3 -0.7,0 0,0 C-0.7,0 -1.5,0.3 -2,1 C-1.55,0.3 -1.55,-0.3 -2,-1 Z",
        )],
    ),
    // 20 — the open ring, centered ON the endpoint.
    (20, &[ring(0.0, 0.0, 0.75)]),
    // 21 — the open square, centered ON the endpoint.
    (
        21,
        &[sp("M-0.75,-0.75 L0.75,-0.75 L0.75,0.75 L-0.75,0.75 Z")],
    ),
    // 22 — the open diamond, 4.5 × 2.25.
    (22, &[sp("M0,0 L-2.25,-1.125 L-4.5,0 L-2.25,1.125 Z")]),
    // 23 — single slanted tick: 45° behind the tip ("\"), shaft
    // through.
    (23, &[sp("M-2.9,-1 L-0.9,1")]),
    // 24–26 — perpendicular ticks at 1.5 + 0.75·i behind the tip.
    (24, &[sp("M-1.5,-0.75 L-1.5,0.75")]),
    (25, &[sp("M-1.5,-0.75 L-1.5,0.75 M-2.25,-0.75 L-2.25,0.75")]),
    (
        26,
        &[sp(
            "M-1.5,-0.75 L-1.5,0.75 M-2.25,-0.75 L-2.25,0.75 M-3,-0.75 L-3,0.75",
        )],
    ),
    // 27 — the three-pronged open fork (crow's foot).
    (27, &[sp("M0,-0.75 L-1.5,0 L0,0.75")]),
    // 28 — crow's-foot fork + perpendicular tick behind it.
    (
        28,
        &[sp("M0,-0.75 L-1.5,0 L0,0.75"), sp("M-2,-0.75 L-2,0.75")],
    ),
    // 29 — crow's-foot fork, then the open ring at 2.25.
    (
        29,
        &[sp("M0,-0.75 L-1.5,0 L0,0.75"), ring(-2.25, 0.0, 0.75)],
    ),
    // 30 — open ring at 2.25, then a tick toward the tip.
    (
        30,
        &[ring(-2.25, 0.0, 0.75), sp("M-0.75,-0.75 L-0.75,0.75")],
    ),
    // 31–33 — 1–3 ticks behind an open ring at the tip.
    (
        31,
        &[sp("M-2.25,-0.75 L-2.25,0.75"), ring(-0.75, 0.0, 0.75)],
    ),
    (
        32,
        &[
            sp("M-2.25,-0.75 L-2.25,0.75 M-3,-0.75 L-3,0.75"),
            ring(-0.75, 0.0, 0.75),
        ],
    ),
    (
        33,
        &[
            sp("M-2.25,-0.75 L-2.25,0.75 M-3,-0.75 L-3,0.75 M-3.75,-0.75 L-3.75,0.75"),
            ring(-0.75, 0.0, 0.75),
        ],
    ),
    // 34 — open diamond (1.5–6) + open ring at the tip.
    (
        34,
        &[
            ring(-0.75, 0.0, 0.75),
            sp("M-1.5,0 L-3.75,-1.125 L-6,0 L-3.75,1.125 Z"),
        ],
    ),
    // 35–37 — 1–3 ticks behind a filled 0.825-radius dot at the tip.
    (
        35,
        &[dot(-0.75, 0.0, 0.825), sp("M-2.25,-0.75 L-2.25,0.75")],
    ),
    (
        36,
        &[
            dot(-0.75, 0.0, 0.825),
            sp("M-2.25,-0.75 L-2.25,0.75 M-3,-0.75 L-3,0.75"),
        ],
    ),
    (
        37,
        &[
            dot(-0.75, 0.0, 0.825),
            sp("M-2.25,-0.75 L-2.25,0.75 M-3,-0.75 L-3,0.75 M-3.75,-0.75 L-3.75,0.75"),
        ],
    ),
    // 38 — OPEN diamond (1.5–6) + the filled dot. #239's fixture pass:
    // the 600 dpi print's center-line runs a gap through the diamond
    // interior (no fill between the apex and the shaft stub) — the
    // earlier "filled" gallery read was the low-res screenshot's.
    (
        38,
        &[
            dot(-0.75, 0.0, 0.825),
            sp("M-1.5,0 L-3.75,-1.125 L-6,0 L-3.75,1.125 Z"),
        ],
    ),
    // 39 — double filled triangle (two solid heads).
    (39, &[fp("M0,0 L-2,-1 L-2,1 Z M-2,0 L-4,-1 L-4,1 Z")]),
    // 40 — double open triangle (two hollow heads).
    (40, &[sp("M0,0 L-2,-1 L-2,1 Z M-2,0 L-4,-1 L-4,1 Z")]),
    // 41 — the open ring (front edge at the tip).
    (41, &[ring(-0.75, 0.0, 0.75)]),
    // 42 — the filled dot (front edge at the tip).
    (42, &[dot(-0.75, 0.0, 0.75)]),
    // 43 — the open double chevron (two 2-unit V's, len 4; each V is
    // corner→apex→corner so no base edge closes it).
    (43, &[sp("M-2,-1 L0,0 L-2,1 M-4,-1 L-2,0 L-4,1")]),
    // 44 — perpendicular tick behind a single open V.
    (44, &[sp("M-2,-1 L0,0 L-2,1"), sp("M-3,-0.75 L-3,0.75")]),
    // 45 — perpendicular tick behind the double chevron.
    (
        45,
        &[
            sp("M-2,-1 L0,0 L-2,1 M-4,-1 L-2,0 L-4,1"),
            sp("M-5,-0.75 L-5,0.75"),
        ],
    ),
];

/// The unmapped-value fallback: the classic filled triangle.
const FALLBACK_TRIANGLE: &[HeadPart] = &[fp("M0,0 L-2,-1 L-2,1 Z")];

/// The gallery lookup: `None` = no head (values 46–50 print bare
/// line ends); unmapped values fall back to the filled triangle.
fn arrow_spec(kind: i64) -> Option<&'static [HeadPart]> {
    if (46..=50).contains(&kind) {
        return None;
    }
    Some(
        GALLERY
            .iter()
            .find(|(k, _)| *k == kind)
            .map_or(FALLBACK_TRIANGLE, |(_, parts)| *parts),
    )
}

/// EndArrowSize 0–6, in points: the physical head unit is
/// LineWeight + C[size] — witnessed across all 45 head types and
/// sizes 0–6, independent of export scale, group resize, drawing
/// scale and dash pattern (#185).
const ARROW_SIZE_PT: [f64; 7] = [1.44, 1.8, 2.52, 3.24, 3.96, 9.0, 18.0];

/// The gallery's largest head reach in units (the 34/38 diamond's
/// rear vertex; kind 9 reaches 1 unit forward and the diamonds 1.125
/// sideways, both inside this square). The conservative bound the
/// ink-box walk pads around arrowed endpoints.
const ARROW_MAX_REACH_UNITS: f64 = 6.0;

/// The physical half-extent an arrowhead reaches beyond a line
/// endpoint: `reach × (LineWeight + C[size])` with the zero-weight
/// SVG floor — the same unit law [`emit_arrow_head`] draws with, so
/// the ink-box padding never clips a head.
fn arrow_head_reach(weight: f64, sz: f64) -> f64 {
    let head_weight = if weight > 0.0 { weight } else { 0.75 / 72.0 };
    let i = (sz.round() as i64).clamp(0, 6) as usize;
    ARROW_MAX_REACH_UNITS * (head_weight + ARROW_SIZE_PT[i] / 72.0)
}

/// Per-kind shaft setback in head units (#185): how far back from the
/// line end the shaft stops. Closed/hollow heads trim to their rear
/// (the measured values ARE the glyph rear extents); pass-through
/// heads — the open V/barbs (1, 3, 7), the ticks (9, 23–26), the
/// crow's-foot set (27/28), the chevrons (12, 43–45) — ride 0 and the
/// shaft runs beneath them to the tip. Filled heads trim under their
/// own fill, so the value only has to reach covered ground.
fn arrow_setback(kind: i64) -> f64 {
    match kind {
        2 | 15 => 1.0,
        4 | 16 => 2.0,
        5 | 8 | 17 => 1.75,
        6 | 18 => 2.25,
        10 | 11 | 20 | 21 | 42 => 0.75,
        13 | 14 | 29 | 30 => 3.0,
        19 => 1.65,
        22 => 4.5,
        31..=33 | 35..=37 | 41 => 1.5,
        34 | 38 => 6.0,
        39 | 40 => 4.0,
        _ => 0.0,
    }
}

/// The BEGIN head's setback: the end setback minus max(1% of it,
/// 0.36 pt) — Visio's begin end trims marginally less — floored at
/// zero (#185).
fn arrow_begin_setback(end_setback: f64) -> f64 {
    (end_setback - (end_setback * 0.01).max(0.36 / 72.0)).max(0.0)
}

/// #239: the open diamonds (22, 34/38) trim their shaft TWO PENS short
/// of the rear vertex, not at it. The 600 dpi fixture's ink shows the
/// stop where the shaft's ink would first touch the outline's ink —
/// the outline center one full pen off-axis, which the slope-½ rear
/// halves doubles: `rear − 2 × pen` (22's on-axis ink ends ≈3.79 u of
/// its 4.5 rear; 34/38's ≈5.45 of 6 — the model puts the geometric
/// stops at 3.77/5.27 and the butt-cap/AA residuals land inside
/// ±0.2 u; the flat-rear hollow heads keep the full extent because
/// their trim hides under the rear stroke). Floored at half the
/// setback — never in front of the diamond's widest point, so a fat
/// pen cannot push the stop into the wide half.
fn diamond_shaft_trim(kind: i64, setback: f64, head_weight: f64) -> f64 {
    if matches!(kind, 22 | 34 | 38) {
        (setback - 2.0 * head_weight).max(0.5 * setback)
    } else {
        setback
    }
}

fn path_command_arity(c: char) -> Option<usize> {
    match c {
        'M' | 'L' => Some(2),
        'Q' => Some(4),
        'C' => Some(6),
        'A' => Some(7),
        _ => None,
    }
}

/// Parse emitted path data into (letter, floats) commands; None when
/// anything deviates from the emitter's strict absolute M/L/Q/C/A
/// shape (the caller keeps the untrimmed path).
fn parse_path_commands(d: &str) -> Option<Vec<(char, Vec<f64>)>> {
    let mut cmds: Vec<(char, Vec<f64>)> = Vec::new();
    let mut cur: Option<(char, Vec<f64>)> = None;
    for tok in d.split_whitespace() {
        let mut chars = tok.chars();
        let first = chars.next().unwrap_or(' ');
        if chars.count() == 0 && first.is_ascii_alphabetic() {
            if let Some((c, f)) = cur.take() {
                if path_command_arity(c) != Some(f.len()) {
                    return None;
                }
                cmds.push((c, f));
            }
            cur = Some((first, Vec::new()));
        } else {
            cur.as_mut()?.1.push(tok.parse::<f64>().ok()?);
        }
    }
    if let Some((c, f)) = cur.take() {
        if path_command_arity(c) != Some(f.len()) {
            return None;
        }
        cmds.push((c, f));
    }
    Some(cmds)
}

/// One drawing segment: (letter, params, start, end). `M` resets the
/// current point without drawing.
type PathSeg = (char, Vec<f64>, (f64, f64), (f64, f64));

fn path_segments(cmds: &[(char, Vec<f64>)]) -> Option<Vec<PathSeg>> {
    let mut segs: Vec<PathSeg> = Vec::new();
    let mut cur: Option<(f64, f64)> = None;
    for (c, f) in cmds {
        match c {
            'M' => cur = Some((f[0], f[1])),
            'L' | 'Q' | 'C' | 'A' => {
                let s = cur?;
                let e = match c {
                    'Q' => (f[2], f[3]),
                    'C' => (f[4], f[5]),
                    'A' => (f[5], f[6]),
                    _ => (f[0], f[1]),
                };
                segs.push((*c, f.clone(), s, e));
                cur = Some(e);
            }
            _ => return None,
        }
    }
    Some(segs)
}

/// An SVG arc's center-form parameters: (rx, ry, φ rad, θ1, Δθ,
/// center). `s` is the arc's start point, `f` the full A parameters.
type ArcGeo = (f64, f64, f64, f64, f64, (f64, f64));

fn arc_geo(f: &[f64], s: (f64, f64)) -> Option<ArcGeo> {
    let (mut rx, mut ry) = (f[0], f[1]);
    if rx <= 0.0 || ry <= 0.0 {
        return None;
    }
    let phi = f[2].to_radians();
    let (sp, cp) = phi.sin_cos();
    let e = (f[5], f[6]);
    let (dx2, dy2) = ((s.0 - e.0) / 2.0, (s.1 - e.1) / 2.0);
    let x1 = cp * dx2 + sp * dy2;
    let y1 = -sp * dx2 + cp * dy2;
    let lam = x1 * x1 / (rx * rx) + y1 * y1 / (ry * ry);
    if lam > 1.0 {
        let r = lam.sqrt();
        rx *= r;
        ry *= r;
    }
    let den = rx * rx * y1 * y1 + ry * ry * x1 * x1;
    if den <= 1e-18 {
        return None; // coincident endpoints
    }
    let num = rx * rx * ry * ry - den;
    let sign = if f[3] != f[4] { 1.0 } else { -1.0 };
    let co = sign * (num / den).max(0.0).sqrt();
    let (cxp, cyp) = (co * rx * y1 / ry, -co * ry * x1 / rx);
    let center = (
        cp * cxp - sp * cyp + (s.0 + e.0) / 2.0,
        sp * cxp + cp * cyp + (s.1 + e.1) / 2.0,
    );
    let th1 = ((y1 - cyp) / ry).atan2((x1 - cxp) / rx);
    let mut dth = ((-y1 - cyp) / ry).atan2((-x1 - cxp) / rx) - th1;
    if f[4] == 0.0 && dth > 0.0 {
        dth -= std::f64::consts::TAU;
    }
    if f[4] != 0.0 && dth < 0.0 {
        dth += std::f64::consts::TAU;
    }
    Some((rx, ry, phi, th1, dth, center))
}

/// The point and tangent direction at parameter t ∈ [0, 1] of an arc.
fn arc_point_tangent(g: &ArcGeo, t: f64) -> ((f64, f64), (f64, f64)) {
    let (rx, ry, phi, th1, dth, c) = *g;
    let th = th1 + t * dth;
    let (st, ct) = th.sin_cos();
    let (sp, cp) = phi.sin_cos();
    let p = (
        c.0 + cp * rx * ct - sp * ry * st,
        c.1 + sp * rx * ct + cp * ry * st,
    );
    let dir = (-cp * rx * st - sp * ry * ct, -sp * rx * st + cp * ry * ct);
    let dir = if dth < 0.0 { (-dir.0, -dir.1) } else { dir };
    (p, dir)
}

/// Chord length of a sample polyline.
fn polyline_length(samples: &[(f64, f64)]) -> f64 {
    samples
        .windows(2)
        .map(|w| ((w[1].0 - w[0].0).powi(2) + (w[1].1 - w[0].1).powi(2)).sqrt())
        .sum()
}

/// Polyline approximation of one segment (16 chords) for arc-length
/// walks.
fn segment_samples(c: char, f: &[f64], s: (f64, f64)) -> Vec<(f64, f64)> {
    match c {
        'L' => vec![s, (f[0], f[1])],
        'Q' => (0..=16)
            .map(|i| {
                let t = i as f64 / 16.0;
                let mt = 1.0 - t;
                (
                    mt * mt * s.0 + 2.0 * mt * t * f[0] + t * t * f[2],
                    mt * mt * s.1 + 2.0 * mt * t * f[1] + t * t * f[3],
                )
            })
            .collect(),
        'C' => (0..=16)
            .map(|i| {
                let t = i as f64 / 16.0;
                let mt = 1.0 - t;
                (
                    mt * mt * mt * s.0
                        + 3.0 * mt * mt * t * f[0]
                        + 3.0 * mt * t * t * f[2]
                        + t * t * t * f[4],
                    mt * mt * mt * s.1
                        + 3.0 * mt * mt * t * f[1]
                        + 3.0 * mt * t * t * f[3]
                        + t * t * t * f[5],
                )
            })
            .collect(),
        'A' => arc_geo(f, s)
            .map(|g| {
                (0..=16)
                    .map(|i| arc_point_tangent(&g, i as f64 / 16.0).0)
                    .collect()
            })
            .unwrap_or_else(|| vec![s, (f[5], f[6])]),
        _ => vec![s, s],
    }
}

/// Parameter t for the arc-length `target` along a segment's sample
/// polyline (exact for lines, sub-chord for curves).
fn sample_time_at_length(samples: &[(f64, f64)], target: f64) -> f64 {
    let n = samples.len().saturating_sub(1) as f64;
    if n <= 0.0 {
        return 0.0;
    }
    let mut acc = 0.0;
    for i in 0..samples.len() - 1 {
        let (a, b) = (samples[i], samples[i + 1]);
        let l = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
        if l <= 0.0 {
            continue;
        }
        if acc + l >= target {
            let frac = ((target - acc) / l).clamp(0.0, 1.0);
            return (i as f64 + frac) / n;
        }
        acc += l;
    }
    1.0
}

fn quad_at(p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), t: f64) -> (f64, f64) {
    let mt = 1.0 - t;
    (
        mt * mt * p0.0 + 2.0 * mt * t * p1.0 + t * t * p2.0,
        mt * mt * p0.1 + 2.0 * mt * t * p1.1 + t * t * p2.1,
    )
}

fn cubic_at(p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), t: f64) -> (f64, f64) {
    let mt = 1.0 - t;
    (
        mt * mt * mt * p0.0 + 3.0 * mt * mt * t * p1.0 + 3.0 * mt * t * t * p2.0 + t * t * t * p3.0,
        mt * mt * mt * p0.1 + 3.0 * mt * mt * t * p1.1 + 3.0 * mt * t * t * p2.1 + t * t * t * p3.1,
    )
}

/// The point at parameter t on one segment (t=0 start, t=1 end).
fn segment_point(c: char, f: &[f64], s: (f64, f64), t: f64) -> (f64, f64) {
    match c {
        'Q' => quad_at(s, (f[0], f[1]), (f[2], f[3]), t),
        'C' => cubic_at(s, (f[0], f[1]), (f[2], f[3]), (f[4], f[5]), t),
        'A' => arc_geo(f, s).map_or(s, |g| arc_point_tangent(&g, t).0),
        _ => (s.0 + (f[0] - s.0) * t, s.1 + (f[1] - s.1) * t),
    }
}

/// Quadratic restricted to [t1, t2] → (start, control, end); the
/// control comes from B'(t1)·(t2−t1)/2.
fn quad_sub(
    p0: (f64, f64),
    p1: (f64, f64),
    p2: (f64, f64),
    t1: f64,
    t2: f64,
) -> ((f64, f64), (f64, f64), (f64, f64)) {
    let h = t2 - t1;
    let start = quad_at(p0, p1, p2, t1);
    let end = quad_at(p0, p1, p2, t2);
    let d = (
        (1.0 - t1) * (p1.0 - p0.0) + t1 * (p2.0 - p1.0),
        (1.0 - t1) * (p1.1 - p0.1) + t1 * (p2.1 - p1.1),
    );
    (start, (start.0 + d.0 * h, start.1 + d.1 * h), end)
}

/// Cubic restricted to [t1, t2] → (start, c1, c2, end); the handles
/// come from C'(t1)·(t2−t1)/3 and −C'(t2)·(t2−t1)/3.
type CubicSub = ((f64, f64), (f64, f64), (f64, f64), (f64, f64));

/// One head frame: an anchor point and its direction angle (radians).
type HeadFrame = ((f64, f64), f64);

/// Cubic restricted to [t1, t2] → (start, c1, c2, end); the handles
/// come from C'(t1)·(t2−t1)/3 and −C'(t2)·(t2−t1)/3.
fn cubic_sub(
    p0: (f64, f64),
    p1: (f64, f64),
    p2: (f64, f64),
    p3: (f64, f64),
    t1: f64,
    t2: f64,
) -> CubicSub {
    let h = t2 - t1;
    let start = cubic_at(p0, p1, p2, p3, t1);
    let end = cubic_at(p0, p1, p2, p3, t2);
    let deriv = |t: f64| {
        let mt = 1.0 - t;
        (
            mt * mt * (p1.0 - p0.0) + 2.0 * mt * t * (p2.0 - p1.0) + t * t * (p3.0 - p2.0),
            mt * mt * (p1.1 - p0.1) + 2.0 * mt * t * (p2.1 - p1.1) + t * t * (p3.1 - p2.1),
        )
    };
    let d1 = deriv(t1);
    let d2 = deriv(t2);
    (
        start,
        (start.0 + d1.0 * h, start.1 + d1.1 * h),
        (end.0 - d2.0 * h, end.1 - d2.1 * h),
        end,
    )
}

/// Trim one open contour of an emitted path by physical lengths at
/// each end (#185): `begin` off the start, `end` off the end, in user
/// units. Only a single open contour trims — closed loops and
/// multi-subpath data come back untouched (Visio gives them no
/// arrowheads either), and a cut that would consume the whole path
/// keeps the original. Curves split exactly (Blossom
/// re-parameterization for Q/C, the angular subrange for A); the cut
/// parameter comes from a 16-sample chord walk, exact for lines.
fn trim_path(d: &str, begin: f64, end: f64) -> String {
    let keep = |d: &str| d.to_string();
    let Some(cmds) = parse_path_commands(d) else {
        return keep(d);
    };
    let Some(segs) = path_segments(&cmds) else {
        return keep(d);
    };
    if segs.is_empty() || cmds.iter().filter(|(c, _)| *c == 'M').count() != 1 {
        return keep(d);
    }
    let start = segs[0].2;
    let last_end = segs[segs.len() - 1].3;
    if ((last_end.0 - start.0).powi(2) + (last_end.1 - start.1).powi(2)).sqrt() < 1e-9 {
        return keep(d); // closed loop
    }
    let polys: Vec<Vec<(f64, f64)>> = segs
        .iter()
        .map(|(c, f, s, _)| segment_samples(*c, f, *s))
        .collect();
    let lens: Vec<f64> = polys.iter().map(|p| polyline_length(p)).collect();
    let total: f64 = lens.iter().sum();
    let cut_end = total - end;
    if cut_end <= begin + 1e-9 {
        return keep(d); // nothing remains
    }
    let fmt = |p: (f64, f64)| format!("{:.5} {:.5}", p.0, p.1);
    let mut out_d = String::new();
    let mut offset = 0.0;
    for (i, (c, f, s, _)) in segs.iter().enumerate() {
        let len = lens[i];
        let seg_start = offset;
        offset += len;
        let a = begin.max(seg_start);
        let b = cut_end.min(offset);
        if b <= a + 1e-12 {
            continue;
        }
        let poly = &polys[i];
        let t1 = sample_time_at_length(poly, a - seg_start);
        let t2 = sample_time_at_length(poly, b - seg_start);
        if out_d.is_empty() {
            out_d = format!("M {}", fmt(segment_point(*c, f, *s, t1)));
        }
        match c {
            'L' => {
                out_d += &format!(
                    " L {}",
                    fmt((s.0 + (f[0] - s.0) * t2, s.1 + (f[1] - s.1) * t2))
                );
            }
            'Q' => {
                let (_, p1, p2) = quad_sub(*s, (f[0], f[1]), (f[2], f[3]), t1, t2);
                out_d += &format!(" Q {} {}", fmt(p1), fmt(p2));
            }
            'C' => {
                let (_, p1, p2, p3) =
                    cubic_sub(*s, (f[0], f[1]), (f[2], f[3]), (f[4], f[5]), t1, t2);
                out_d += &format!(" C {} {} {}", fmt(p1), fmt(p2), fmt(p3));
            }
            'A' => {
                if let Some(g) = arc_geo(f, *s) {
                    let (p, _) = arc_point_tangent(&g, t2);
                    let large = ((t2 - t1) * g.4.abs() > std::f64::consts::PI) as u8;
                    out_d += &format!(
                        " A {:.5} {:.5} {:.4} {} {} {}",
                        g.0,
                        g.1,
                        f[2],
                        large,
                        f[4] as u8,
                        fmt(p)
                    );
                }
            }
            _ => {}
        }
    }
    if out_d.is_empty() {
        return keep(d);
    }
    out_d
}

/// A segment's start tangent (the true curve tangent where the
/// segment has one).
fn seg_start_tangent(c: char, f: &[f64], s: (f64, f64)) -> Option<(f64, f64)> {
    let d = match c {
        'A' => arc_geo(f, s).map(|g| arc_point_tangent(&g, 0.0).1)?,
        _ => (f[0] - s.0, f[1] - s.1),
    };
    (d.0 * d.0 + d.1 * d.1 > 1e-18).then_some(d)
}

/// A segment's end tangent.
fn seg_end_tangent(c: char, f: &[f64], s: (f64, f64)) -> Option<(f64, f64)> {
    let d = match c {
        'L' => (f[0] - s.0, f[1] - s.1),
        'Q' => (f[2] - f[0], f[3] - f[1]),
        'C' => (f[4] - f[2], f[5] - f[3]),
        'A' => arc_geo(f, s).map(|g| arc_point_tangent(&g, 1.0).1)?,
        _ => return None,
    };
    (d.0 * d.0 + d.1 * d.1 > 1e-18).then_some(d)
}

/// The path's two head frames: the BEGIN tip with its OUTWARD
/// direction (the reversed start tangent) and the END tip with the
/// travel direction. True segment tangents where available, the
/// chord otherwise; needs two distinct points. `M`-only, malformed,
/// or CLOSED single-subpath data returns None (the caller skips the
/// heads but keeps the shaft — #227: a closed geometry draws no line
/// ends).
fn path_frames(d: &str) -> Option<(HeadFrame, HeadFrame)> {
    let cmds = parse_path_commands(d)?;
    let segs = path_segments(&cmds)?;
    let eps = 1e-12;
    let first = segs.first()?;
    let last = segs.last()?;
    // #227: a geometry whose walk closes draws no line ends whatever
    // the arrow cells say — a closed rect with EndArrow=5 gets a clean
    // corner in Visio, not an arrowhead (MisterDiagram #105 witness;
    // the cells stay in the file, only rendering is gated). Closure =
    // the single-subpath walk returning to its start (the same test
    // `trim_path` uses to refuse trimming a loop). Multi-subpath
    // sections take no heads either — the same evidence note
    // `trim_path` carries (Visio gives them no heads; CodeRabbit
    // flagged the last-subpath-closed case, #278).
    if cmds.iter().filter(|(c, _)| *c == 'M').count() != 1 {
        return None;
    }
    let start = first.2;
    let last_end = last.3;
    if ((last_end.0 - start.0).powi(2) + (last_end.1 - start.1).powi(2)).sqrt() < 1e-9 {
        return None;
    }
    let begin = seg_start_tangent(first.0, &first.1, first.2)
        .or_else(|| {
            let (dx, dy) = (first.3 .0 - first.2 .0, first.3 .1 - first.2 .1);
            (dx * dx + dy * dy > eps).then_some((dx, dy))
        })
        .map(|d| (first.2, (-d.1).atan2(-d.0)))?;
    let end = seg_end_tangent(last.0, &last.1, last.2)
        .or_else(|| {
            let (dx, dy) = (last.3 .0 - last.2 .0, last.3 .1 - last.2 .1);
            (dx * dx + dy * dy > eps).then_some((dx, dy))
        })
        .map(|d| (last.3, d.1.atan2(d.0)))?;
    Some((begin, end))
}

/// Emit one arrow head at `tip` facing along `dir` (radians; the +x
/// of the head frame): a translate·rotate·scale group of the
/// unit-space gallery parts. Open parts stroke with the head pen
/// (the line weight, or the 0.75 pt zero-weight nominal) with round
/// caps/joins like the print; filled parts fill only — the print
/// shows no pen inflation on filled glyphs (#185).
fn emit_arrow_head(
    out: &mut String,
    kind: i64,
    sz: f64,
    weight: f64,
    color: &str,
    tip: (f64, f64),
    dir: f64,
) {
    let i = (sz.round() as i64).clamp(0, 6) as usize;
    let unit = weight + ARROW_SIZE_PT[i] / 72.0;
    if !unit.is_finite() || unit <= 0.0 {
        return;
    }
    let Some(parts) = arrow_spec(kind) else {
        return;
    };
    // The group scales unit-space geometry by `unit`; open strokes
    // divide the pen back out so the ink is `weight` wide on paper.
    let pen = floored_stroke(weight) / unit;
    let color = escape_attr(color);
    let mut body = String::new();
    for part in parts {
        if let Some((cx, cy, r)) = part.circle {
            body += &if part.filled {
                format!(r#"<circle cx="{cx}" cy="{cy}" r="{r}" fill="{color}"/>"#)
            } else {
                format!(
                    r#"<circle cx="{cx}" cy="{cy}" r="{r}" fill="none" stroke="{color}" stroke-width="{pen:.5}"/>"#
                )
            };
        } else if part.filled {
            body += &format!(r#"<path d="{}" fill="{color}"/>"#, part.d);
        } else {
            body += &format!(
                r#"<path d="{}" fill="none" stroke="{color}" stroke-width="{pen:.5}" stroke-linecap="round" stroke-linejoin="round"/>"#,
                part.d
            );
        }
    }
    let _ = writeln!(
        out,
        r#"<g class="dc-ah" transform="translate({:.5} {:.5}) rotate({:.4}) scale({:.5})">{}</g>"#,
        tip.0 + 0.0, // normalize -0.0
        tip.1 + 0.0,
        dir.to_degrees() + 0.0,
        unit,
        body
    );
}
/// Tiny FNV-1a for stable marker-id hashing (no external deps).
pub(crate) fn fnv(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Solve an `EllipticalArcTo`: start `s`, on-arc point `m`, end `e` (all in
/// master-space, y-up), major-axis angle `c_ang` (radians) and axis ratio
/// `ratio` (ry/rx). Returns SVG `A` parameters: rx, ry, x-axis-rotation in
/// degrees (already mirrored for the Y-flipped path frame), large-arc and
/// sweep flags — with the endpoint scaled by `scale`.
///
/// The ellipse center satisfies, in the axis-rotated frame, the linear
/// system derived from `k²(x-ox)² + (y-oy)² = k²·rx²` for each of the three
/// points.
fn solve_ellipse_arc(
    s: (f64, f64),
    m: (f64, f64),
    e: (f64, f64),
    c_ang: f64,
    ratio: f64,
    scale: (f64, f64),
) -> Option<(f64, f64, f64, bool, u8)> {
    let k = ratio;
    let (cos_c, sin_c) = (c_ang.cos(), c_ang.sin());
    let rot =
        |p: (f64, f64)| -> (f64, f64) { (cos_c * p.0 + sin_c * p.1, -sin_c * p.0 + cos_c * p.1) };
    let ps = rot(s);
    let pm = rot(m);
    let pe = rot(e);

    // 2·k²(Px-Qx)·ox + 2(Py-Qy)·oy = k²(Px²-Qx²) + (Py²-Qy²)
    let eqn = |p: (f64, f64), q: (f64, f64)| -> ((f64, f64), f64) {
        (
            (2.0 * k * k * (p.0 - q.0), 2.0 * (p.1 - q.1)),
            k * k * (p.0 * p.0 - q.0 * q.0) + (p.1 * p.1 - q.1 * q.1),
        )
    };
    let ((a1, b1), c1) = eqn(ps, pm);
    let ((a2, b2), c2) = eqn(pm, pe);
    let det = a1 * b2 - a2 * b1;
    if det.abs() < 1e-12 {
        return None;
    }
    let ox = (c1 * b2 - c2 * b1) / det;
    let oy = (a1 * c2 - a2 * c1) / det;

    // Semi-axes: k²(Sx-ox)² + (Sy-oy)² = k²·rx²  →  rx = √(…)/k, ry = k·rx.
    let rx = ((k * k * (ps.0 - ox).powi(2) + (ps.1 - oy).powi(2)) / (k * k)).sqrt();
    if !rx.is_finite() || rx < 1e-9 {
        return None;
    }
    let ry = k * rx;

    // Traversal direction and sweep magnitude from parametric angles.
    let ang = |p: (f64, f64)| -> f64 { ((p.1 - oy) / ry).atan2((p.0 - ox) / rx) };
    let norm = |a: f64| -> f64 {
        let mut a = a;
        while a > std::f64::consts::PI {
            a -= 2.0 * std::f64::consts::PI;
        }
        while a < -std::f64::consts::PI {
            a += 2.0 * std::f64::consts::PI;
        }
        a
    };
    let t_s = ang(ps);
    let t_m = ang(pm);
    let t_e = ang(pe);
    let dir = norm(t_m - t_s).signum();
    let mut delta = norm(t_e - t_s);
    if delta * dir < 0.0 {
        delta += dir * 2.0 * std::f64::consts::PI;
    }
    let large = delta.abs() > std::f64::consts::PI;

    // Sweep flag from the traversal s → m → e: the path frame is
    // Y-mirrored and displayed y-down, so a counter-clockwise turn in
    // the y-up frame stays counter-clockwise on the page — SVG's
    // negative-angle direction (sweep 0); a clockwise one is sweep 1.
    // (The old `cross * dir` product was positive for every well-formed
    // arc, so clockwise arcs bulged to the wrong side of their chord —
    // Visio 2021 witness t14.)
    let cross = (m.0 - s.0) * (e.1 - m.1) - (m.1 - s.1) * (e.0 - m.0);
    let sweep = if cross > 0.0 { 0 } else { 1 };

    // Scale the ellipse anisotropically to the instance frame.
    let (rx2, ry2, ang2) = transform_ellipse(rx, k, c_ang, scale);
    Some((
        rx2.max(1e-6),
        ry2.max(1e-6),
        -ang2.to_degrees(),
        large,
        sweep,
    ))
}

/// Transform an ellipse (semi-major `r_major`, ratio `k = minor/major`, axis
/// angle `ang`) by an anisotropic scale `scale`. Returns the new semi-axes
/// (major, minor) and the new major-axis angle.
///
/// The image ellipse's squared semi-axes are the eigenvalues of
/// `D·R·diag(r², (k·r)²)·Rᵀ·D` with `D = diag(scale)`.
fn transform_ellipse(r_major: f64, k: f64, ang: f64, scale: (f64, f64)) -> (f64, f64, f64) {
    let (c, si) = (ang.cos(), ang.sin());
    let a = r_major * r_major;
    let b = (k * r_major) * (k * r_major);
    // Q = R·diag(a,b)·Rᵀ
    let q11 = a * c * c + b * si * si;
    let q12 = (a - b) * c * si;
    let q22 = a * si * si + b * c * c;
    // B = D·Q·D
    let (sx, sy) = scale;
    let m11 = q11 * sx * sx;
    let m12 = q12 * sx * sy;
    let m22 = q22 * sy * sy;
    // Eigenvalues of symmetric [[m11, m12], [m12, m22]].
    let tr = m11 + m22;
    let det = m11 * m22 - m12 * m12;
    let disc = ((tr * tr / 4.0) - det).max(0.0).sqrt();
    let lam1 = tr / 2.0 + disc; // major²
    let lam2 = (tr / 2.0 - disc).max(1e-12); // minor²
                                             // Eigenvector for lam1: B·(λ-m22, m12) = λ·(λ-m22, m12) for symmetric B.
    let (vx, vy) = if m12.abs() > 1e-12 {
        (lam1 - m22, m12)
    } else if m11 >= m22 {
        (1.0, 0.0)
    } else {
        (0.0, 1.0)
    };
    let mut new_ang = vy.atan2(vx);
    if new_ang > std::f64::consts::FRAC_PI_2 {
        new_ang -= std::f64::consts::PI;
    } else if new_ang < -std::f64::consts::FRAC_PI_2 {
        new_ang += std::f64::consts::PI;
    }
    (lam1.sqrt(), lam2.sqrt(), new_ang)
}

/// Rel* rows express coordinates as fractions of Width/Height.
fn rel_adjust(t: &str, x: f64, y: f64, w: f64, h: f64) -> (f64, f64) {
    if t.starts_with("Rel") {
        (x * w, y * h)
    } else {
        (x, y)
    }
}

thread_local! {
    /// The narrowest stroke (inches) the current canvas layer render
    /// paints — see [`stroke_floor_in`]. Zero outside a canvas render.
    static STROKE_FLOOR_IN: std::cell::Cell<f64> = const { std::cell::Cell::new(0.0) };
}

/// The narrowest stroke the canvas paints at `ppi` pixels per inch:
/// one device pixel at the low end of `ppi`'s half-octave. Visio draws
/// every line at least one pixel wide on screen, so hairlines stay
/// solid when zoomed out; tiny-skia fades a sub-pixel stroke to a gray
/// ghost instead. Quantized to half-octaves so a layer re-serializes
/// only when the zoom moves far enough to change a floored stroke
/// (floored lines are 1 to 1.41 px).
pub fn stroke_floor_in(ppi: f32) -> f64 {
    if !ppi.is_finite() || ppi <= 0.0 {
        return 0.0;
    }
    let class = (2.0 * (ppi as f64).log2()).floor();
    1.0 / 2f64.powf(class / 2.0)
}

/// The narrowest positive `stroke-width` in a rendered layer, for the
/// canvas's tree-reuse check. Infinite when the layer strokes nothing.
pub fn min_stroke_width(svg: &str) -> f64 {
    svg.split("stroke-width=\"")
        .skip(1)
        .filter_map(|t| t.split('"').next()?.parse::<f64>().ok())
        .filter(|w| *w > 0.0)
        .fold(f64::INFINITY, f64::min)
}

/// `w` raised to the current canvas floor; an explicit zero stays zero
/// (#235: Visio paints nothing at LineWeight 0).
fn floored_stroke(w: f64) -> f64 {
    if w > 0.0 {
        w.max(STROKE_FLOOR_IN.with(|f| f.get()))
    } else {
        w
    }
}

/// Sets the canvas stroke floor for the rest of a layer render and
/// clears it on drop.
struct StrokeFloorGuard;

impl StrokeFloorGuard {
    fn set(ppi: Option<f32>, bounds: (f64, f64, f64, f64)) -> StrokeFloorGuard {
        let floor = ppi.map_or(0.0, |p| layer_stroke_floor_in(p, bounds));
        STROKE_FLOOR_IN.with(|f| f.set(floor));
        StrokeFloorGuard
    }
}

/// The stroke floor a canvas layer with `bounds` (inches) renders with
/// at `ppi`: [`stroke_floor_in`] at the ppi the layer's texture really
/// gets — the canvas caps a texture at 2048 px a side, so a large layer
/// rasterizes coarser than the zoom asks for.
pub fn layer_stroke_floor_in(ppi: f32, bounds: (f64, f64, f64, f64)) -> f64 {
    const MAX_TEX: f64 = 2048.0;
    let cap = (MAX_TEX / bounds.2.max(1e-9)).min(MAX_TEX / bounds.3.max(1e-9));
    stroke_floor_in((ppi as f64).min(cap) as f32)
}

impl Drop for StrokeFloorGuard {
    fn drop(&mut self) {
        STROKE_FLOOR_IN.with(|f| f.set(0.0));
    }
}


/// The stroke half of a style attribute (#228 split): dashed strokes
/// carry `stroke-linecap="round"` — the cap the dash table's
/// ink→geometry conversion assumes (#235).
fn stroke_attr(
    no_line: bool,
    line_color: &str,
    line_weight: f64,
    dash: Option<&str>,
    alpha: f64,
) -> String {
    if no_line {
        r#"stroke="none""#.to_string()
    } else {
        let dash = dash
            .map(|d| format!(r#" stroke-dasharray="{d}" stroke-linecap="round""#))
            .unwrap_or_default();
        // #235: an explicit-zero LineWeight stays zero — SVG paints
        // nothing at stroke-width 0, exactly the Visio behavior (a
        // fallback pen drew stray hairlines; MisterDiagram witness).
        // The old `.max(0.001)` fallback pen is gone.
        format!(
            r#"stroke="{}" stroke-width="{:.5}"{dash}{}"#,
            escape_attr(line_color),
            floored_stroke(line_weight),
            opacity_attr("stroke-opacity", alpha)
        )
    }
}

/// A Visio 2013+ gradient as SVG paint: a `url(#…)` linear/radial
/// gradient, or — for the rectangular directions SVG has no gradient
/// for — tiles (four triangles, each a linear gradient from the
/// origin to one side) the caller clips to the fill path.
#[derive(Clone)]
enum GradientPaint {
    Url(String),
    Tiles(String),
}

/// A legacy fill pattern (FillPattern 2–23): Visio draws them as 8×8
/// device-pixel bitmaps at 96 dpi — period 1/12 in, 1/96 in lines in
/// FillForegnd over FillBkgnd (Visio 2021 witness t12). The line
/// patterns become clipped strokes; the dot/dither patterns a mixed
/// color (8 ≈ ¾ foreground, the rest ≈ ½ — their bitmaps are not
/// modeled).
enum Hatch {
    Lines(String),
    Mixed(String),
}

fn hatch_fill(pattern: f64, fg: &str, bg: &str, (w, h): (f64, f64)) -> Option<Hatch> {
    let pat = pattern.round() as i64;
    if !(2..=23).contains(&pat) {
        return None;
    }
    if pat >= 8 {
        let (a, b) = (hex_rgb(fg), hex_rgb(bg));
        let k = if pat == 8 { 0.75 } else { 0.5 };
        let mix = |x: u8, y: u8| (f64::from(x) * k + f64::from(y) * (1.0 - k)).round() as u8;
        return Some(Hatch::Mixed(format!(
            "#{:02x}{:02x}{:02x}",
            mix(a.0, b.0),
            mix(a.1, b.1),
            mix(a.2, b.2)
        )));
    }
    let period = 1.0 / 12.0;
    let pen = 1.0 / 96.0;
    // The local box spans x 0…|w|, y −|h|…0 (y down); lines cover it
    // generously and the fill path clips them.
    let (w, h) = (w.abs(), h.abs());
    let span = w + h + period;
    let mut d = String::new();
    let mut lines = |dir: u8| {
        let n = (span / period).ceil() as i64 + 1;
        for k in -n..=n {
            let c = k as f64 * period;
            let (a, b) = match dir {
                // horizontal / vertical
                0 => ((-period, -c), (w + period, -c)),
                1 => ((c, -h - period), (c, period)),
                // "/" (x + y = c) and "\" (x − y = c), y down
                2 => ((c + h + period, -h - period), (c - period, period)),
                _ => ((c - h - period, -h - period), (c + period, period)),
            };
            let _ = write!(d, "M {:.4} {:.4} L {:.4} {:.4} ", a.0, a.1, b.0, b.1);
        }
    };
    match pat {
        2 => lines(2),
        3 => {
            lines(0);
            lines(1);
        }
        4 => {
            lines(2);
            lines(3);
        }
        5 => lines(3),
        6 => lines(0),
        _ => lines(1),
    }
    Some(Hatch::Lines(format!(
        r#"<rect x="0" y="{:.5}" width="{w:.5}" height="{h:.5}" fill="{}"/><path d="{}" fill="none" stroke="{}" stroke-width="{pen:.5}"/>"#,
        -h,
        escape_attr(bg),
        d.trim_end(),
        escape_attr(fg)
    )))
}

/// One gradient stop: color, alpha, position 0–1.
type Stop = (String, f64, f64);

/// The `<prefix>Gradient` stop rows (FillGradient / LineGradient)
/// through the instance → master → style chain; per-row cells the
/// instance row omits come from the master's row of the same IX.
fn gradient_stops(doc: &Document, s: &Shape, chain: &[&Shape], prefix: &str) -> Vec<Stop> {
    let name = format!("{prefix}Gradient");
    fn first<'a>(sheet: &'a Sheet, name: &str) -> Option<&'a vsdx::Section> {
        sheet.sections_named(name).into_iter().next()
    }
    let src = doc
        .instance_source(s, chain)
        .and_then(|m| first(&m.sheet, &name));
    // The style chain (the shape's — or its master's — Fill/LineStyle,
    // then each style's parent attribute): the first style whose stop
    // rows carry real values. The Theme style's `V='Themed'` rows are
    // no values without an evaluated theme.
    let parent_attr = format!("{prefix}Style");
    let mut style = None;
    let mut id = doc.shape_style_for(s, chain, &format!("{prefix}Gradient"));
    for _ in 0..8 {
        let Some(st) = doc.stylesheet(id) else { break };
        if let Some(sec) = first(&st.sheet, &name) {
            let real = sec.rows.iter().any(|r| {
                r.cell("GradientStopColor")
                    .is_some_and(|c| !c.is_themed() && !c.is_inherited())
            });
            if real {
                style = Some(sec);
                break;
            }
        }
        match st
            .attrs
            .iter()
            .find(|(n, _)| *n == parent_attr)
            .and_then(|(_, v)| v.parse::<i64>().ok())
        {
            Some(next) if next != id => id = next,
            _ => break,
        }
    }
    // Stop rows merge per IX across the layers — instance, master,
    // style — and per CELL within a row: a master row that overrides
    // only `GradientStopColor` keeps the style row's position (Visio
    // 2021 witness: the Network-Locations cloud overrides stop 0's
    // color and inherits stop 1 entirely from "3D Net Face"). A row
    // deleted (`Del='1'`) in the topmost layer that mentions it is gone.
    let layers: Vec<&vsdx::Section> = [first(&s.sheet, &name), src, style]
        .into_iter()
        .flatten()
        .collect();
    let mut ixs: Vec<i64> = layers
        .iter()
        .flat_map(|sec| sec.rows.iter().map(|r| r.ix))
        .collect();
    ixs.sort_unstable();
    ixs.dedup();
    let mut stops: Vec<Stop> = ixs
        .into_iter()
        .filter_map(|ix| {
            let rows: Vec<&vsdx::Row> = layers
                .iter()
                .filter_map(|sec| sec.rows.iter().find(|r| r.ix == ix))
                .collect();
            if rows.first().is_none_or(|r| r.is_deleted()) {
                return None;
            }
            let cell = |n: &str| {
                rows.iter()
                    .take_while(|r| !r.is_deleted())
                    .filter_map(|r| r.cell(n))
                    .find(|c| !c.is_inherited() && !c.is_themed())
            };
            let col = cell("GradientStopColor")?;
            let color = color_value(doc, col.v.as_deref().unwrap_or(""), col.f.as_deref());
            let trans = cell("GradientStopColorTrans")
                .and_then(|c| c.numeric())
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            let pos = cell("GradientStopPosition")
                .and_then(|c| c.numeric())
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
            Some((color, 1.0 - trans, pos))
        })
        .collect();
    stops.sort_by(|a, b| a.2.total_cmp(&b.2));
    stops
}

/// `<stop>` elements for `stops`.
fn stop_elems(stops: &[Stop]) -> String {
    let mut out = String::new();
    for (c, a, p) in stops {
        let _ = write!(
            out,
            r#"<stop offset="{p:.4}" stop-color="{}"{}/>"#,
            escape_attr(c),
            opacity_attr("stop-opacity", *a)
        );
    }
    out
}

/// Resolve a `Fill`/`Line` gradient for one shape (Visio 2021
/// witnesses t08–t10). `None` unless `<prefix>GradientEnabled` is on.
///
/// - Stops come from the `<prefix>Gradient` rows; a shape with the
///   flag but no rows takes `fallback` (FillForegnd → FillBkgnd).
/// - Dir 0 is linear along `<prefix>GradientAngle` (default 90°):
///   0° runs left → right, 90° top → bottom, 45° from the top-left
///   to the bottom-right corner — the axis spans the box's projection
///   onto the direction, so stop 0 and 1 land exactly on the corners.
/// - Dir 1–7 are radial from a box anchor (1 bottom-right, 2
///   bottom-left, 3 center, 4 top-center, 5 bottom-center, 6
///   top-right, 7 top-left), reaching the farthest corner.
/// - Dir 8–12 are rectangular from an anchor (8 bottom-right, 9
///   bottom-left, 10 center, 11 top-right, 12 top-left); 13 (path)
///   renders as the centered rectangle.
///
/// Coordinates are the shape's local SVG frame (x 0…W, y −H…0), so
/// every gradient is `userSpaceOnUse` and the definitions dedupe by
/// content hash into `out`. The box is the frame's ABSOLUTE extent: a
/// connector drawn right-to-left has a negative Width (`EndX−BeginX`),
/// and Visio still runs a 0° gradient left → right across it.
#[allow(clippy::too_many_arguments)]
fn gradient_paint(
    doc: &Document,
    s: &Shape,
    chain: &[&Shape],
    env: &vsdx::formula::Env,
    prefix: &str,
    (w, h): (f64, f64),
    fallback: (&str, &str),
    out: &mut String,
) -> Option<GradientPaint> {
    let num = |n: &str| {
        doc.resolve_cell(s, chain, &format!("{prefix}{n}"))
            .and_then(|c| cell_num(&c, env))
            .filter(|v| v.is_finite())
    };
    if num("GradientEnabled").is_none_or(|v| v == 0.0) {
        return None;
    }
    let mut stops = gradient_stops(doc, s, chain, prefix);
    if stops.is_empty() {
        stops = vec![
            (fallback.0.to_string(), 1.0, 0.0),
            (fallback.1.to_string(), 1.0, 1.0),
        ];
    }
    let stops_svg = stop_elems(&stops);
    // The absolute box in the local SVG frame (y down).
    let (bl, br) = (w.min(0.0), w.max(0.0));
    let (bt, bb) = ((-h).min(0.0), (-h).max(0.0));
    let (w, h) = (br - bl, bb - bt);
    let dir = num("GradientDir").unwrap_or(0.0).round() as i64;
    let angle = num("GradientAngle").unwrap_or(std::f64::consts::FRAC_PI_2);
    let def = |out: &mut String, body: String| -> String {
        let id = format!("dc-grad{:x}", fnv(&body));
        if !out.contains(&format!(r#"id="{id}""#)) {
            let _ = writeln!(out, "<defs>{}</defs>", body.replace("{ID}", &id));
        }
        id
    };
    let linear = |out: &mut String, (x1, y1): (f64, f64), (x2, y2): (f64, f64)| {
        def(
            out,
            format!(
                r#"<linearGradient id="{{ID}}" gradientUnits="userSpaceOnUse" x1="{x1:.5}" y1="{y1:.5}" x2="{x2:.5}" y2="{y2:.5}">{stops_svg}</linearGradient>"#
            ),
        )
    };
    // Box anchors in the local SVG frame (y down, −H…0).
    let (mx, my) = ((bl + br) / 2.0, (bt + bb) / 2.0);
    let anchor = |k: i64| -> (f64, f64) {
        match k {
            1 | 8 => (br, bb),
            2 | 9 => (bl, bb),
            4 => (mx, bt),
            5 => (mx, bb),
            6 | 11 => (br, bt),
            7 | 12 => (bl, bt),
            _ => (mx, my),
        }
    };
    let rect = (8..=13).contains(&dir) && prefix == "Fill";
    if (1..=7).contains(&dir) {
        let (cx, cy) = anchor(dir);
        let r = [(bl, bt), (br, bt), (bl, bb), (br, bb)]
            .iter()
            .map(|(x, y)| (x - cx).hypot(y - cy))
            .fold(0.0, f64::max)
            .max(1e-6);
        let id = def(
            out,
            format!(
                r#"<radialGradient id="{{ID}}" gradientUnits="userSpaceOnUse" cx="{cx:.5}" cy="{cy:.5}" r="{r:.5}">{stops_svg}</radialGradient>"#
            ),
        );
        return Some(GradientPaint::Url(format!("url(#{id})")));
    }
    if rect {
        // Chebyshev distance from the anchor: four triangles (one per
        // side), each a linear ramp from the anchor to that side.
        let (ox, oy) = anchor(dir);
        let rx = (ox - bl).max(br - ox).max(1e-6);
        let ry = (oy - bt).max(bb - oy).max(1e-6);
        let mut tiles = String::new();
        for (sx, sy, horiz) in [
            (1.0, 0.0, true),
            (-1.0, 0.0, true),
            (0.0, 1.0, false),
            (0.0, -1.0, false),
        ] {
            let (c1, c2) = if horiz {
                ((ox + sx * rx, oy - ry), (ox + sx * rx, oy + ry))
            } else {
                ((ox - rx, oy + sy * ry), (ox + rx, oy + sy * ry))
            };
            let end = (ox + sx * rx, oy + sy * ry);
            let id = linear(out, (ox, oy), end);
            let _ = write!(
                tiles,
                r#"<path d="M {ox:.5} {oy:.5} L {:.5} {:.5} L {:.5} {:.5} Z" fill="url(#{id})"/>"#,
                c1.0, c1.1, c2.0, c2.1
            );
        }
        return Some(GradientPaint::Tiles(tiles));
    }
    let (dx, dy) = (angle.cos(), angle.sin());
    let half = (dx.abs() * w + dy.abs() * h) / 2.0;
    let (cx, cy) = (mx, my);
    let id = linear(
        out,
        (cx - dx * half, cy - dy * half),
        (cx + dx * half, cy + dy * half),
    );
    Some(GradientPaint::Url(format!("url(#{id})")))
}

/// Round the corners of a straight-line path (`M`/`L`/`Z` only, the
/// renderer's own number format) with radius `r`, the way Visio's
/// Rounding cell does: each corner is replaced by a circular arc
/// tangent to both legs, the tangent length clamped to half of the
/// shorter leg so neighboring arcs never overlap. A walk that returns
/// to its start rounds that corner too. `None` (path unchanged) for
/// any path with curves — those keep their authored shape — or with
/// more than one subpath.
fn round_corners(d: &str, r: f64) -> Option<String> {
    let mut pts: Vec<(f64, f64)> = Vec::new();
    let mut toks = d.split_whitespace();
    let mut moves = 0;
    while let Some(t) = toks.next() {
        match t {
            "M" | "L" => {
                if t == "M" {
                    moves += 1;
                }
                let x: f64 = toks.next()?.parse().ok()?;
                let y: f64 = toks.next()?.parse().ok()?;
                if pts
                    .last()
                    .is_none_or(|p: &(f64, f64)| (p.0 - x).hypot(p.1 - y) > 1e-9)
                {
                    pts.push((x, y));
                }
            }
            "Z" => {}
            _ => return None,
        }
    }
    if moves != 1 || pts.len() < 3 {
        return None;
    }
    let closed = {
        let (a, b) = (pts[0], pts[pts.len() - 1]);
        (a.0 - b.0).hypot(a.1 - b.1) < 1e-6
    };
    if closed {
        pts.pop();
        if pts.len() < 3 {
            return None;
        }
    }
    let n = pts.len();
    // The corner at pts[i] between its neighbors: (T1, T2, radius,
    // sweep) or None for a straight/degenerate corner.
    // (tangent point on the incoming leg, on the outgoing leg, arc
    // radius, SVG sweep flag)
    type Corner = ((f64, f64), (f64, f64), f64, u8);
    let corner = |i: usize| -> Option<Corner> {
        let p = pts[i];
        let a = pts[(i + n - 1) % n];
        let b = pts[(i + 1) % n];
        let (la, lb) = ((a.0 - p.0).hypot(a.1 - p.1), (b.0 - p.0).hypot(b.1 - p.1));
        if la < 1e-9 || lb < 1e-9 {
            return None;
        }
        let u = ((a.0 - p.0) / la, (a.1 - p.1) / la);
        let v = ((b.0 - p.0) / lb, (b.1 - p.1) / lb);
        let theta = (u.0 * v.0 + u.1 * v.1).clamp(-1.0, 1.0).acos();
        if theta < 1e-6 || (std::f64::consts::PI - theta) < 1e-6 {
            return None;
        }
        let half_tan = (theta / 2.0).tan();
        let t = (r / half_tan).min(la / 2.0).min(lb / 2.0);
        let rr = t * half_tan;
        let t1 = (p.0 + u.0 * t, p.1 + u.1 * t);
        let t2 = (p.0 + v.0 * t, p.1 + v.1 * t);
        // Incoming direction (a→p) × outgoing (p→b): the turn's side
        // in the y-down frame picks the arc's sweep.
        let cross = (p.0 - a.0) * (b.1 - p.1) - (p.1 - a.1) * (b.0 - p.0);
        Some((t1, t2, rr, u8::from(cross > 0.0)))
    };
    let mut out = String::new();
    let arc_to = |out: &mut String, c: Corner| {
        let ((x1, y1), (x2, y2), rr, sw) = c;
        let _ = write!(
            out,
            "L {x1:.5} {y1:.5} A {rr:.5} {rr:.5} 0 0 {sw} {x2:.5} {y2:.5} "
        );
    };
    if closed {
        let start = corner(0).map_or(pts[0], |c| c.1);
        let _ = write!(out, "M {:.5} {:.5} ", start.0, start.1);
        for i in (1..n).chain(std::iter::once(0)) {
            match corner(i) {
                Some(c) => arc_to(&mut out, c),
                None => {
                    let _ = write!(out, "L {:.5} {:.5} ", pts[i].0, pts[i].1);
                }
            }
        }
        out.push_str("Z ");
    } else {
        let _ = write!(out, "M {:.5} {:.5} ", pts[0].0, pts[0].1);
        for (i, p) in pts.iter().enumerate().take(n - 1).skip(1) {
            match corner(i) {
                Some(c) => arc_to(&mut out, c),
                None => {
                    let _ = write!(out, "L {:.5} {:.5} ", p.0, p.1);
                }
            }
        }
        let _ = write!(out, "L {:.5} {:.5} ", pts[n - 1].0, pts[n - 1].1);
    }
    Some(out)
}

/// Does a ShapeSheet formula reference the shape's own `Width` or
/// `Height` cell? A whole-identifier match: `User.BarWidth`,
/// `Sheet.5!Width` or `TxtHeight` are other cells, and treating them as
/// the shape's size scaled a stretched U-Turn arrow's 0.2 in bar to
/// 1.9 in (the network LLD witness).
fn refs_own_size(f: &str) -> bool {
    let b = f.as_bytes();
    ["Width", "Height"].iter().any(|w| {
        f.match_indices(w).any(|(i, _)| {
            let before = i.checked_sub(1).map(|j| b[j]);
            let after = b.get(i + w.len()).copied();
            let ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'.' || c == b'!';
            !before.is_some_and(ident)
                && !after.is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
    })
}

/// The stroke's end caps and corner joins for a LineCap value. Dashed
/// strokes keep the round caps their dash table assumes (#235), but
/// the join still follows LineCap: flat and extended caps miter their
/// corners also when dashed (CodeRabbit on PR #342).
fn cap_attr(cap: i64, dashed: bool) -> String {
    let join = if matches!(cap, 1 | 2) {
        "miter"
    } else {
        "round"
    };
    let cap = match (cap, dashed) {
        (_, true) => String::new(),
        (1, false) => r#" stroke-linecap="butt""#.into(),
        (2, false) => r#" stroke-linecap="square""#.into(),
        _ => r#" stroke-linecap="round""#.into(),
    };
    format!(r#"{cap} stroke-linejoin="{join}""#)
}

/// Close a single-subpath stroke that returns to its start (`Z`), so
/// the start corner joins like every other corner instead of showing
/// two butt ends (Visio 2021 witness, b04's rectangle).
fn close_returning_path(d: &str) -> String {
    let t = d.trim_end();
    if t.ends_with('Z') || t.matches('M').count() != 1 {
        return d.to_string();
    }
    let nums: Vec<f64> = t
        .split(|c: char| c.is_whitespace() || c.is_ascii_alphabetic())
        .filter_map(|x| x.parse().ok())
        .collect();
    let first = t
        .trim_start()
        .strip_prefix('M')
        .map(|r| {
            r.split_whitespace()
                .take(2)
                .filter_map(|x| x.parse::<f64>().ok())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    match (first.as_slice(), nums.len()) {
        ([x0, y0], n) if n >= 6 => {
            let (x1, y1) = (nums[n - 2], nums[n - 1]);
            if (x1 - x0).hypot(y1 - y0) < 1e-6 {
                format!("{t} Z")
            } else {
                d.to_string()
            }
        }
        _ => d.to_string(),
    }
}

/// The closed subpaths of a path in the renderer's own format
/// (space-separated tokens, each subpath opened by `M`): a subpath is
/// closed when it ends in `Z` or its last point returns to its `M`
/// point. Any command closes, so a single curve that returns to its
/// start counts. Open subpaths are dropped.
fn closed_subpaths(d: &str) -> String {
    let mut out = String::new();
    let mut flush = |sub: &[&str]| {
        if sub.len() < 4 {
            return; // `M x y` alone draws nothing
        }
        let num = |t: &str| t.parse::<f64>().ok();
        let closed = sub.last() == Some(&"Z")
            || match (
                num(sub[1]),
                num(sub[2]),
                num(sub[sub.len() - 2]),
                num(sub[sub.len() - 1]),
            ) {
                (Some(x0), Some(y0), Some(x1), Some(y1)) => {
                    (x0 - x1).abs() < 1e-4 && (y0 - y1).abs() < 1e-4
                }
                _ => false,
            };
        if closed {
            out.push_str(&sub.join(" "));
            out.push(' ');
        }
    };
    // Command letters need not be space-separated: curve and arc rows
    // trim their trailing space, so a following `M` lands glued to the
    // last number (`… 0.00000M 1 2`). Split every command letter off
    // as its own token (Greptile on PR #328); our numbers carry no
    // exponent, so no letter is part of a number.
    let toks: Vec<&str> = d
        .split_whitespace()
        .flat_map(|w| {
            let mut parts = Vec::new();
            let mut from = 0;
            for (i, c) in w.char_indices() {
                if c.is_ascii_alphabetic() {
                    if i > from {
                        parts.push(&w[from..i]);
                    }
                    parts.push(&w[i..i + 1]);
                    from = i + 1;
                }
            }
            if from < w.len() {
                parts.push(&w[from..]);
            }
            parts
        })
        .collect();
    let mut start = 0;
    for (i, t) in toks.iter().enumerate() {
        if *t == "M" && i > start {
            flush(&toks[start..i]);
            start = i;
        }
    }
    flush(&toks[start..]);
    out
}

/// The geometry frame's own size on one axis: `dim / scale` recovers
/// the master's dimension from the instance's (`scale` = instance /
/// master, so a negative instance size keeps its sign). A zero scale
/// has no master dimension to recover and keeps `dim` (Greptile on
/// PR #329: the old `scale.max(1e-12)` threw negative scales to 1e12).
fn frame_dim(dim: f64, scale: f64) -> f64 {
    if scale.abs() > 1e-12 {
        dim / scale
    } else {
        dim
    }
}

/// ` name="a"` for a partial alpha, nothing when opaque.
fn opacity_attr(name: &str, alpha: f64) -> String {
    if alpha < 1.0 {
        format!(r#" {name}="{:.3}""#, alpha.max(0.0))
    } else {
        String::new()
    }
}

/// One shape's accumulated paint, #228: every FILL-ELIGIBLE Geometry
/// section's path concatenates into ONE fill path the caller emits
/// with `fill-rule="evenodd"` — Visio's alternate rule, where nesting
/// and overlap toggle coverage regardless of winding direction
/// (MisterDiagram M5.109: inner nested region clear for both
/// directions, overlap clear, third nesting filled). Per-section
/// flags gate participation: NoFill opts a section out of the fill
/// only, NoShow already skipped it entirely, NoLine leaves the fill
/// intact; open contours close implicitly for the fill (SVG fills
/// open subpaths as closed) without gaining a stroked closing edge —
/// the strokes stay per-section, in section order, OVER the fill.
struct ShapePaint {
    /// The concatenated fill subpaths (each section's `d`, which all
    /// start with `M`).
    fill_d: String,
    /// The fill style (shape-level cells — identical for every
    /// participating section; the last one computed wins).
    fill: String,
    /// The fill's alpha (1 − FillForegndTrans / FillBkgndTrans).
    fill_opacity: f64,
    /// A rectangular gradient's tiles (`gradient_paint`): painted
    /// clipped to the fill path instead of the plain fill.
    fill_tiles: Option<String>,
    /// Stroke-only paths, arrow shafts/heads, and standalone ellipse
    /// rows, in emission order.
    strokes: String,
}

impl Default for ShapePaint {
    fn default() -> Self {
        Self {
            fill_d: String::new(),
            fill: "none".to_string(),
            fill_opacity: 1.0,
            fill_tiles: None,
            strokes: String::new(),
        }
    }
}

/// Emit the accumulated paint under the caller's ink (#228): the ONE
/// even-odd fill path first, then the per-section strokes.
fn flush_shape_paint(paint: &mut ShapePaint, out: &mut String) {
    if let (false, Some(tiles)) = (paint.fill_d.is_empty(), &paint.fill_tiles) {
        let d = paint.fill_d.trim();
        let id = format!("dc-gclip{:x}", fnv(&format!("{d}|{tiles}")));
        let _ = writeln!(
            out,
            r#"<clipPath id="{id}"><path clip-rule="evenodd" d="{}"/></clipPath><g clip-path="url(#{id})"{}{}>{tiles}</g>"#,
            escape_attr(d),
            opacity_attr("fill-opacity", paint.fill_opacity),
            opacity_attr("stroke-opacity", paint.fill_opacity)
        );
    } else if !paint.fill_d.is_empty() {
        let _ = writeln!(
            out,
            r#"<path fill-rule="evenodd" d="{}" fill="{}"{}/>"#,
            escape_attr(paint.fill_d.trim()),
            escape_attr(&paint.fill),
            opacity_attr("fill-opacity", paint.fill_opacity)
        );
    }
    out.push_str(&paint.strokes);
}

/// Visio's built-in dash/dot line patterns (LinePattern 2–23) as SVG
/// `stroke-dasharray` values in LineWeight multiples (patterns scale with
/// the line weight — the ShapeSheet gallery's defining property). The
/// table is MEASURED INK — run/gap lengths as painted at 600 dpi — and
/// the caller strokes it with ROUND caps, so the values convert to
/// Visio's canonical GEOMETRY before emission (#235 cap-aware split):
/// a round cap extends each mark by one weight (half at each end) and
/// eats one weight from the gap, i.e. geometry on = ink − 1w, gap =
/// ink + 1w — exactly MisterDiagram's canonical grammar (pattern 2 =
/// 7/5 where our ink reads 8/4; 14 = 9/3/3; 21 = 39/9/15/9). Dots
/// (ink ≤ 1w marks) are weight-diameter circles: geometry length 0,
/// plus a 0.0001w lead so rasterizers keep a tangent for the
/// zero-length subpath (their dot fix; the following gap gives the
/// lead back, period preserved).
fn line_dasharray(pattern: f64, weight: f64) -> Option<String> {
    // #171: the table is measured from Visio 2016 itself — the
    // owner-authored fixture `Sample/test files/lines.vsdx` (patterns
    // 0–23, 0.02 in weight) printed at 600 dpi, run/gap sequences in
    // stroke-width units (Analysis/line-patterns-visio2016.md). 0/1
    // stay None (0 hides the line entirely at the call site).
    let spec: &[f64] = match pattern as i64 {
        2 => &[8.0, 4.0],                         // dash
        3 => &[1.0, 4.0],                         // dot
        4 => &[8.0, 4.0, 1.0, 4.0],               // dash-dot
        5 => &[8.0, 4.0, 1.0, 4.0, 1.0, 4.0],     // dash-dot-dot
        6 => &[8.0, 4.0, 8.0, 4.0, 1.0, 4.0],     // dash-dash-dot
        7 => &[20.0, 4.0, 8.0, 4.0],              // long + short dash
        8 => &[20.0, 4.0, 8.0, 4.0, 8.0, 4.0],    // long + 2 short dashes
        9 => &[4.0, 2.0], // short dash (the corpus legend "long dash" label was wrong)
        10 => &[1.0, 2.0], // fine dot
        11 => &[4.0, 2.0, 1.0, 2.0], // short dash-dot
        12 => &[4.0, 2.0, 1.0, 2.0, 1.0, 2.0], // short dash-dot-dot
        13 => &[4.0, 2.0, 4.0, 2.0, 1.0, 2.0], // short dash-dash-dot
        14 => &[10.0, 2.0, 4.0, 2.0], // medium-short dash pair
        15 => &[10.0, 2.0, 4.0, 2.0, 4.0, 2.0], // medium + 2 short dashes
        16 => &[16.0, 8.0], // long dash
        17 => &[1.0, 8.0], // sparse dot
        18 => &[16.0, 8.0, 1.0, 8.0], // long dash-dot
        19 => &[16.0, 8.0, 1.0, 8.0, 1.0, 8.0], // long dash-dot-dot
        20 => &[16.0, 8.0, 16.0, 8.0, 1.0, 8.0], // long dash-dash-dot
        21 => &[40.0, 8.0, 16.0, 8.0], // extra-long + long dash
        22 => &[40.0, 8.0, 16.0, 8.0, 16.0, 8.0], // extra-long + 2 long
        23 => &[2.0, 1.0], // fine dash
        _ => return None,
    };
    // Ink → geometry under round caps (#235): marks give up one
    // weight to their caps, gaps take it back; a dot's mark becomes
    // the 0.0001w tangent lead and its gap returns the difference so
    // the period is unchanged.
    let dot_lead = 0.0001;
    let geom: Vec<f64> = spec
        .iter()
        .enumerate()
        .map(|(i, ink)| {
            if i % 2 == 0 {
                // An on-mark: a dot (ink ≤ 1w) draws as the lead
                // alone; a dash shortens by its caps.
                if *ink <= 1.0 {
                    dot_lead
                } else {
                    ink - 1.0
                }
            } else {
                // A gap: regains the mark's cap width, minus the
                // lead when the preceding mark was a dot.
                let prev_was_dot = spec[i - 1] <= 1.0;
                ink + 1.0 - if prev_was_dot { dot_lead } else { 0.0 }
            }
        })
        .collect();
    Some(
        geom.iter()
            .map(|m| {
                let v = m * weight;
                // The dot lead (0.0001w) sits far below the 5-decimal
                // rounding floor — print sub-floor values at 10
                // decimals so the lead and its gap compensation
                // survive (CodeRabbit, #278: the old `.max(1e-4)`
                // clamp inflated the lead 50× at a 0.02 weight and
                // stretched the period). Every value is positive by
                // construction, so no zero-length dash entries occur.
                if v < 1e-4 {
                    format!("{v:.10}")
                } else {
                    format!("{v:.5}")
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Resolve a color-ish cell to `#rrggbb`.
/// Resolve a color cell for rendering: literal hex / document palette,
/// or — when the cell's formula is a THEMEVAL("VariantColorN") — the
/// theme's variant color (Visio evaluates the formula, not the cached
/// palette-index V, for themed shapes).
/// One color cell's stored value as `#rrggbb` — the shared mapping
/// behind the fill, line, and TEXT color paths (#73): a `#…` hex
/// string verbatim, a palette/document-table index, or a THEMEVAL
/// variant formula. Unresolvable values fall to black.
fn color_value(doc: &Document, v: &str, f: Option<&str>) -> String {
    if let Some(f) = f {
        if f.contains("THEMEVAL") {
            for n in 1..=8 {
                if f.contains(&format!("VariantColor{n}")) {
                    let variants = doc.theme_variant_colors();
                    if let Some(col) = variants.get(n - 1) {
                        return col.clone();
                    }
                }
            }
            // THEMEVAL with no variant we model: fall through to V
        }
    }
    let v = v.trim();
    if v.starts_with('#') {
        v.to_string()
    } else {
        match v.parse::<i64>() {
            Ok(idx) => doc.color(idx).cloned().unwrap_or_else(|| palette(idx)),
            Err(_) => "#000000".into(),
        }
    }
}

pub(crate) fn themed_color_string(
    doc: &Document,
    s: &Shape,
    chain: &[&Shape],
    cell: &str,
) -> String {
    match doc.resolve_cell(s, chain, cell) {
        Some(c) => color_value(doc, c.v.as_deref().unwrap_or(""), c.f.as_deref()),
        None => palette(-1),
    }
}

/// Standard color table for indices 0–23, verbatim from [MS-VSDX]
/// "Color Table": any integer color value ≤ 23 resolves here; ≥ 24 resolves
/// through the document `Colors` table.
fn palette(idx: i64) -> String {
    match idx {
        0 => "#000000",
        1 => "#FFFFFF",
        2 => "#FF0000",
        3 => "#00FF00",
        4 => "#0000FF",
        5 => "#FFFF00",
        6 => "#FF00FF",
        7 => "#00FFFF",
        8 => "#800000",
        9 => "#008000",
        10 => "#000080",
        11 => "#808000",
        12 => "#800080",
        13 => "#008080",
        14 => "#C0C0C0",
        15 => "#E6E6E6",
        16 => "#CDCDCD",
        17 => "#B3B3B3",
        18 => "#9A9A9A",
        19 => "#808080",
        20 => "#666666",
        21 => "#4D4D4D",
        22 => "#333333",
        23 => "#1A1A1A",
        _ => "#000000",
    }
    .into()
}

fn num(cell: Option<&Cell>) -> Option<f64> {
    cell.and_then(|c| c.numeric())
}

fn cell_num(cell: &Cell, env: &vsdx::formula::Env) -> Option<f64> {
    if let Some(v) = cell.numeric() {
        return Some(v);
    }
    let f = cell.f.as_deref()?;
    if f == "Inh" || f == "No Formula" {
        return None;
    }
    vsdx::formula::eval(f, env).ok()?.as_f64()
}

/// A cell of the first Character row, inherited per-cell the way Visio
/// resolves character formatting: the instance's own row first (master
/// instances typically carry a LangID-only row), then the source shape's
/// row via the master link, then the text-style stylesheet cascade. A row
/// that omits a cell inherits it — sections inherit separately from
/// cells, and `effective_sheet` stops at the instance sheet as soon as it
/// has any section (the same gap the Paragraph lookup in `emit_text`
/// works around).
fn char_cell<'a>(
    doc: &'a Document,
    s: &'a Shape,
    chain: &[&'a Shape],
    name: &str,
) -> Option<&'a Cell> {
    let first_row = |sheet: &'a Sheet| {
        sheet
            .sections_named("Character")
            .into_iter()
            .next()
            .and_then(|sec| sec.rows.first())
    };
    if let Some(c) = first_row(&s.sheet).and_then(|r| r.cell(name)) {
        return Some(c);
    }
    let source = doc.instance_source(s, chain);
    if let Some(src) = source {
        if let Some(c) = first_row(&src.sheet).and_then(|r| r.cell(name)) {
            return Some(c);
        }
    }
    // Text-style stylesheet cascade. The instance usually leaves the
    // style attributes to its master root; stylesheets reference their
    // parents through a TextStyle attribute.
    let style_id = s
        .text_style
        .or_else(|| source.and_then(|src| src.text_style))?;
    let mut cur = doc.stylesheet(style_id)?;
    for _ in 0..8 {
        if let Some(c) = first_row(&cur.sheet).and_then(|r| r.cell(name)) {
            return Some(c);
        }
        let next_id = cur
            .attrs
            .iter()
            .find(|(k, _)| k == "TextStyle")
            .and_then(|(_, v)| v.parse::<i64>().ok())
            .or_else(|| {
                cur.sheet
                    .cell("TextStyle")
                    .and_then(|c| c.numeric())
                    .map(|v| v as i64)
            })?;
        if next_id == cur.id {
            return None;
        }
        cur = doc.stylesheet(next_id)?;
    }
    None
}

/// First Character-section row's Size cell (inches), per-cell inherited
/// through `char_cell`.
fn char_size(doc: &Document, s: &Shape, chain: &[&Shape]) -> Option<f64> {
    char_cell(doc, s, chain, "Size")
        .and_then(|c| c.numeric())
        .filter(|v| *v > 0.0)
}

/// First Character-section row's Font cell — the family name as stored
/// in VSDX (`<Cell N='Font' V='Arial'/>`), inherited per-cell like Size.
/// Legacy binary-format font indexes (numeric values) and the `Themed`
/// placeholder (meaning "the theme's font", rendered with Visio's default
/// family) name no family.
fn char_font(doc: &Document, s: &Shape, chain: &[&Shape]) -> Option<String> {
    let name = char_cell(doc, s, chain, "Font").and_then(|c| c.v.clone())?;
    let name = name.trim();
    if name.is_empty() || name.parse::<f64>().is_ok() || name.eq_ignore_ascii_case("themed") {
        return None;
    }
    Some(name.to_string())
}

/// Metric-compatible substitutes for the Microsoft core fonts (the
/// libre designer-alternative set), with the CSS generic each family
/// degrades to. Case-insensitive lookup.
fn font_substitute(name: &str) -> Option<(&'static str, &'static str)> {
    Some(match name.trim().to_ascii_lowercase().as_str() {
        "arial" => ("Liberation Sans", "sans-serif"),
        "calibri" => ("Carlito", "sans-serif"),
        "cambria" => ("Caladea", "serif"),
        "segoe ui" => ("Carlito", "sans-serif"),
        "times new roman" => ("Liberation Serif", "serif"),
        "courier new" => ("Liberation Mono", "monospace"),
        // The other stock monospace faces: without the face installed
        // they must stay monospace (Visio 2021 witness: the network
        // LLD's Lucida Console interface labels drew in Carlito and
        // re-wrapped). Liberation Mono's 0.6 em advance matches Lucida
        // Console's; Consolas (0.55 em) is the closest the vendored
        // set offers.
        "lucida console" | "consolas" => ("Liberation Mono", "monospace"),
        _ => return None,
    })
}

/// The stock-font rule (GitHub #232, cleanroom-rebuilt from the
/// observed Visio behavior documented on the issue — never Microsoft's
/// 273-entry table): a font NOT in Visio's stock set draws in
/// **Calibri** on a stock Windows + Office install, whatever the
/// document's root style says — Visio's own SVG even names unknown
/// families Calibri. So a document naming Helvetica, Roboto, or an
/// invented family must lay out in our Calibri metric twin (Carlito),
/// or its TEXTWIDTH-driven wrapping diverges from Visio's (Calibri is
/// narrower than the look-alike faces a generic fallback picks).
///
/// The table is the well-known Windows/Office core set a stock Visio
/// install offers; extend it only with evidence. Case-insensitive.
fn is_stock_visio_font(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "arial"
            | "arial black"
            | "calibri"
            | "cambria"
            | "candara"
            | "comic sans ms"
            | "consolas"
            | "constantia"
            | "corbel"
            | "courier new"
            | "ebrima"
            | "franklin gothic medium"
            | "gabriola"
            | "gadugi"
            | "georgia"
            | "impact"
            | "lucida console"
            | "lucida sans unicode"
            | "malgun gothic"
            | "microsoft himalaya"
            | "microsoft jhenghei"
            | "microsoft yahei"
            | "mongolian baiti"
            | "ms gothic"
            | "ms mincho"
            | "ms pgothic"
            | "mv boli"
            | "palatino linotype"
            | "segoe script"
            | "segoe serif"
            | "segoe ui"
            | "simsun"
            | "sylfaen"
            | "symbol"
            | "tahoma"
            | "times new roman"
            | "trebuchet ms"
            | "verdana"
            | "webdings"
            | "wingdings"
            | "yu gothic"
    )
}

/// The measurement/paint infrastructure faces (#232): families THIS
/// app names itself (the vendored deterministic floor, the #257 RTL
/// fallback, the #282 CJK fallback) resolve as themselves — they never
/// appear in Visio files, so exempting them has no parity cost, and
/// the RTL fallback would regress to Carlito if the stock rule
/// swallowed them.
fn is_app_font(name: &str) -> bool {
    matches!(
        name.trim().to_ascii_lowercase().as_str(),
        "carlito"
            | "liberation sans"
            | "liberation serif"
            | "liberation mono"
            | "vazirmatn"
            | "noto sans sc"
            | "noto sans jp"
            | "noto sans kr"
    )
}

/// The measured Character.Pos metrics for a family (#230): the
/// reduced glyph em and the baseline shift in NOMINAL em (positive =
/// raised). Only Calibri and Arial were witnessed (a 36-case + 36-
/// control measurement on the issue): Calibri 0.667 reduced with a
/// 0.3-em superscript raise, Arial 1331/2048 ≈ 0.6499 with 0.477;
/// the subscript drop is −0.14 for both. Our metric twins inherit the
/// numbers (Carlito = Calibri, Liberation Sans = Arial); the #232
/// stock rule already reads non-stock names as Calibri, so a Roboto
/// label scripts with Calibri's numbers — exactly what Visio draws.
/// Every other family returns `None`: the block keeps its nominal
/// draw until that face is independently witnessed.
pub(crate) fn script_metrics(font: Option<&str>, pos: i32) -> Option<(f64, f64)> {
    if pos != 1 && pos != 2 {
        return None;
    }
    let named = font.map(str::trim).filter(|f| !f.is_empty());
    let key = match named {
        None => "calibri".to_string(), // the default family IS Calibri
        Some(n) if !is_stock_visio_font(n) && !is_app_font(n) => "calibri".to_string(),
        Some(n) => n.to_ascii_lowercase(),
    };
    let (reduced, super_shift) = match key.as_str() {
        "calibri" | "carlito" => (0.667, 0.3),
        "arial" | "liberation sans" => (1331.0 / 2048.0, 0.477),
        _ => return None,
    };
    let shift = match pos {
        1 => super_shift,
        _ => -0.14,
    };
    Some((reduced, shift))
}

/// Quote a font-family name for the CSS shorthand (single quotes when
/// it contains anything but letters, digits and dashes). Inside the
/// quotes, `'` and `\` are CSS-escaped so a document-supplied family
/// name can never break out of the quoted string.
fn quote_family(name: &str) -> String {
    if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// `font-family` fallback chain for a document font (GitHub #17):
/// the document's own family first (the best face on any viewer),
/// then its metric-compatible substitute if the alias table covers
/// it, then Carlito (the GUI's embedded face — also a sensible
/// last-name fallback for viewers), then the CSS generic class.
/// Text with no Font cell gets Visio's default family — Calibri —
/// through the same table (Calibri → Carlito). Deduplicated
/// case-insensitively.
/// The font-family fallback chain (UNQUOTED names, CSS generic
/// last) the renderer declares for a shape's resolved Character Font
/// — the same walk [`crate::textmetrics`] measures with, so the wrap
/// decision and the raster use one resolution (#21).
///
/// #232 stock-font rule: a family OUTSIDE Visio's stock set (and not
/// one of our own infrastructure faces) resolves to the Calibri chain
/// outright — named first, substituted last, like Visio's own SVG
/// which renames unknown families Calibri. Wrap and raster follow one
/// chain, so a Helvetica or Roboto label lays out exactly where Visio
/// would draw it (Carlito = Calibri metrics) instead of drifting onto
/// a wider look-alike.
pub(crate) fn family_chain(font: Option<&str>) -> Vec<String> {
    let name = match font.map(str::trim) {
        Some(f) if !f.is_empty() => f,
        _ => "Calibri",
    };
    if !is_stock_visio_font(name) && !is_app_font(name) {
        return vec![
            "Calibri".to_string(),
            "Carlito".to_string(),
            "sans-serif".to_string(),
        ];
    }
    let (substitute, class) = match font_substitute(name) {
        Some((s, c)) => (Some(s), c),
        None => (None, "sans-serif"),
    };
    let mut chain = vec![name.to_string()];
    for c in substitute.into_iter().chain(["Carlito"]) {
        if !chain.iter().any(|f| f.eq_ignore_ascii_case(c)) {
            chain.push(c.to_string());
        }
    }
    chain.push(class.to_string());
    chain
}

fn font_family_attr(font: Option<&str>) -> String {
    let chain: Vec<String> = family_chain(font).iter().map(|f| quote_family(f)).collect();
    // Document-supplied family names land in an XML attribute — the
    // joined CSS list is attribute content like any other.
    format!(" font-family=\"{}\"", escape_attr(&chain.join(", ")))
}

/// Resolved (Width, Height) for geometry scaling.
pub(crate) fn shape_size(doc: &Document, s: &Shape, chain: &[&Shape]) -> (f64, f64) {
    let env = shape_env(doc, s, chain);
    let get = |n: &str| {
        doc.resolve_cell(s, chain, n)
            .and_then(|c| cell_num(&c, &env))
    };
    (get("Width").unwrap_or(1.0), get("Height").unwrap_or(1.0))
}

/// Environment for evaluating geometry-row formulas: the shape's resolved
/// named cells plus this geometry section's own row cells.
fn shape_env_for(doc: &Document, s: &Shape, chain: &[&Shape], rows: &[Row]) -> vsdx::formula::Env {
    let mut env = shape_env(doc, s, chain);
    for row in rows.iter().filter(|r| r.t.is_some()) {
        for (cname, cell) in &row.cells {
            if let Some(v) = cell.numeric() {
                env.insert(format!("{}.{}", cname, row.ix), v);
            }
        }
    }
    env
}

/// Build an evaluation environment for a shape: resolved cells by name,
/// two passes (values first, then formulas against those values).
pub fn shape_env(doc: &Document, s: &Shape, chain: &[&Shape]) -> vsdx::formula::Env {
    let names = [
        "PinX",
        "PinY",
        "Width",
        "Height",
        "LocPinX",
        "LocPinY",
        "Angle",
        "TxtPinX",
        "TxtPinY",
        "TxtWidth",
        "TxtHeight",
        "LineWeight",
        "Char.Size",
    ];
    let mut env = vsdx::formula::Env::new();
    for n in names {
        if let Some(c) = doc.resolve_cell(s, chain, n) {
            if let Some(v) = c.numeric() {
                env.insert(n.to_string(), v);
            }
        }
    }
    // User-defined cells (master geometry formulas reference
    // `User.Corner` and friends for rounded-corner rectangles).
    // Instance-local rows override the master's: inline copies keep
    // only sparse User caches (e.g. a heading's HTxtWidth) while the
    // rest — CStyle, hidden, … — inherit from the master source, so
    // style-gated section cells like NoShow re-evaluate per instance.
    let own = doc.effective_sheet(s, chain);
    let mut user_sheets: Vec<&Sheet> = Vec::new();
    if let Some(src) = doc.instance_source(s, chain) {
        if !std::ptr::eq(&src.sheet, own.as_ref()) {
            user_sheets.push(&src.sheet);
        }
    }
    user_sheets.push(own.as_ref());
    for sheet in user_sheets {
        for user in sheet.sections_named("User") {
            for row in user.rows.iter().filter(|r| r.n.is_some()) {
                let key = format!("User.{}", row.n.as_deref().unwrap_or(""));
                let inh = row
                    .cell("Value")
                    .and_then(|c| c.f.as_deref())
                    .is_some_and(|f| f == "Inh");
                match row.cell("Value").and_then(|c| c.numeric()) {
                    // The master sheet seeds first; an Inh-marked
                    // local row's numeric cache is the STALE snapshot
                    // and must not override it (CodeRabbit agent
                    // review — the string-side twin of the round-5
                    // render_field rule).
                    Some(v) => {
                        if inh && env.contains_key(&key) {
                            continue;
                        }
                        env.insert(key, v);
                    }
                    // A row whose Value cache is EXPLICITLY emptied
                    // (V='') clears the inherited number: the string
                    // env seeds the empty binding, but the numeric
                    // layer kept the master's 42 for the evaluator to
                    // read FIRST (CodeRabbit PR review). An Inh-marked
                    // EMPTY cache inherits instead of clearing. No
                    // cache at all (a sparse F-only row) still
                    // inherits.
                    None if row
                        .cell("Value")
                        .is_some_and(|c| c.v.is_some() && c.f.as_deref() != Some("Inh")) =>
                    {
                        env.remove(&key);
                    }
                    None => {}
                }
            }
        }
        // Scratch + Controls numeric cells (#249): the connector
        // stencil's band/knee masters derive their geometry rows
        // through them (`LineTo(F:Scratch.X2*Scratch.C1)`,
        // `Controls.Row_1.Y`) — without them every such row fell to
        // its authored cached pose and the ink never tracked the
        // endpoints. Scratch references are `Scratch.X1` (row 1's X
        // cell); Controls references carry the row's name
        // (`Controls.Row_1.Y`).
        for scratch in sheet.sections_named("Scratch") {
            for row in &scratch.rows {
                for (name, cell) in &row.cells {
                    if let Some(v) = cell.numeric() {
                        // Visio's Scratch references are 1-BASED
                        // (`Scratch.X1` is the first row's X).
                        env.insert(format!("Scratch.{}{}", name, row.ix + 1), v);
                    }
                }
            }
        }
        for controls in sheet.sections_named("Controls") {
            for row in &controls.rows {
                let rn = row.n.clone().unwrap_or_else(|| format!("Row_{}", row.ix));
                for (name, cell) in &row.cells {
                    if let Some(v) = cell.numeric() {
                        env.insert(format!("Controls.{}.{}", rn, name), v);
                    }
                }
            }
        }
    }
    // Geometry row coordinates (Geometry1.X1 style references).
    for (i, geo) in own.sections_named("Geometry").iter().enumerate() {
        let gname = format!("Geometry{}", i + 1);
        for row in geo.rows.iter().filter(|r| r.t.is_some()) {
            for (cname, cell) in &row.cells {
                if let Some(v) = cell.numeric() {
                    env.insert(format!("{}.{}{}", gname, cname, row.ix), v);
                }
            }
        }
    }
    // Second pass: formulas may reference the first-pass values.
    for n in names {
        if let Some(c) = doc.resolve_cell(s, chain, n) {
            if c.numeric().is_none() {
                if let Some(f) = c.f.as_deref() {
                    if let Some(v) = vsdx::formula::eval(f, &env).ok().and_then(|v| v.as_f64()) {
                        env.insert(n.to_string(), v);
                    }
                }
            }
        }
    }
    env
}

pub(crate) fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use vsdx::model::{IndexChild, Page};

    /// Synthetic Role-callout-shaped master (issue #15 specimen, values
    /// recomputed — no Microsoft content): a root group whose children
    /// mix proportional XForms (`Sheet.5!Width*1`) with GUARDed absolute
    /// ones (`GUARD(4MM)`), driven by User cells whose formulas reference
    /// the group Height.
    fn role_master_fixture() -> (Shape, Shape) {
        let set = |sheet: &mut Sheet, n: &str, v: f64, f: Option<&str>| {
            sheet.set_cell(
                n,
                Cell {
                    v: Some(format!("{v}")),
                    f: f.map(str::to_string),
                    ..Cell::default()
                },
            );
        };
        // Root: master canvas 1.1811 × 0.7874.
        let mut root = Shape {
            id: 5,
            ..Shape::default()
        };
        set(&mut root.sheet, "Width", 1.181102362204724, None);
        set(&mut root.sheet, "Height", 0.7874015748031495, None);
        let urow = |n: &str, v: f64, f: Option<&str>| Row {
            n: Some(n.into()),
            cells: vec![(
                "Value".into(),
                Cell {
                    v: Some(format!("{v}")),
                    f: f.map(str::to_string),
                    ..Cell::default()
                },
            )],
            ..Row::default()
        };
        root.sheet.children.push(SheetChild::Section(Section {
            name: "User".into(),
            rows: vec![
                urow("ShapeLayout", 0.0, None),
                urow("InfoPanelHeight", 0.2362204724409449, None),
                urow("IconX", 0.1181102362204724, Some("User.InfoPanelHeight/2")),
                urow(
                    "IconY",
                    0.6692913385826771,
                    Some("IF(OR(User.ShapeLayout<>1), Height-User.InfoPanelHeight/2, User.InfoPanelHeight/2)"),
                ),
            ],
            ..Section::default()
        }));
        // Container child: fully proportional.
        let mut container = Shape {
            id: 6,
            ..Shape::default()
        };
        set(
            &mut container.sheet,
            "Width",
            1.181102362204721,
            Some("Sheet.5!Width*1"),
        );
        set(
            &mut container.sheet,
            "Height",
            0.7874015748031491,
            Some("Sheet.5!Height*1"),
        );
        set(
            &mut container.sheet,
            "PinX",
            0.5905511811023657,
            Some("Sheet.5!Width*0.5"),
        );
        set(
            &mut container.sheet,
            "PinY",
            0.3937007874015748,
            Some("Sheet.5!Height*0.5"),
        );
        set(
            &mut container.sheet,
            "LocPinX",
            0.5905511811023605,
            Some("Width*0.5"),
        );
        set(
            &mut container.sheet,
            "LocPinY",
            0.3937007874015746,
            Some("Height*0.5"),
        );
        // Icon child: absolute size, pinned via User cells.
        let mut icon = Shape {
            id: 8,
            ..Shape::default()
        };
        set(
            &mut icon.sheet,
            "Width",
            0.1574803149606299,
            Some("GUARD(4MM)"),
        );
        set(
            &mut icon.sheet,
            "Height",
            0.1692913385826771,
            Some("GUARD(4.3MM)"),
        );
        set(
            &mut icon.sheet,
            "PinX",
            0.1181102362204724,
            Some("GUARD(Sheet.5!User.IconX)"),
        );
        set(
            &mut icon.sheet,
            "PinY",
            0.6692913385826771,
            Some("GUARD(Sheet.5!User.IconY)"),
        );
        set(
            &mut icon.sheet,
            "LocPinX",
            0.07874015748031496,
            Some("Width*0.5"),
        );
        set(
            &mut icon.sheet,
            "LocPinY",
            0.08464566929133857,
            Some("Height*0.5"),
        );
        // Literal child (no formulas): uniform-scale fallback.
        let mut plain = Shape {
            id: 16,
            ..Shape::default()
        };
        set(&mut plain.sheet, "Width", 0.5, None);
        set(&mut plain.sheet, "Height", 0.25, None);
        set(&mut plain.sheet, "PinX", 0.5, None);
        set(&mut plain.sheet, "PinY", 0.25, None);
        root.sub_shapes = vec![container, icon, plain];

        // Instance: same master stretched tall (2.25 × 7.02 aspect change).
        let mut inst = Shape {
            id: 65,
            master: Some(7),
            ..Shape::default()
        };
        set(&mut inst.sheet, "Width", 2.657480296178819, None);
        set(&mut inst.sheet, "Height", 5.528301723437821, None);
        (root, inst)
    }

    /// Parse `matrix(a,b,c,d,e,f)` / `scale(sx,sy)` into (a..f).
    fn parse_tf(t: &str) -> [f64; 6] {
        let body = t
            .trim_start_matches("matrix(")
            .trim_start_matches("scale(")
            .trim_end_matches(')');
        let nums: Vec<f64> = body.split(',').map(|n| n.parse().unwrap()).collect();
        if t.starts_with("scale(") {
            [nums[0], 0.0, 0.0, nums[1], 0.0, 0.0]
        } else {
            nums.try_into().unwrap()
        }
    }

    #[test]
    fn mirrored_proportional_child_keeps_uniform_scale() {
        let (root, inst) = role_master_fixture();
        let (iw, ih) = (2.657480296178819, 5.528301723437821);
        let (mw, mh) = (1.181102362204724, 0.7874015748031495);
        let (kx, ky) = (iw / mw, ih / mh); // 2.25 × 7.020943
        let env = mirror_env(&root, &inst, iw, ih);
        // Container (Sheet.5!Width*1 …): exactly the historical uniform
        // scale — no regression for proportional children.
        let t = mirror_child_transform(&root.sub_shapes[0], &env, (kx, ky)).unwrap();
        assert_eq!(t, "scale(2.250000,7.020943)", "got {t}");
    }

    #[test]
    fn mirrored_guarded_child_keeps_absolute_size() {
        let (root, inst) = role_master_fixture();
        let (iw, ih) = (2.657480296178819, 5.528301723437821);
        let env = mirror_env(&root, &inst, iw, ih);
        // Icon: 4×4.3 mm stays absolute; its pin re-evaluates against the
        // instance Height (User.IconY = H − InfoPanelHeight/2).
        let t = mirror_child_transform(&root.sub_shapes[1], &env, (2.25, 7.02)).unwrap();
        let m = parse_tf(&t);
        let e_py = ih - 0.2362204724409449 / 2.0; // 5.410191487…
        let m_py = 0.6692913385826771;
        let m_h = 0.1692913385826771;
        // wanted ∘ placed⁻¹ for angle 0, scale 1: pure translate by the
        // pin delta (effective LocPin = W/2 in both frames).
        let expected_ty = -((e_py - 0.1692913385826771 / 2.0) - (m_py - m_h / 2.0));
        assert!((m[0] - 1.0).abs() < 1e-6, "a={}", m[0]);
        assert!((m[3] - 1.0).abs() < 1e-6, "d={}", m[3]);
        assert!(m[4].abs() < 1e-6, "e={}", m[4]);
        assert!(
            (m[5] - expected_ty).abs() < 1e-5,
            "f={} expected {expected_ty}",
            m[5]
        );
    }

    #[test]
    fn mirrored_literal_child_falls_back_to_uniform_scale() {
        let (root, inst) = role_master_fixture();
        let env = mirror_env(&root, &inst, 2.657480296178819, 5.528301723437821);
        // No formulas anywhere: behavior is byte-identical to the old
        // uniform wrapper.
        let t = mirror_child_transform(&root.sub_shapes[2], &env, (2.25, 7.020943)).unwrap();
        assert_eq!(t, "scale(2.250000,7.020943)", "got {t}");
    }

    #[test]
    fn mirror_env_resolves_user_formulas_against_instance_height() {
        let (root, inst) = role_master_fixture();
        let env = mirror_env(&root, &inst, 2.657480296178819, 5.528301723437821);
        let v = env.get("User.IconY").copied().unwrap();
        assert!((v - (5.528301723437821 - 0.2362204724409449 / 2.0)).abs() < 1e-9);
    }

    /// A locally CLEARED User row (`V=''`) shadows the master's
    /// numeric binding — the string env seeds the empty value, and
    /// the numeric env must not keep the master's 42 for the
    /// evaluator to read first (CodeRabbit round 5). A restated
    /// local value still overrides, and no local row still inherits.
    #[test]
    fn cleared_user_row_shadows_the_master_number() {
        let user_sheet = |v: Option<&str>| {
            let mut sheet = Sheet::default();
            sheet.children.push(vsdx::model::SheetChild::Section(
                vsdx::model::Section {
                    name: "User".into(),
                    rows: vec![vsdx::model::Row {
                        n: Some("Label".into()),
                        cells: vec![(
                            "Value".to_string(),
                            Cell {
                                v: v.map(str::to_string),
                                ..Cell::default()
                            },
                        )],
                        ..vsdx::model::Row::default()
                    }],
                    ..vsdx::model::Section::default()
                },
            ));
            sheet
        };
        let mut doc = Document::default();
        doc.masters.push(vsdx::model::Master {
            id: 7,
            shapes: vec![Shape {
                id: 1,
                sheet: user_sheet(Some("42")),
                ..Shape::default()
            }],
            ..vsdx::model::Master::default()
        });
        // No local row: the master's 42 inherits.
        let inherit = Shape {
            id: 1,
            master: Some(7),
            ..Shape::default()
        };
        let chain = [&inherit];
        assert_eq!(
            shape_env(&doc, &inherit, &chain).get("User.Label").copied(),
            Some(42.0),
            "no local row inherits the master's number"
        );
        // A cleared local row removes the binding.
        let cleared = Shape {
            id: 1,
            master: Some(7),
            sheet: user_sheet(Some("")),
            ..Shape::default()
        };
        let chain = [&cleared];
        assert_eq!(
            shape_env(&doc, &cleared, &chain).get("User.Label"),
            None,
            "a cleared row shadows the master's number"
        );
        // A restated local value overrides.
        let local = Shape {
            id: 1,
            master: Some(7),
            sheet: user_sheet(Some("7")),
            ..Shape::default()
        };
        let chain = [&local];
        assert_eq!(
            shape_env(&doc, &local, &chain).get("User.Label").copied(),
            Some(7.0),
            "a local value overrides the master's"
        );

        // An Inh-marked row inherits instead of overriding — its
        // numeric cache is the stale snapshot; and an Inh-marked
        // EMPTY cache inherits rather than clearing (CodeRabbit
        // agent review).
        let with_row = |f: Option<&str>, v: Option<&str>| {
            let mut sheet = vsdx::model::Sheet::default();
            sheet.children.push(vsdx::model::SheetChild::Section(
                vsdx::model::Section {
                    name: "User".into(),
                    rows: vec![vsdx::model::Row {
                        n: Some("Label".into()),
                        cells: vec![(
                            "Value".to_string(),
                            vsdx::model::Cell {
                                v: v.map(str::to_string),
                                f: f.map(str::to_string),
                                ..vsdx::model::Cell::default()
                            },
                        )],
                        ..vsdx::model::Row::default()
                    }],
                    ..vsdx::model::Section::default()
                },
            ));
            sheet
        };
        let mut doc2 = Document::default();
        doc2.masters.push(vsdx::model::Master {
            id: 8,
            shapes: vec![Shape {
                id: 1,
                sheet: user_sheet(Some("42")),
                ..Shape::default()
            }],
            ..vsdx::model::Master::default()
        });
        let stale = Shape {
            id: 1,
            master: Some(8),
            sheet: with_row(Some("Inh"), Some("41")),
            ..Shape::default()
        };
        let chain = [&stale];
        assert_eq!(
            shape_env(&doc2, &stale, &chain).get("User.Label").copied(),
            Some(42.0),
            "an Inh row's stale cache does not override the master"
        );
        let inh_empty = Shape {
            id: 1,
            master: Some(8),
            sheet: with_row(Some("Inh"), Some("")),
            ..Shape::default()
        };
        let chain = [&inh_empty];
        assert_eq!(
            shape_env(&doc2, &inh_empty, &chain).get("User.Label").copied(),
            Some(42.0),
            "an Inh row's empty cache inherits, it does not clear"
        );
    }

    #[test]
    fn line_pattern_dasharrays_scale_with_weight() {
        // #171 measured the INK (the owner-authored lines.vsdx fixture
        // printed at 600 dpi — Analysis/line-patterns-visio2016.md);
        // #235 reconciles it with MisterDiagram's canonical grammar:
        // Visio strokes dashes with ROUND caps, so the emitted
        // geometry is ink − 1w per mark / ink + 1w per gap (pattern 2
        // = the canonical 7/5, 14 = 9/3/3, 21 = 39/9/15/9), and dots
        // (ink 1w) become the 0.0001w tangent lead with the following
        // gap returning it — same painted ink, canonical parameters.
        // The values below are the CANONICAL GEOMETRY × weight, dotted
        // against the three witnessed grammar entries.
        let w = 0.02; // the fixture's weight
        let arr = |p: f64| line_dasharray(p, w);
        assert_eq!(arr(2.0).as_deref(), Some("0.14000 0.10000")); // canonical 7/5
        assert_eq!(arr(3.0).as_deref(), Some("0.0000020000 0.10000"));
        assert_eq!(
            arr(4.0).as_deref(),
            Some("0.14000 0.10000 0.0000020000 0.10000")
        );
        assert_eq!(
            arr(5.0).as_deref(),
            Some("0.14000 0.10000 0.0000020000 0.10000 0.0000020000 0.10000")
        );
        assert_eq!(
            arr(6.0).as_deref(),
            Some("0.14000 0.10000 0.14000 0.10000 0.0000020000 0.10000")
        );
        assert_eq!(arr(7.0).as_deref(), Some("0.38000 0.10000 0.14000 0.10000"));
        assert_eq!(
            arr(8.0).as_deref(),
            Some("0.38000 0.10000 0.14000 0.10000 0.14000 0.10000")
        );
        assert_eq!(arr(9.0).as_deref(), Some("0.06000 0.06000"));
        assert_eq!(arr(10.0).as_deref(), Some("0.0000020000 0.06000"));
        assert_eq!(
            arr(11.0).as_deref(),
            Some("0.06000 0.06000 0.0000020000 0.06000")
        );
        assert_eq!(
            arr(12.0).as_deref(),
            Some("0.06000 0.06000 0.0000020000 0.06000 0.0000020000 0.06000")
        );
        assert_eq!(
            arr(13.0).as_deref(),
            Some("0.06000 0.06000 0.06000 0.06000 0.0000020000 0.06000")
        );
        assert_eq!(
            arr(14.0).as_deref(),
            Some("0.18000 0.06000 0.06000 0.06000") // canonical 9/3/3
        );
        assert_eq!(
            arr(15.0).as_deref(),
            Some("0.18000 0.06000 0.06000 0.06000 0.06000 0.06000")
        );
        assert_eq!(arr(16.0).as_deref(), Some("0.30000 0.18000"));
        assert_eq!(arr(17.0).as_deref(), Some("0.0000020000 0.18000"));
        assert_eq!(
            arr(18.0).as_deref(),
            Some("0.30000 0.18000 0.0000020000 0.18000")
        );
        assert_eq!(
            arr(19.0).as_deref(),
            Some("0.30000 0.18000 0.0000020000 0.18000 0.0000020000 0.18000")
        );
        assert_eq!(
            arr(20.0).as_deref(),
            Some("0.30000 0.18000 0.30000 0.18000 0.0000020000 0.18000")
        );
        assert_eq!(
            arr(21.0).as_deref(),
            Some("0.78000 0.18000 0.30000 0.18000") // canonical 39/9/15/9
        );
        assert_eq!(
            arr(22.0).as_deref(),
            Some("0.78000 0.18000 0.30000 0.18000 0.30000 0.18000")
        );
        assert_eq!(arr(23.0).as_deref(), Some("0.02000 0.04000"));
        // Solid (1) and hidden (0, handled at the call site) render
        // without a dasharray; so do out-of-gallery values.
        assert!(line_dasharray(1.0, w).is_none());
        assert!(line_dasharray(0.0, w).is_none());
        assert!(line_dasharray(24.0, w).is_none());
    }

    /// A Character row with Font/Size/Color cells for char_font tests.
    fn char_row(font: Option<&str>) -> Row {
        let mut cells = Vec::new();
        if let Some(f) = font {
            cells.push((
                "Font".to_string(),
                Cell {
                    v: Some(f.to_string()),
                    ..Cell::default()
                },
            ));
        }
        Row {
            cells,
            ..Row::default()
        }
    }

    fn shape_with_character(font: Option<&str>) -> Shape {
        let mut s = Shape::default();
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Character".into(),
            rows: vec![char_row(font)],
            ..Section::default()
        }));
        s
    }

    /// #72 + #199 + #185: arrowheads render per value at their
    /// measured physical sizes — each gallery entry emits its own
    /// head group matching the 600 dpi Visio print (open heads 14–22
    /// stroke, the dimension set composes ticks with rings/dots, 12
    /// is the single wide V, 43 the double chevron), the head scales
    /// with the LineWeight + C[size] unit law, and 0/46–50 emit none.
    /// #218 review round: fractional NURBS controls (xType/yType = 0)
    /// in the MASTER frame scale exactly once — a fraction lands at
    /// gx·W instance units (gx·mw master units through the one
    /// master→instance scale), never the old gx·W·scale. A degree-1
    /// NURBS flattens through its control points, so the emitted
    /// path pins them.
    #[test]
    fn nurbs_fractional_controls_scale_once_in_master_frame() {
        // Master: a 2×2 canvas; a degree-1 NURBS whose interior
        // controls sit at x fractions 0.5 and 0.75, y fraction 0.5.
        let mut mroot = Shape {
            id: 1,
            ..Shape::default()
        };
        for (n, v) in [("Width", "2"), ("Height", "2")] {
            mroot.sheet.set_cell(n, Cell::value(v));
        }
        mroot.sheet.children.push(SheetChild::Section(Section {
            name: "Geometry".into(),
            ix: 0,
            rows: vec![
                Row {
                    ix: 1,
                    t: Some("MoveTo".into()),
                    cells: vec![
                        ("X".into(), Cell::value("0")),
                        ("Y".into(), Cell::value("0")),
                    ],
                    ..Row::default()
                },
                Row {
                    ix: 2,
                    t: Some("NURBSTo".into()),
                    cells: vec![
                        ("X".into(), Cell::value("2")),
                        ("Y".into(), Cell::value("2")),
                        ("A".into(), Cell::value("2")),
                        ("B".into(), Cell::value("1")),
                        ("C".into(), Cell::value("0")),
                        ("D".into(), Cell::value("1")),
                        (
                            "E".into(),
                            Cell {
                                v: Some("NURBS(3, 1, 0, 0, 0.5,0.5,1,1, 0.75,0.5,2,1)".into()),
                                ..Cell::default()
                            },
                        ),
                    ],
                    ..Row::default()
                },
            ],
            ..Section::default()
        }));
        let doc = Document {
            masters: vec![vsdx::model::Master {
                id: 7,
                shapes: vec![mroot],
                ..vsdx::model::Master::default()
            }],
            ..Document::default()
        };
        // Instance: 4×2 — double the master width, equal height.
        let mut inst = Shape {
            id: 9,
            master: Some(7),
            ..Shape::default()
        };
        for (n, v) in [
            ("PinX", "3"),
            ("PinY", "3"),
            ("Width", "4"),
            ("Height", "2"),
            ("LocPinX", "0"),
            ("LocPinY", "0"),
        ] {
            inst.sheet.set_cell(n, Cell::value(v));
        }
        let chain: Vec<&Shape> = Vec::new();
        let mut out = String::new();
        render_shape(
            &doc, &inst, 8.5, &chain, "", None, None, None, None, (0.0, 0.0), &mut out,
        );
        // The controls land at gx·W: 0.5 → 2.0 and 0.75 → 3.0; the y
        // fraction 0.5 of H=2 → 1.0 (emitted negated). The old
        // double-scale put them at 4.0 and 6.0.
        assert!(
            out.contains("2.00000 -1.00000"),
            "control 1 (0.5·W) missing: {out}"
        );
        assert!(
            out.contains("3.00000 -1.00000"),
            "control 2 (0.75·W) missing: {out}"
        );
        assert!(
            !out.contains("6.00000"),
            "double-scaled control leaked: {out}"
        );
    }

    #[test]
    fn arrow_heads_render_per_value_and_scale() {
        let doc = Document::default();
        let line_with = |cells: &[(&str, &str)]| {
            let mut s = Shape::default();
            for (n, v) in cells {
                s.sheet.set_cell(n, Cell::value(*v));
            }
            s.sheet.children.push(SheetChild::Section(Section {
                name: "Geometry".into(),
                ix: 0,
                rows: vec![
                    Row {
                        ix: 1,
                        t: Some("MoveTo".into()),
                        cells: vec![
                            ("X".into(), Cell::value("0")),
                            ("Y".into(), Cell::value("0")),
                        ],
                        ..Row::default()
                    },
                    Row {
                        ix: 2,
                        t: Some("LineTo".into()),
                        cells: vec![
                            ("X".into(), Cell::value("1")),
                            ("Y".into(), Cell::value("1")),
                        ],
                        ..Row::default()
                    },
                ],
                ..Section::default()
            }));
            let chain = [&s];
            let mut out = String::new();
            render_shape(&doc, &s, 11.0, &chain, "", None, None, None, None, (0.0, 0.0), &mut out);
            out
        };
        // 0 = no head at all.
        let none = line_with(&[("EndArrow", "0")]);
        assert!(!none.contains("dc-ah"), "value 0 emits no head: {none}");
        // Each gallery value emits a head group with its own unit-space
        // shape (#169 + #199 + the #185 print re-measure).
        for (v, needle) in [
            (1, "M-1,-1 L0,0 L-1,1"),
            (2, "M0,0 L-1,-1 L-1,1 Z"),
            (3, "M-2,-1 L0,0 L-2,1"),
            (4, "M0,0 L-2,-1 L-2,1 Z"),
            (5, "C-1.68524,-0.37049"),
            (6, "C-2.31476,-0.37049"),
            (9, "M1,-1 L-1,1"),
            (11, "M-0.75,-0.75 L0.75,-0.75 L0.75,0.75 L-0.75,0.75 Z"),
            (12, "M-3,-1 L0,0 L-3,1"),
            (13, "M0,0 L-3,-1 L-3,1 Z"),
            (14, "M0,0 L-3,-1 L-3,1 Z"),
            (15, "M0,0 L-1,-1 L-1,1 Z"),
            (22, "M0,0 L-2.25,-1.125 L-4.5,0 L-2.25,1.125 Z"),
            (25, "M-2.25,-0.75 L-2.25,0.75"),
            (27, "M0,-0.75 L-1.5,0 L0,0.75"),
            (40, "M0,0 L-2,-1 L-2,1 Z M-2,0 L-4,-1 L-4,1 Z"),
            (43, "M-4,-1 L-2,0 L-4,1"),
            (45, "M-5,-0.75 L-5,0.75"),
        ] {
            let out = line_with(&[("EndArrow", &v.to_string())]);
            assert!(out.contains("dc-ah"), "value {v} emits a head group: {out}");
            assert!(out.contains(needle), "value {v} draws its shape: {out}");
        }
        // The dots/rings are circle elements (10/42 filled, 20/41
        // rings; the 35–37 dots carry the measured 0.825 radius).
        for (v, ring) in [(10i64, false), (20, true), (41, true), (42, false)] {
            let out = line_with(&[("EndArrow", &v.to_string())]);
            assert!(out.contains("<circle"), "value {v} is a circle: {out}");
            let head = out.split(r#"class="dc-ah""#).nth(1).expect("head group");
            assert_eq!(head.contains("fill=\"none\""), ring, "value {v}: {out}");
        }
        let dot35 = line_with(&[("EndArrow", "35")]);
        assert!(
            dot35.contains(r#"r="0.825""#) && dot35.contains("M-2.25,-0.75 L-2.25,0.75"),
            "value 35 composes tick + 0.825 dot: {dot35}"
        );
        // 8 shares the stealth-dart glyph with 5 (the native SVG alias;
        // the print's concave rear agrees).
        let a5 = line_with(&[("EndArrow", "5")]);
        let a8 = line_with(&[("EndArrow", "8")]);
        let d5 = a5.split(r#"class="dc-ah""#).nth(1).expect("5");
        let d8 = a8.split(r#"class="dc-ah""#).nth(1).expect("8");
        assert_eq!(d5, d8, "8 renders the 5 glyph");
        // #169 regression: 13 is a SINGLE head — one closed triangle —
        // and the other filled singles (2, 4) never stack.
        for v in [2i64, 4, 13] {
            let out = line_with(&[("EndArrow", &v.to_string())]);
            let closes = out.matches(" Z").count();
            assert!(closes <= 1, "value {v} must not stack triangles: {out}");
        }
        // #199: the open heads 14–22 stroke (fill="none"), never fill;
        // the double triangles 39/40 stack two heads, filled vs open.
        for v in 14..=22i64 {
            let out = line_with(&[("EndArrow", &v.to_string())]);
            assert!(
                out.contains("fill=\"none\""),
                "value {v} is an open head: {out}"
            );
        }
        let d39 = line_with(&[("EndArrow", "39")]);
        assert_eq!(d39.matches(" Z").count(), 2, "39 stacks two: {d39}");
        let d40 = line_with(&[("EndArrow", "40")]);
        assert_eq!(d40.matches(" Z").count(), 2, "40 stacks two: {d40}");
        assert!(d40.contains("fill=\"none\""), "40 is the open double");
        // #158: past the gallery, Visio draws a bare end (fixture
        // values 46–50 print no head).
        for v in 46..=50i64 {
            let out = line_with(&[("EndArrow", &v.to_string())]);
            assert!(!out.contains("dc-ah"), "value {v} emits no head: {out}");
        }
        // The unit law (#185): the head scales by LineWeight + C[size]
        // — size 1 (C=1.8 pt) at a 0.05 in pen gives 0.075 in.
        let heavy = line_with(&[
            ("EndArrow", "1"),
            ("EndArrowSize", "1"),
            ("LineWeight", "0.05"),
        ]);
        assert!(
            heavy.contains("scale(0.07500)"),
            "the head rides LineWeight + C[size]: {heavy}"
        );
        // Begin + end heads face outward/along the travel: the
        // diagonal runs to (1, -1) in the y-down frame, so the end
        // head points at -45° and the begin head outward at 135°.
        let both = line_with(&[("BeginArrow", "1"), ("EndArrow", "1")]);
        assert!(
            both.contains("rotate(-45.0000)"),
            "end head along the travel: {both}"
        );
        assert!(
            both.contains("rotate(135.0000)"),
            "begin head faces outward: {both}"
        );
        // The shaft trims at the head's rear (setback 2 × unit for the
        // classic triangle), the head anchoring at the ORIGINAL end.
        let unit = 0.75 / 72.0 + 2.52 / 72.0;
        let t = (2.0f64.sqrt() - 2.0 * unit) / 2.0f64.sqrt();
        let trimmed = line_with(&[("EndArrow", "4")]);
        assert!(
            trimmed.contains(&format!("L {:.5} -{:.5}", t, t)),
            "the shaft stops at the setback: {trimmed}"
        );
        assert!(
            trimmed.contains("translate(1.00000"),
            "the head keeps the original endpoint: {trimmed}"
        );
        // The begin setback is the end setback minus max(1%, 0.36 pt).
        let both4 = line_with(&[("BeginArrow", "4"), ("EndArrow", "4")]);
        let begin_setback = 2.0 * unit - (2.0 * unit * 0.01).max(0.36 / 72.0);
        let bt = begin_setback / 2.0f64.sqrt();
        assert!(
            both4.contains(&format!("M {:.5} -{:.5}", bt, bt)),
            "the begin trim is the smaller one: {both4}"
        );
    }

    /// A geometry row for the wave-3 shape fixtures.
    fn w3_row(ix: i64, t: &str, cells: &[(&str, &str)]) -> Row {
        Row {
            ix,
            t: Some(t.to_string()),
            cells: cells
                .iter()
                .map(|(n, v)| (n.to_string(), Cell::value(*v)))
                .collect(),
            ..Row::default()
        }
    }

    /// #227: a geometry whose walk closes draws no line ends — Visio
    /// renders a closed rectangle with EndArrow=5 as a clean corner
    /// (MisterDiagram #105 field batch); an open control line beside
    /// it keeps its head, and the closed section keeps its stroke.
    #[test]
    fn closed_geometry_draws_no_arrowheads() {
        let doc = Document::default();
        let render = |rows: Vec<Row>, cells: &[(&str, &str)]| {
            let mut s = Shape::default();
            for (n, v) in cells {
                s.sheet.set_cell(n, Cell::value(*v));
            }
            s.sheet.children.push(SheetChild::Section(Section {
                name: "Geometry".into(),
                ix: 0,
                rows,
                ..Section::default()
            }));
            let chain = [&s];
            let mut out = String::new();
            render_shape(&doc, &s, 11.0, &chain, "", None, None, None, None, (0.0, 0.0), &mut out);
            out
        };
        // A closed rectangle walk (returns to its start) with both
        // arrow cells set.
        let closed = render(
            vec![
                w3_row(1, "MoveTo", &[("X", "0"), ("Y", "0")]),
                w3_row(2, "LineTo", &[("X", "1"), ("Y", "0")]),
                w3_row(3, "LineTo", &[("X", "1"), ("Y", "1")]),
                w3_row(4, "LineTo", &[("X", "0"), ("Y", "1")]),
                w3_row(5, "LineTo", &[("X", "0"), ("Y", "0")]),
            ],
            &[("EndArrow", "5"), ("BeginArrow", "5")],
        );
        assert!(
            !closed.contains("dc-ah"),
            "a closed walk draws no line ends: {closed}"
        );
        assert!(
            closed.contains("<path") && closed.contains("L 0.00000 -0.00000"),
            "the closed contour still strokes: {closed}"
        );
        // The open control keeps its head.
        let open = render(
            vec![
                w3_row(1, "MoveTo", &[("X", "0"), ("Y", "0")]),
                w3_row(2, "LineTo", &[("X", "1"), ("Y", "1")]),
            ],
            &[("EndArrow", "5")],
        );
        assert!(
            open.contains("dc-ah"),
            "the open line keeps its head: {open}"
        );
        // A MULTI-subpath walk takes no heads either — Visio gives
        // them none (the trim_path evidence note; CodeRabbit flagged
        // the last-subpath-closed case, #278).
        let multi = render(
            vec![
                w3_row(1, "MoveTo", &[("X", "0"), ("Y", "0")]),
                w3_row(2, "LineTo", &[("X", "1"), ("Y", "0")]),
                w3_row(3, "MoveTo", &[("X", "2"), ("Y", "0")]),
                w3_row(4, "LineTo", &[("X", "3"), ("Y", "1")]),
            ],
            &[("EndArrow", "5"), ("BeginArrow", "5")],
        );
        assert!(
            !multi.contains("dc-ah"),
            "a multi-subpath walk draws no line ends: {multi}"
        );
    }

    /// #233: an authored D=0 (zero-width) EllipticalArcTo draws the
    /// endpoint CHORD even when its A/B control is off the chord —
    /// the old fallback's quadratic-through-m doubled back
    /// (MisterDiagram M5.50; 887 corpus masters carry these rows). A
    /// positive-width control on the same points still solves as an
    /// arc.
    #[test]
    fn zero_width_elliptical_arc_to_draws_the_chord() {
        let doc = Document::default();
        let render = |d: &str| {
            let mut s = Shape::default();
            s.sheet.set_cell("Width", Cell::value("2"));
            s.sheet.set_cell("Height", Cell::value("2"));
            s.sheet.children.push(SheetChild::Section(Section {
                name: "Geometry".into(),
                ix: 0,
                rows: vec![
                    w3_row(1, "MoveTo", &[("X", "0"), ("Y", "0")]),
                    w3_row(
                        2,
                        "EllipticalArcTo",
                        &[
                            ("X", "1"),
                            ("Y", "0"),
                            ("A", "0.5"),
                            ("B", "1"),
                            ("C", "0"),
                            ("D", d),
                        ],
                    ),
                ],
                ..Section::default()
            }));
            let chain = [&s];
            let mut out = String::new();
            render_shape(&doc, &s, 11.0, &chain, "", None, None, None, None, (0.0, 0.0), &mut out);
            out
        };
        let flat = render("0");
        assert!(
            flat.contains("L 1.00000 -0.00000"),
            "D=0 draws the endpoint chord: {flat}"
        );
        assert!(!flat.contains("Q "), "no doubled-back quadratic: {flat}");
        assert!(
            !flat.contains(" A "),
            "no solved arc for a zero-width row: {flat}"
        );
        let arc = render("0.5");
        assert!(
            arc.contains(" A "),
            "a positive width still solves an arc: {arc}"
        );
    }

    /// #235: an explicit-zero LineWeight keeps zero — SVG paints
    /// nothing at stroke-width 0, and the old `.max(0.001)` fallback
    /// pen drew stray hairlines Visio never shows.
    #[test]
    fn explicit_zero_line_weight_paints_nothing() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.set_cell("LineWeight", Cell::value("0"));
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Geometry".into(),
            ix: 0,
            rows: vec![
                w3_row(1, "MoveTo", &[("X", "0"), ("Y", "0")]),
                w3_row(2, "LineTo", &[("X", "1"), ("Y", "1")]),
            ],
            ..Section::default()
        }));
        let chain = [&s];
        let mut out = String::new();
        render_shape(&doc, &s, 11.0, &chain, "", None, None, None, None, (0.0, 0.0), &mut out);
        assert!(
            out.contains(r#"stroke-width="0.00000""#),
            "an explicit zero stays zero: {out}"
        );
        assert!(
            !out.contains(r#"stroke-width="0.001"#),
            "no fallback pen: {out}"
        );
    }

    /// #228: every fill-eligible Geometry section of one shape joins
    /// ONE alternate-fill path (`fill-rule="evenodd"`) under the
    /// per-section strokes — nesting and overlap toggle coverage
    /// regardless of winding direction (MisterDiagram M5.109: the
    /// two rects here wind in OPPOSITE directions, which nonzero
    /// fill would paint solid). NoFill opts a section out of the
    /// fill only; NoLine leaves the fill in and drops the stroke.
    #[test]
    fn sections_fill_one_even_odd_path() {
        let doc = Document::default();
        let mut s = Shape::default();
        // Outer rect, clockwise.
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Geometry".into(),
            ix: 0,
            rows: vec![
                w3_row(1, "MoveTo", &[("X", "0"), ("Y", "0")]),
                w3_row(2, "LineTo", &[("X", "2"), ("Y", "0")]),
                w3_row(3, "LineTo", &[("X", "2"), ("Y", "2")]),
                w3_row(4, "LineTo", &[("X", "0"), ("Y", "2")]),
                w3_row(5, "LineTo", &[("X", "0"), ("Y", "0")]),
            ],
            ..Section::default()
        }));
        // Inner rect, COUNTER-clockwise — same coverage under
        // even-odd, solid under nonzero.
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Geometry".into(),
            ix: 1,
            rows: vec![
                w3_row(1, "MoveTo", &[("X", "0.5"), ("Y", "0.5")]),
                w3_row(2, "LineTo", &[("X", "0.5"), ("Y", "1.5")]),
                w3_row(3, "LineTo", &[("X", "1.5"), ("Y", "1.5")]),
                w3_row(4, "LineTo", &[("X", "1.5"), ("Y", "0.5")]),
                w3_row(5, "LineTo", &[("X", "0.5"), ("Y", "0.5")]),
            ],
            ..Section::default()
        }));
        // A NoFill section: stroke only, excluded from the union.
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Geometry".into(),
            ix: 2,
            cells: vec![("NoFill".to_string(), Cell::value("1"))],
            rows: vec![
                w3_row(1, "MoveTo", &[("X", "3"), ("Y", "0")]),
                w3_row(2, "LineTo", &[("X", "3"), ("Y", "1")]),
            ],
            ..Section::default()
        }));
        // A NoLine section: fill only — joins the union, no stroke.
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Geometry".into(),
            ix: 3,
            cells: vec![("NoLine".to_string(), Cell::value("1"))],
            rows: vec![
                w3_row(1, "MoveTo", &[("X", "4"), ("Y", "0")]),
                w3_row(2, "LineTo", &[("X", "4"), ("Y", "1")]),
            ],
            ..Section::default()
        }));
        let chain = [&s];
        let mut out = String::new();
        render_shape(&doc, &s, 11.0, &chain, "", None, None, None, None, (0.0, 0.0), &mut out);
        // ONE even-odd fill element, first among the paths (under the
        // ink), carrying exactly the three fill-eligible subpaths.
        assert_eq!(
            out.matches("fill-rule=\"evenodd\"").count(),
            1,
            "one alternate fill per shape: {out}"
        );
        let fill_elem = out
            .split("fill-rule=\"evenodd\"")
            .nth(1)
            .expect("fill element present");
        let fill_elem = &fill_elem[..fill_elem.find("/>").unwrap_or(fill_elem.len())];
        assert_eq!(
            fill_elem.matches("M ").count(),
            3,
            "the two rects (either winding) and the NoLine section join; NoFill stays out: {fill_elem}"
        );
        // The strokes stay per-section: outer, inner, NoFill open
        // line — three stroke paths after the fill element.
        let after_fill = out.split("fill-rule=\"evenodd\"").nth(1).unwrap_or("");
        assert_eq!(
            after_fill.matches("<path").count(),
            3,
            "three per-section strokes follow the fill: {after_fill}"
        );
        assert!(
            after_fill.contains(r#"fill="none""#),
            "stroke paths carry no fill: {after_fill}"
        );
    }

    /// #73: Char.Color accepts both spellings with fill/line-parity
    /// semantics — a `#rrggbb` hex string verbatim, a legacy palette
    /// index through the [MS-VSDX] table — so a palette write and a
    /// legacy file render identically.
    #[test]
    fn char_color_accepts_hex_and_palette_index() {
        let doc = Document::default();
        let emit = |color: &str| {
            let mut s = Shape::default();
            s.sheet.text = Some("colored".into());
            s.sheet.set_cell("Width", Cell::value("2.0"));
            s.sheet.set_cell("TxtWidth", Cell::value("2.0"));
            s.sheet.children.push(SheetChild::Section(Section {
                name: "Character".into(),
                rows: vec![Row {
                    cells: vec![("Color".to_string(), Cell::value(color))],
                    ..Row::default()
                }],
                ..Section::default()
            }));
            let chain = [&s];
            let mut out = String::new();
            emit_text(
                &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
            );
            out
        };
        let hex = emit("#ED7D31");
        assert!(
            hex.contains(r##"fill="#ED7D31""##),
            "hex Char.Color renders verbatim: {hex}"
        );
        // Palette index 4 is #0000FF in the [MS-VSDX] table; 2 is
        // #FF0000 — the legacy spelling resolves through the same
        // mapping as the fill/line path.
        assert!(
            emit("4").contains(r##"fill="#0000FF""##),
            "an index Char.Color resolves through the palette table"
        );
        assert!(emit("2").contains(r##"fill="#FF0000""##), "index 2 is red");
        // No Color cell: the built-in default is black.
        let mut s = Shape::default();
        s.sheet.text = Some("plain".into());
        s.sheet.set_cell("Width", Cell::value("2.0"));
        s.sheet.set_cell("TxtWidth", Cell::value("2.0"));
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        assert!(
            out.contains(r##"fill="#000000""##),
            "default text is black: {out}"
        );
    }

    #[test]
    fn font_family_chains_follow_the_alias_table() {
        // Document family first, then the metric-compatible substitute,
        // then Carlito, then the CSS generic (GitHub #17's example).
        assert_eq!(
            font_family_attr(Some("Arial")),
            " font-family=\"Arial, 'Liberation Sans', Carlito, sans-serif\""
        );
        assert_eq!(
            font_family_attr(Some("Calibri")),
            " font-family=\"Calibri, Carlito, sans-serif\"",
            "Calibri's substitute IS Carlito — no duplicate entry"
        );
        assert_eq!(
            font_family_attr(Some("Times New Roman")),
            " font-family=\"'Times New Roman', 'Liberation Serif', Carlito, serif\""
        );
        assert_eq!(
            font_family_attr(Some("Courier New")),
            " font-family=\"'Courier New', 'Liberation Mono', Carlito, monospace\""
        );
        assert_eq!(
            font_family_attr(Some("Cambria")),
            " font-family=\"Cambria, Caladea, Carlito, serif\""
        );
        // Unknown families: kept as-is, Carlito + sans-serif tail.
        assert_eq!(
            font_family_attr(Some("Wingdings")),
            " font-family=\"Wingdings, Carlito, sans-serif\""
        );
        // Lookup is case-insensitive and trims; no Font cell (or an
        // empty one) falls back to Visio's default family, Calibri.
        assert_eq!(
            font_family_attr(Some("  ARIAL ")),
            " font-family=\"ARIAL, 'Liberation Sans', Carlito, sans-serif\""
        );
        assert_eq!(
            font_family_attr(None),
            " font-family=\"Calibri, Carlito, sans-serif\"",
            "Visio's default family is Calibri"
        );
        assert_eq!(
            font_family_attr(Some("")),
            " font-family=\"Calibri, Carlito, sans-serif\""
        );
    }

    #[test]
    fn char_font_reads_the_first_character_row() {
        let doc = Document::default();
        let s = shape_with_character(Some("Arial"));
        let chain = [&s];
        assert_eq!(char_font(&doc, &s, &chain).as_deref(), Some("Arial"));
        // Numeric (legacy binary index) values name no family.
        let s = shape_with_character(Some("4"));
        let chain = [&s];
        assert_eq!(char_font(&doc, &s, &chain), None);
        // No Font cell / no section at all.
        let s = shape_with_character(None);
        let chain = [&s];
        assert_eq!(char_font(&doc, &s, &chain), None);
        let s = Shape::default();
        let chain = [&s];
        assert_eq!(char_font(&doc, &s, &chain), None);
    }

    #[test]
    fn char_font_inherits_from_the_master_sheet() {
        // The instance's own sheet carries no sections, so the
        // effective sheet is its master's (the Size/Color path).
        let mut root = shape_with_character(Some("Segoe UI"));
        root.id = 1;
        let doc = Document {
            masters: vec![vsdx::Master {
                id: 7,
                shapes: vec![root],
                ..Default::default()
            }],
            ..Document::default()
        };
        let inst = Shape {
            id: 2,
            master: Some(7),
            ..Shape::default()
        };
        assert_eq!(char_font(&doc, &inst, &[]).as_deref(), Some("Segoe UI"));
        // An own Character row wins over the master's.
        let mut inst = Shape {
            id: 3,
            master: Some(7),
            ..Shape::default()
        };
        inst.sheet.children.push(SheetChild::Section(Section {
            name: "Character".into(),
            rows: vec![char_row(Some("Cambria"))],
            ..Section::default()
        }));
        assert_eq!(char_font(&doc, &inst, &[]).as_deref(), Some("Cambria"));
    }

    #[test]
    fn emitted_text_carries_the_font_family_chain() {
        let doc = Document::default();
        let mut s = shape_with_character(Some("Arial"));
        s.sheet.text = Some("hello".into());
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        assert!(
            out.contains(r#"font-family="Arial, 'Liberation Sans', Carlito, sans-serif""#),
            "emitted text element: {out}"
        );
        assert!(out.contains("hello"), "emitted: {out}");
    }

    /// #124 item 2: a text block whose on-screen em height falls under
    /// TEXT_GREEK_PX emits gray bars (no glyph nodes for usvg to turn
    /// into outlines) and records its size in the greek profile; above
    /// the threshold — or with no ppi at all (export/print/thumbnail
    /// paths pass None) — glyphs emit exactly as before.
    #[test]
    fn text_greeks_below_the_pixel_threshold() {
        let doc = Document::default();
        let tiny = |size: &str| {
            // sizes floor at 0.02 in in resolve_text_block
            let mut s = Shape::default();
            s.sheet.children.push(SheetChild::Section(Section {
                name: "Character".into(),
                rows: vec![Row {
                    cells: vec![("Size".to_string(), Cell::value(size))],
                    ..Row::default()
                }],
                ..Section::default()
            }));
            s.sheet.text = Some("tiny text".into());
            s
        };
        // 0.02 in em (the resolver floor) at 96 ppi = 1.92 px: greeked to bars.
        let s = tiny("0.02");
        let chain = [&s];
        let ctx = GreekCtx::new(96.0);
        let mut out = String::new();
        emit_text(
            &doc,
            &s,
            &chain,
            2.0,
            1.0,
            1.0,
            1.0,
            false,
            Some(&ctx),
            None,
            &mut out,
        );
        assert!(
            out.contains(r##"fill="#7f7f7f""##),
            "greek bars emitted: {out}"
        );
        assert!(!out.contains("<text"), "no glyph node when greeked: {out}");
        assert!(!out.contains("<tspan"), "no tspan when greeked: {out}");
        assert_eq!(ctx.sizes.into_inner(), vec![0.02]);
        assert_eq!(greeked_count(&[0.02], 96.0), 1);

        // 0.13889 in em (the 10 pt default) at 96 ppi ≈ 13.3 px: glyphs.
        let s = tiny("0.13889");
        let chain = [&s];
        let ctx = GreekCtx::new(96.0);
        let mut out = String::new();
        emit_text(
            &doc,
            &s,
            &chain,
            2.0,
            1.0,
            1.0,
            1.0,
            false,
            Some(&ctx),
            None,
            &mut out,
        );
        assert!(out.contains("tiny text"), "glyph path unchanged: {out}");
        assert!(
            !out.contains("#7f7f7f"),
            "no bars above the threshold: {out}"
        );
        assert_eq!(
            greeked_count(&[0.13889], 96.0),
            0,
            "13 px text does not greek"
        );

        // No greek context (export/print/thumbnails): glyphs regardless
        // of size — greeking never enters those paths.
        let s = tiny("0.02");
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        assert!(out.contains("tiny text"), "export never greeks: {out}");
    }

    /// Compose an SVG transform list (`translate(a,b) scale(c,d)
    /// rotate(e) …`) into one matrix. SVG applies leftmost-last to the
    /// point, so composition is left-to-right matrix multiplication.
    fn compose_tf(list: &str) -> Mat {
        let mut total = Mat::identity();
        for cmd in list.split(')').filter(|s| !s.trim().is_empty()) {
            let body = cmd.trim().split('(').nth(1).unwrap_or("");
            let n: Vec<f64> = body.split(',').map(|v| v.trim().parse().unwrap()).collect();
            let m = if cmd.trim().starts_with("translate") {
                Mat::translate(n[0], n[1])
            } else if cmd.trim().starts_with("scale") {
                Mat::scale(n[0], n[1])
            } else if cmd.trim().starts_with("rotate") {
                Mat::rotation(n[0].to_radians())
            } else {
                panic!("unhandled transform command: {cmd}");
            };
            total = total.mul(&m);
        }
        total
    }

    /// A text block pinned off-origin, for the TxtPin-pivot tests.
    fn shape_with_pinned_text(txt_angle_deg: f64) -> Shape {
        let mut s = Shape::default();
        let set = |s: &mut Shape, n: &str, v: f64| {
            s.sheet.set_cell(
                n,
                Cell {
                    v: Some(format!("{v}")),
                    ..Cell::default()
                },
            );
        };
        set(&mut s, "TxtPinX", 1.5);
        set(&mut s, "TxtPinY", 0.75);
        set(&mut s, "TxtWidth", 1.0);
        set(&mut s, "TxtHeight", 0.5);
        set(&mut s, "TxtAngle", txt_angle_deg.to_radians());
        s.sheet.text = Some("label".into());
        s
    }

    fn text_wrapper_transform(
        sx: f64,
        sy: f64,
        txt_angle_deg: f64,
    ) -> Option<(String, (f64, f64))> {
        let doc = Document::default();
        let s = shape_with_pinned_text(txt_angle_deg);
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, sx, sy, false, None, None, &mut out,
        );
        let t = out
            .split("transform=\"")
            .nth(1)?
            .split('"')
            .next()
            .map(str::to_owned)?;
        Some((t, (1.5, -0.75))) // (list, TxtPin in the emitted y-down frame)
    }

    #[test]
    fn rotated_text_block_pivots_at_txtpin() {
        let Some((t, pin)) = text_wrapper_transform(1.0, 1.0, 30.0) else {
            panic!("rotation must emit a transform wrapper");
        };
        let m = compose_tf(&t);
        let [a, b, c, d, e, f] = m.0;
        let (x, y) = (pin.0 * a + pin.1 * c + e, pin.0 * b + pin.1 * d + f);
        assert!(
            (x - pin.0).abs() < 1e-6 && (y - pin.1).abs() < 1e-6,
            "anchor ({x},{y}) must stay at the TxtPin {pin:?} — transform {t}"
        );
    }

    #[test]
    fn flipped_text_block_pivots_at_txtpin() {
        // FlipX only and both axes: the counter-scale keeps the anchor
        // fixed at the pin in each case.
        for (sx, sy) in [(-1.0, 1.0), (-1.0, -1.0)] {
            let Some((t, pin)) = text_wrapper_transform(sx, sy, 0.0) else {
                panic!("flip must emit a transform wrapper");
            };
            let m = compose_tf(&t);
            let [a, b, c, d, e, f] = m.0;
            let (x, y) = (pin.0 * a + pin.1 * c + e, pin.0 * b + pin.1 * d + f);
            assert!(
                (x - pin.0).abs() < 1e-6 && (y - pin.1).abs() < 1e-6,
                "anchor ({x},{y}) must stay at the TxtPin {pin:?} — transform {t}"
            );
        }
    }

    #[test]
    fn unflipped_unrotated_text_has_no_wrapper() {
        assert!(text_wrapper_transform(1.0, 1.0, 0.0).is_none());
    }

    /// D is the ratio of the axis along C to the axis across it
    /// (Visio 2021 witness t14, Greptile on PR #330): through
    /// (0,.5)-(1,1)-(2,.5) in a 2×1 box with C = 0, D = 2 draws the
    /// flat half-ellipse (along 1, across 0.5), D = 0.5 a tall one.
    /// The ratio-1 solver tests alone would not catch an inverted D.
    #[test]
    fn elliptical_arc_d_is_along_over_across() {
        let arc = |d: f64| {
            let mut s = shape_at(1, (2.0, 2.0), 2.0, 1.0, 0.0);
            push_geo(
                &mut s,
                vec![
                    geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.5)]),
                    geo_row(
                        2,
                        "EllipticalArcTo",
                        &[
                            ("X", 2.0),
                            ("Y", 0.5),
                            ("A", 1.0),
                            ("B", 1.0),
                            ("C", 0.0),
                            ("D", d),
                        ],
                    ),
                ],
            );
            let (doc, page) = page_fixture(vec![s]);
            let svg = render_page(&doc, &page).unwrap();
            let i = svg.find(" A ").unwrap_or_else(|| panic!("no arc: {svg}"));
            let v: Vec<f64> = svg[i + 3..]
                .split_whitespace()
                .take(3)
                .map(|t| t.parse().unwrap())
                .collect();
            // (along C, across C): rx/ry, swapped when the solver
            // reports the axes rotated by 90°.
            let rot = v[2].rem_euclid(180.0);
            if (rot - 90.0).abs() < 1e-3 {
                (v[1], v[0])
            } else {
                assert!(rot.abs() < 1e-3 || (rot - 180.0).abs() < 1e-3, "rot {rot}");
                (v[0], v[1])
            }
        };
        let (along, across) = arc(2.0);
        assert!(
            (along - 1.0).abs() < 1e-4 && (across - 0.5).abs() < 1e-4,
            "D=2: the flat half-ellipse ({along}, {across})"
        );
        let (along, across) = arc(0.5);
        assert!(
            (along / across - 0.5).abs() < 1e-4,
            "D=0.5: along/across = 0.5 ({along}, {across})"
        );
    }

    #[test]
    fn elliptical_arc_half_circle() {
        // Upper half of the unit circle: start (-1,0), on-arc top (0,1),
        // end (1,0), axis angle 0, ratio 1 → rx = ry = 1, minor arc,
        // CLOCKWISE in the y-up frame (over the top, left → right),
        // which stays clockwise on the y-down page: sweep 1 (Visio 2021
        // witness t14 draws this arc above its chord).
        let (rx, ry, rot, large, sweep) =
            solve_ellipse_arc((-1.0, 0.0), (0.0, 1.0), (1.0, 0.0), 0.0, 1.0, (1.0, 1.0)).unwrap();
        assert!((rx - 1.0).abs() < 1e-9);
        assert!((ry - 1.0).abs() < 1e-9);
        assert!(rot.abs() < 1e-9);
        assert!(!large);
        assert_eq!(sweep, 1);
    }

    /// The junction-circle specimen from Analysis/06: MoveTo(0, W/2) then
    /// two EllipticalArcTo rows (W = 0.0393700787…in) that must reconstruct
    /// one shared circle of radius W/2 as two exact half-arcs.
    #[test]
    fn elliptical_arc_junction_circle() {
        let w = 0.03937007874015748;
        // Row 2: arc from (0, W/2) via on-arc point (W/2, W) to (W, W/2).
        let (rx, ry, _, large, sweep) = solve_ellipse_arc(
            (0.0, w / 2.0),
            (w / 2.0, w),
            (w, w / 2.0),
            0.0,
            1.0,
            (1.0, 1.0),
        )
        .unwrap();
        assert!(
            (rx - ry).abs() < 1e-9,
            "ratio 1 ⇒ circle, got rx={rx} ry={ry}"
        );
        assert!((rx - w / 2.0).abs() < 1e-9, "rx={rx} expected={}", w / 2.0);
        assert!(!large, "exact half arc");
        assert_eq!(sweep, 1, "over the top, left → right: clockwise");
        // Row 3 closes the circle back to (0, W/2) via (W/2, 0).
        let (rx2, ry2, _, large2, sweep2) = solve_ellipse_arc(
            (w, w / 2.0),
            (w / 2.0, 0.0),
            (0.0, w / 2.0),
            0.0,
            1.0,
            (1.0, 1.0),
        )
        .unwrap();
        assert!((rx2 - w / 2.0).abs() < 1e-9);
        assert!((ry2 - w / 2.0).abs() < 1e-9);
        assert!(!large2);
        assert_eq!(sweep2, 1, "under the bottom, right → left: clockwise");
    }

    #[test]
    fn elliptical_arc_scaled_circle_becomes_ellipse() {
        // A unit circle scaled ×2 horizontally is an ellipse with rx=2.
        let (rx, ry, _, _, _) =
            solve_ellipse_arc((-1.0, 0.0), (0.0, 1.0), (1.0, 0.0), 0.0, 1.0, (2.0, 1.0)).unwrap();
        assert!((rx - 2.0).abs() < 1e-9, "rx={rx}");
        assert!((ry - 1.0).abs() < 1e-9, "ry={ry}");
    }

    #[test]
    fn transform_ellipse_identity_and_rotation() {
        let (r1, r2, a) = transform_ellipse(2.0, 0.5, 0.3, (1.0, 1.0));
        assert!((r1 - 2.0).abs() < 1e-9 && (r2 - 1.0).abs() < 1e-9);
        assert!((a - 0.3).abs() < 1e-9);

        // Rotating then anisotropically scaling a circle: a circle has no
        // preferred axis, so scaling (2,1) gives rx=2, ry=1 regardless of
        // the incoming axis angle.
        let (r1, r2, a) = transform_ellipse(1.0, 1.0, std::f64::consts::FRAC_PI_2, (2.0, 1.0));
        assert!((r1 - 2.0).abs() < 1e-9 && (r2 - 1.0).abs() < 1e-9);
        assert!(a.abs() < 1e-9);
    }

    /// #12 specimen class: a container-heading shape whose master
    /// Geometry section is gated by a section-level `NoShow` cell, and a
    /// page instance carrying only sparse row caches (no section cells).
    /// The merge must inherit the flag, and the formula must re-evaluate
    /// per instance so a restyled instance shows its band again even
    /// against a stale master cache.
    #[test]
    fn geometry_noshow_inherits_and_formula_reevaluates() {
        let urow = |n: &str, v: f64| Row {
            n: Some(n.into()),
            cells: vec![(
                "Value".into(),
                Cell {
                    v: Some(format!("{v}")),
                    ..Cell::default()
                },
            )],
            ..Row::default()
        };
        let grow = |ix: i64, t: &str, x: f64, y: f64| Row {
            ix,
            t: Some(t.into()),
            cells: vec![
                ("X".into(), Cell::value(format!("{x}"))),
                ("Y".into(), Cell::value(format!("{y}"))),
            ],
            ..Row::default()
        };
        // Heading master: band rect 2.0 × 0.5 with a style-gated NoShow.
        let heading_master = |cstyle: f64, noshow_f: Option<&str>| -> vsdx::Master {
            let mut sh = Shape {
                id: 8,
                ..Shape::default()
            };
            let set = |sheet: &mut Sheet, n: &str, v: f64| {
                sheet.set_cell(n, Cell::value(format!("{v}")));
            };
            set(&mut sh.sheet, "Width", 2.0);
            set(&mut sh.sheet, "Height", 0.5);
            sh.sheet.children.push(SheetChild::Section(Section {
                name: "User".into(),
                rows: vec![urow("hidden", 0.0), urow("CStyle", cstyle)],
                ..Section::default()
            }));
            sh.sheet.children.push(SheetChild::Section(Section {
                name: "Geometry".into(),
                ix: 0,
                cells: vec![
                    ("NoFill".into(), Cell::value("0".to_string())),
                    ("NoLine".into(), Cell::value("0".to_string())),
                    (
                        "NoShow".into(),
                        Cell {
                            v: Some("1".into()),
                            f: noshow_f.map(str::to_string),
                            ..Cell::default()
                        },
                    ),
                ],
                rows: vec![
                    grow(1, "MoveTo", 0.0, 0.0),
                    grow(2, "LineTo", 2.0, 0.0),
                    grow(3, "LineTo", 2.0, 0.5),
                    grow(4, "LineTo", 0.0, 0.5),
                    grow(5, "LineTo", 0.0, 0.0),
                ],
                ..Section::default()
            }));
            vsdx::Master {
                id: 42,
                shapes: vec![sh],
                ..Default::default()
            }
        };
        // Sparse instance: only two row caches, no section-level cells —
        // exactly how Visio writes inline copies of master geometry.
        let inst = Shape {
            id: 1228,
            master: Some(42),
            master_shape: Some(8),
            ..Shape::default()
        };
        let mut inst = inst;
        inst.sheet.set_cell("Width", Cell::value("4.0"));
        inst.sheet.set_cell("Height", Cell::value("0.5"));
        inst.sheet.children.push(SheetChild::Section(Section {
            name: "Geometry".into(),
            ix: 0,
            rows: vec![grow(1, "MoveTo", 0.0, 0.0), grow(2, "LineTo", 4.0, 0.0)],
            ..Section::default()
        }));

        // Plain style (CStyle=1): the formula hides the band — the
        // utilisation.vsdx heading case.
        let doc = Document {
            masters: vec![heading_master(1.0, Some("OR(User.hidden,User.CStyle<>4)"))],
            ..Document::default()
        };
        let mut out = String::new();
        render_all_geometry(&doc, &inst, &[], 4.0, 0.5, (0.0, 0.0), &mut out);
        assert!(
            !out.contains("<path"),
            "NoShow'd section must not draw: {out}"
        );

        // Restyled (CStyle=4): the formula re-evaluates to visible even
        // though the master's cached NoShow V=1 says hidden.
        let doc = Document {
            masters: vec![heading_master(4.0, Some("OR(User.hidden,User.CStyle<>4)"))],
            ..Document::default()
        };
        let mut out = String::new();
        render_all_geometry(&doc, &inst, &[], 4.0, 0.5, (0.0, 0.0), &mut out);
        assert!(
            out.contains("<path"),
            "formula must beat the stale cache: {out}"
        );
        assert!(
            out.contains("4.00000"),
            "row merge must scale master X into instance units: {out}"
        );

        // Cache-only flag (no formula): the cached V=1 still hides.
        let doc = Document {
            masters: vec![heading_master(4.0, None)],
            ..Document::default()
        };
        let mut out = String::new();
        render_all_geometry(&doc, &inst, &[], 4.0, 0.5, (0.0, 0.0), &mut out);
        assert!(!out.contains("<path"), "cached NoShow must hide: {out}");
    }

    // ------------------------------------------------------------------
    // Group XForm composition (GitHub #78): the affine helpers must
    // reproduce the renderer's nested transform math exactly.

    fn xform_shape(cells: &[(&str, f64)]) -> Shape {
        let mut s = Shape::default();
        for (n, v) in cells {
            s.sheet.set_cell(
                n,
                Cell {
                    v: Some(format!("{v}")),
                    ..Cell::default()
                },
            );
        }
        s
    }

    /// A member at parent-local Pin (1,1), 1×1: parent-local rect
    /// (0.5, 0.5, 1, 1).
    fn unit_member() -> Shape {
        let mut m = xform_shape(&[]);
        m.id = 10;
        m
    }

    #[test]
    fn member_page_rect_unrotated_group() {
        // Group 2×4 at Pin (2,3), LocPin centered by default (W=4,H=2
        // → LocPin (2,1)), on an 11-inch page. A member rect of
        // (0.5,0.5,1,1) in group-local y-up maps to the same numbers
        // in page space (translate-only: group-local (0..4, 0..2) ↔
        // page (0..4, 2..4)).
        let doc = Document::default();
        let mut g = xform_shape(&[
            ("PinX", 2.0),
            ("PinY", 3.0),
            ("Width", 4.0),
            ("Height", 2.0),
        ]);
        g.id = 9;
        g.type_ = Some("Group".into());
        let mut m = unit_member();
        for (n, v) in [
            ("PinX", 1.0),
            ("PinY", 1.0),
            ("Width", 1.0),
            ("Height", 1.0),
        ] {
            m.sheet.set_cell(
                n,
                Cell {
                    v: Some(v.to_string()),
                    ..Cell::default()
                },
            );
        }
        let chain = vec![&g, &m];
        assert_eq!(
            shape_page_rect(&doc, &chain, 11.0),
            Some((0.5, 2.5, 1.0, 1.0))
        );
    }

    #[test]
    fn member_page_rect_rotated_group() {
        // The group rotated +90° (page CCW): a member at group-local
        // (1,1) has offset (−1,0) from the LocPin (2,1); CCW rotation
        // maps that to (0,−1), so the member center lands at page
        // Pin(4,5)+(0,−1) = (4,4) and its axis-aligned unit rect is
        // (3.5, 3.5, 1, 1).
        let doc = Document::default();
        let mut g = xform_shape(&[
            ("PinX", 4.0),
            ("PinY", 5.0),
            ("Width", 4.0),
            ("Height", 2.0),
            ("Angle", std::f64::consts::FRAC_PI_2),
        ]);
        g.id = 9;
        g.type_ = Some("Group".into());
        let mut m = unit_member();
        for (n, v) in [
            ("PinX", 1.0),
            ("PinY", 1.0),
            ("Width", 1.0),
            ("Height", 1.0),
        ] {
            m.sheet.set_cell(
                n,
                Cell {
                    v: Some(v.to_string()),
                    ..Cell::default()
                },
            );
        }
        let chain = vec![&g, &m];
        assert_eq!(
            shape_page_rect(&doc, &chain, 11.0),
            Some((3.5, 3.5, 1.0, 1.0))
        );
    }

    #[test]
    fn page_point_round_trips_through_parent_local() {
        let doc = Document::default();
        let mut g = xform_shape(&[
            ("PinX", 4.0),
            ("PinY", 5.0),
            ("Width", 4.0),
            ("Height", 2.0),
            ("Angle", std::f64::consts::FRAC_PI_2),
        ]);
        g.id = 9;
        g.type_ = Some("Group".into());
        let m = unit_member();
        let chain = vec![&g, &m];
        for p in [(5.0, 4.0), (0.0, 0.0), (7.5, 9.25)] {
            let local = page_point_to_parent_local(&doc, &chain, 11.0, p);
            let (sx, sy) = ancestors_affine(&doc, &chain, 11.0).apply(local.0, -local.1);
            assert!((sx - p.0).abs() < 1e-9 && (11.0 - sy - p.1).abs() < 1e-9);
            // And back through the forward helper.
            let back = parent_local_point_to_page(&doc, &chain, 11.0, local);
            assert!((back.0 - p.0).abs() < 1e-9 && (back.1 - p.1).abs() < 1e-9);
        }
        // A top-level chain (no ancestors) is the identity in page
        // space: the parent frame IS the page.
        let top = vec![&g];
        let q = (3.25, 8.5);
        assert_eq!(parent_local_point_to_page(&doc, &top, 11.0, q), q);
    }

    #[test]
    fn top_level_rect_is_identity_composition() {
        // A top-level shape's parent frame is the page: the affine is
        // identity and shape_page_rect matches the plain local rect.
        let doc = Document::default();
        let s = xform_shape(&[
            ("PinX", 3.0),
            ("PinY", 4.0),
            ("Width", 2.0),
            ("Height", 1.0),
        ]);
        let chain = vec![&s];
        assert_eq!(
            shape_page_rect(&doc, &chain, 11.0),
            Some((2.0, 3.5, 2.0, 1.0))
        );
    }

    // ---- #93: per-shape layer bounds + renderer ----

    /// A minimal page: PageWidth/Height sheet plus `shapes`.
    /// #187: the export draws line jumps — a crossing fixture's page
    /// SVG carries the jump group with the white gap, the under
    /// redraw, and an arc that bulges page-up (smaller SVG y).
    #[test]
    fn export_renders_line_jumps() {
        let connector = |id: i64, pin: (f64, f64), w: f64, h: f64| {
            let mut s = shape_at(id, pin, w, h, 0.0);
            s.sheet.set_cell("ObjType", Cell::value("2"));
            let line = |ix: i64, x: f64, y: f64| Row {
                t: Some("LineTo".into()),
                ix,
                cells: vec![
                    ("X".to_string(), Cell::value(format!("{x}"))),
                    ("Y".to_string(), Cell::value(format!("{y}"))),
                ],
                ..Row::default()
            };
            let mut sec = Section {
                name: "Geometry".into(),
                ix: 0,
                ..Section::default()
            };
            sec.rows = vec![
                Row {
                    t: Some("MoveTo".into()),
                    ix: 1,
                    cells: vec![
                        ("X".to_string(), Cell::value("0")),
                        ("Y".to_string(), Cell::value("0")),
                    ],
                    ..Row::default()
                },
                line(2, w, h),
            ];
            s.sheet.children.push(SheetChild::Section(sec));
            s
        };
        // A horizontal connector crossed by a vertical one at (3, 5)
        // strictly inside both segments.
        let (doc, page) = page_fixture(vec![
            connector(1, (3.0, 5.0), 4.0, 0.0),
            connector(2, (3.0, 5.0), 0.0, 4.0),
        ]);
        let svg = render_page(&doc, &page).unwrap();
        assert!(
            svg.contains(r#"data-dc="line-jumps""#),
            "the jump overlay group is present"
        );
        assert!(
            svg.contains(r##"stroke="#ffffff""##),
            "the white gap stroke is present"
        );
        // The arc bulges page-UP: the apex sits at a SMALLER SVG y
        // than the crossing's (page y 5 → SVG y 6 on an 11in page).
        // The radius is the page's resolved LineToLineX × factor / 2
        // (#294 — the fixture page sets neither cell, so Visio's
        // defaults apply: 0.125 × 2/3 / 2).
        // Independent expectation — not the renderer's own helper
        // (a tautology would survive a resolver regression, #294
        // review): 0.125 in × 2/3 / 2.
        let apex = format!("{:.5} {:.5}", 3.0, 6.0 - 0.125 * (2.0 / 3.0) / 2.0);
        assert!(svg.contains(&apex), "the arc apex is page-up: {apex}");
    }

    /// #251: the page's Show-Line-Jumps switch (`ConLineJumpCode=1`
    /// on the PageSheet) suppresses the jump overlay entirely; the
    /// `LineJumpFactorX` size multiplier scales the hop arc.
    #[test]
    fn export_gates_line_jumps_on_the_page_setting() {
        let connector = |id: i64, pin: (f64, f64), w: f64, h: f64| {
            let mut s = shape_at(id, pin, w, h, 0.0);
            s.sheet.set_cell("ObjType", Cell::value("2"));
            let line = |ix: i64, x: f64, y: f64| Row {
                t: Some("LineTo".into()),
                ix,
                cells: vec![
                    ("X".to_string(), Cell::value(format!("{x}"))),
                    ("Y".to_string(), Cell::value(format!("{y}"))),
                ],
                ..Row::default()
            };
            let mut sec = Section {
                name: "Geometry".into(),
                ix: 0,
                ..Section::default()
            };
            sec.rows = vec![
                Row {
                    t: Some("MoveTo".into()),
                    ix: 1,
                    cells: vec![
                        ("X".to_string(), Cell::value("0")),
                        ("Y".to_string(), Cell::value("0")),
                    ],
                    ..Row::default()
                },
                line(2, w, h),
            ];
            s.sheet.children.push(SheetChild::Section(sec));
            s
        };
        let (doc, _page) = page_fixture(vec![
            connector(1, (3.0, 5.0), 4.0, 0.0),
            connector(2, (3.0, 5.0), 0.0, 4.0),
        ]);
        // Hidden: the PageSheet switch suppresses the group (0 =
        // visPLOJumpNone, the documented off switch per [MS-Learn];
        // #218 review — 6 was never in the enumeration, and a page
        // set to None must not draw hop arcs).
        for off in ["0", "6"] {
            let mut hidden = doc.clone();
            hidden.pages[0]
                .sheet_mut()
                .unwrap()
                .set_cell("LineJumpCode", Cell::value(off));
            let svg = render_page(&hidden, &hidden.pages[0]).unwrap();
            assert!(
                !svg.contains(r#"data-dc="line-jumps""#),
                "the page set to LineJumpCode {off} draws no jump group"
            );
        }
        // An explicit LineJumpFactorX=0 is a zero-size hop (#294
        // review) — no overlay at all, never degenerate hops.
        let mut zero = doc.clone();
        zero.pages[0]
            .sheet_mut()
            .unwrap()
            .set_cell("LineJumpFactorX", Cell::value("0"));
        let svg = render_page(&zero, &zero.pages[0]).unwrap();
        assert!(
            !svg.contains(r##"data-dc="line-jumps""##),
            "a zero jump factor draws no hop group"
        );
        // Shown at 2×: the radius is LineToLineX × factor / 2 (#294) —
        // the fixture page sets only the factor, so 0.125 × 2 / 2.
        let mut big = doc.clone();
        big.pages[0]
            .sheet_mut()
            .unwrap()
            .set_cell("LineJumpFactorX", Cell::value("2"));
        let svg = render_page(&big, &big.pages[0]).unwrap();
        // 0.125 in × 2 / 2, by hand.
        let apex = format!("{:.5} {:.5}", 3.0, 6.0 - 0.125);
        assert!(svg.contains(&apex), "the 2× arc apex: {apex}");
    }

    /// #187: a page with NO crossings draws no jump group.
    /// #294: a connector's stored jump `ArcTo` straightens — the ink
    /// follows the run (no `A` command) and the overlay draws the ONE
    /// arc at the current crossing with the page's radius (Visio's
    /// defaults here: 0.125 × 2/3 / 2 ≈ 0.04167, not the old 0.12).
    /// A stored arc sitting OFF any crossing simply vanishes.
    #[test]
    fn export_straightens_stored_jump_arcs() {
        let connector =
            |id: i64, pin: (f64, f64), w: f64, h: f64, stored_jump: Option<(f64, f64)>| {
                let mut s = shape_at(id, pin, w, h, 0.0);
                s.sheet.set_cell("ObjType", Cell::value("2"));
                let line = |ix: i64, x: f64, y: f64| Row {
                    t: Some("LineTo".into()),
                    ix,
                    cells: vec![
                        ("X".to_string(), Cell::value(format!("{x}"))),
                        ("Y".to_string(), Cell::value(format!("{y}"))),
                    ],
                    ..Row::default()
                };
                let mut sec = Section {
                    name: "Geometry".into(),
                    ix: 0,
                    ..Section::default()
                };
                let mut rows = vec![Row {
                    t: Some("MoveTo".into()),
                    ix: 1,
                    cells: vec![
                        ("X".to_string(), Cell::value("0")),
                        ("Y".to_string(), Cell::value("0")),
                    ],
                    ..Row::default()
                }];
                // A stored jump mid-run (the #294 repro's row shape):
                // chord along the run, |A| = the jump radius.
                match stored_jump {
                    Some((mid, half)) => {
                        rows.push(line(2, mid - half, 0.0));
                        rows.push(Row {
                            t: Some("ArcTo".into()),
                            ix: 3,
                            cells: vec![
                                ("X".to_string(), Cell::value(format!("{}", mid + half))),
                                ("Y".to_string(), Cell::value("0")),
                                ("A".to_string(), Cell::value(format!("{}", -half))),
                            ],
                            ..Row::default()
                        });
                        rows.push(line(4, w, h));
                    }
                    None => rows.push(line(2, w, h)),
                }
                sec.rows = rows;
                s.sheet.children.push(SheetChild::Section(sec));
                s
            };
        let jump_half = 0.125 * (2.0 / 3.0) / 2.0;
        // Stored jump centered on the crossing (page x = 3): the ink
        // is a straight run, one hop at the crossing.
        let (doc, page) = page_fixture(vec![
            connector(1, (3.0, 5.0), 4.0, 0.0, Some((2.0, jump_half))),
            connector(2, (3.0, 5.0), 0.0, 4.0, None),
        ]);
        let svg = render_page(&doc, &page).unwrap();
        assert!(
            !svg.contains(" A "),
            "the stored jump arc renders as a straight run"
        );
        assert_eq!(
            svg.matches(r##"stroke="#ffffff""##).count(),
            1,
            "one jump at the crossing"
        );
        // The gap spans the crossing ± the page's resolved radius
        // (prefix match: the gap line writes full-precision floats).
        assert!(
            svg.contains(r#"x1="2.95833"#) && svg.contains(r#"x2="3.04166"#),
            "the gap is LineToLineX × factor / 2 wide"
        );
        // The stale case: the stored arc sits at page x = 3 while the
        // crossing moved to x = 4.5 — the arc vanishes, one hop at the
        // CURRENT crossing.
        let (doc, page) = page_fixture(vec![
            connector(1, (3.0, 5.0), 4.0, 0.0, Some((2.0, jump_half))),
            connector(2, (4.5, 5.0), 0.0, 4.0, None),
        ]);
        let svg = render_page(&doc, &page).unwrap();
        assert!(
            !svg.contains(" A "),
            "the stale stored arc does not render"
        );
        assert!(
            svg.contains(r#"x1="4.45833"#) && !svg.contains(r#"x1="2.95833"#),
            "one hop at the current crossing, nothing at the stale arc"
        );
    }

    /// #293: a `Del="1"` geometry row stays deleted — an OWN row
    /// carrying values (what 0.2.15's corrupted save wrote: the
    /// diagonal into row 3) renders nothing, and the phantom jump the
    /// row used to feed is gone (the #294 part-4 prediction).
    #[test]
    fn export_drops_deleted_geometry_rows() {
        // A vertical connector whose deleted row 3 carries the master's
        // hook values (the saved-by-0.2.15 corruption shape), crossed
        // by a horizontal one.
        let mut s = shape_at(5, (3.5, 2.3047), 0.25, 2.2031, 0.0);
        s.sheet.set_cell("ObjType", Cell::value("2"));
        let mut sec = Section {
            name: "Geometry".into(),
            ix: 0,
            ..Section::default()
        };
        sec.rows = vec![
            Row {
                t: Some("MoveTo".into()),
                ix: 1,
                cells: vec![("X".to_string(), Cell::value("0.125"))],
                ..Row::default()
            },
            Row {
                t: Some("LineTo".into()),
                ix: 2,
                cells: vec![
                    ("X".to_string(), Cell::value("0.125")),
                    ("Y".to_string(), Cell::value("2.2031")),
                ],
                ..Row::default()
            },
            Row {
                t: Some("LineTo".into()),
                ix: 3,
                attrs: vec![("Del".to_string(), "1".to_string())],
                cells: vec![
                    ("X".to_string(), Cell::value("1")),
                    ("Y".to_string(), Cell::value("-1")),
                ],
                ..Row::default()
            },
        ];
        s.sheet.children.push(SheetChild::Section(sec));
        // The A→B twin: a horizontal connector at page y = 2.297 that
        // BOTH the vertical run (x = 3.5) and the deleted hook (its
        // page chord runs (3.5, 3.4)→(4.375, 0.2)) would cross.
        let mut ab = shape_at(6, (3.0, 2.2969), 3.0, 0.0, 0.0);
        ab.sheet.set_cell("ObjType", Cell::value("2"));
        let mut asec = Section {
            name: "Geometry".into(),
            ix: 0,
            ..Section::default()
        };
        asec.rows = vec![
            Row {
                t: Some("MoveTo".into()),
                ix: 1,
                cells: vec![("X".to_string(), Cell::value("0"))],
                ..Row::default()
            },
            Row {
                t: Some("LineTo".into()),
                ix: 2,
                cells: vec![("X".to_string(), Cell::value("3"))],
                ..Row::default()
            },
        ];
        ab.sheet.children.push(SheetChild::Section(asec));
        let (doc, page) = page_fixture(vec![ab, s]);
        let svg = render_page(&doc, &page).unwrap();
        // The diagonal hook (local (0.125,2.2)→(1,−1) → page
        // (3.5,3.4)→(4.375,0.2)) draws nothing.
        assert!(
            !svg.contains("4.37500"),
            "the deleted row's diagonal is not drawn"
        );
        // The crossing hops once — the phantom jump the deleted row
        // fed is gone.
        assert_eq!(
            svg.matches(r##"stroke="#ffffff""##).count(),
            1,
            "one jump at the real crossing"
        );
    }

    /// #294 review: an AUTHORED shallow ArcTo on a connector is a
    /// curve — the semicircle band leaves it alone, and with no
    /// crossing nothing replaces it.
    #[test]
    fn export_preserves_authored_shallow_connector_arcs() {
        let mut s = shape_at(1, (3.0, 5.0), 4.0, 0.0, 0.0);
        s.sheet.set_cell("ObjType", Cell::value("2"));
        let mut sec = Section {
            name: "Geometry".into(),
            ix: 0,
            ..Section::default()
        };
        sec.rows = vec![
            Row {
                t: Some("MoveTo".into()),
                ix: 1,
                cells: vec![
                    ("X".to_string(), Cell::value("0")),
                    ("Y".to_string(), Cell::value("0")),
                ],
                ..Row::default()
            },
            Row {
                t: Some("ArcTo".into()),
                ix: 2,
                cells: vec![
                    ("X".to_string(), Cell::value("4")),
                    ("Y".to_string(), Cell::value("0")),
                    ("A".to_string(), Cell::value("0.25")),
                ],
                ..Row::default()
            },
        ];
        s.sheet.children.push(SheetChild::Section(sec));
        let (doc, page) = page_fixture(vec![s]);
        let svg = render_page(&doc, &page).unwrap();
        assert!(svg.contains(" A "), "the shallow arc renders as an arc");
        assert!(
            !svg.contains(r##"data-dc="line-jumps""##),
            "no crossing, no overlay"
        );
    }

    /// #187: a page with NO crossings draws no jump group.
    #[test]
    fn export_without_crossings_has_no_jump_group() {
        let connector = |id: i64, pin: (f64, f64), w: f64, h: f64| {
            let mut s = shape_at(id, pin, w, h, 0.0);
            s.sheet.set_cell("ObjType", Cell::value("2"));
            let line = |ix: i64, x: f64, y: f64| Row {
                t: Some("LineTo".into()),
                ix,
                cells: vec![
                    ("X".to_string(), Cell::value(format!("{x}"))),
                    ("Y".to_string(), Cell::value(format!("{y}"))),
                ],
                ..Row::default()
            };
            let mut sec = Section {
                name: "Geometry".into(),
                ix: 0,
                ..Section::default()
            };
            sec.rows = vec![
                Row {
                    t: Some("MoveTo".into()),
                    ix: 1,
                    cells: vec![
                        ("X".to_string(), Cell::value("0")),
                        ("Y".to_string(), Cell::value("0")),
                    ],
                    ..Row::default()
                },
                line(2, w, h),
            ];
            s.sheet.children.push(SheetChild::Section(sec));
            s
        };
        let (doc, page) = page_fixture(vec![connector(1, (3.0, 5.0), 4.0, 0.0)]);
        let svg = render_page(&doc, &page).unwrap();
        assert!(
            !svg.contains(r##"data-dc="line-jumps""##),
            "no crossings, no overlay"
        );
    }

    /// #203: a NURBSTo row's E cell (formula text) — the interior
    /// points/knots/weights ride in `E`, the endpoint in X/Y.
    fn nurbs_row(ix: i64, x: f64, y: f64, e_text: &str) -> Row {
        Row {
            t: Some("NURBSTo".into()),
            ix,
            cells: vec![
                ("X".to_string(), Cell::value(format!("{x}"))),
                ("Y".to_string(), Cell::value(format!("{y}"))),
                ("A".to_string(), Cell::value("0")),
                ("B".to_string(), Cell::value("1")),
                ("C".to_string(), Cell::value("0")),
                ("D".to_string(), Cell::value("1")),
                (
                    "E".to_string(),
                    vsdx::Cell {
                        v: Some(e_text.to_string()),
                        f: Some(e_text.to_string()),
                        ..Cell::default()
                    },
                ),
            ],
            ..Row::default()
        }
    }

    /// #203: the golden fixture page — (1) a SplineStart+2×SplineKnot
    /// cubic, (2) a NURBSTo with uniform weights, (3) a NURBSTo with
    /// NON-uniform weights (the rational case the chord approximates
    /// wrongly). Committed as an SVG golden; regenerate with
    /// UPDATE_SPLINE_GOLDENS=1.
    #[test]
    fn spline_nurbs_goldens() {
        // 1. the spline: MoveTo(0,0), SplineStart (second control
        //    point) with knots 0/0.3/1 degree 3, knots at 0.6/1.
        let mut spline = shape_at(1, (2.0, 2.0), 4.0, 2.0, 0.0);
        push_geo(
            &mut spline,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                geo_row(
                    2,
                    "SplineStart",
                    &[
                        ("X", 1.0),
                        ("Y", 1.5),
                        ("A", 0.3),
                        ("B", 0.0),
                        ("C", 1.0),
                        ("D", 3.0),
                    ],
                ),
                geo_row(3, "SplineKnot", &[("X", 2.5), ("Y", 0.4), ("A", 0.6)]),
                geo_row(4, "SplineKnot", &[("X", 4.0), ("Y", 2.0), ("A", 1.0)]),
            ],
        );
        // 2. NURBS uniform: M(0,0) → NURBSTo(4,2), interior points
        //    (1.2, 1.6) and (2.8, 0.2), weights 1.
        let mut nurbs_u = shape_at(2, (7.0, 2.0), 4.0, 2.0, 0.0);
        push_geo(
            &mut nurbs_u,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                nurbs_row(2, 4.0, 2.0, "NURBS(1, 3, 0, 0, 1.2,1.6,0,1, 2.8,0.2,0,1)"),
            ],
        );
        // 3. NURBS rational: the same geometry with the middle
        //    control weighted 2 — the belly pulls toward that point.
        let mut nurbs_r = shape_at(3, (12.0, 2.0), 4.0, 2.0, 0.0);
        push_geo(
            &mut nurbs_r,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                nurbs_row(2, 4.0, 2.0, "NURBS(1, 3, 0, 0, 1.2,1.6,0,2, 2.8,0.2,0,1)"),
            ],
        );
        let (doc, page) = page_fixture(vec![spline, nurbs_u, nurbs_r]);
        let svg = render_page(&doc, &page).unwrap();

        let regenerate = std::env::var("UPDATE_SPLINE_GOLDENS").is_ok();
        let golden_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/goldens/spline_nurbs.svg");
        if regenerate {
            let _ = std::fs::create_dir_all(golden_path.parent().unwrap());
            std::fs::write(&golden_path, &svg).unwrap();
        }
        let golden = std::fs::read_to_string(&golden_path)
            .expect("the spline/NURBS golden exists (regenerate with UPDATE_SPLINE_GOLDENS=1)");
        // Git's autocrlf normalizes text files to CRLF on Windows
        // checkouts; the renderer emits LF. Compare newline-normalized.
        let norm = |s: &str| s.replace('\r', "");
        assert_eq!(
            norm(&svg).trim(),
            norm(&golden).trim(),
            "the spline/NURBS golden drifted — if the change is intended, \
             regenerate with UPDATE_SPLINE_GOLDENS=1 and review the diff"
        );
        // Each curve emits a multi-point polyline well above the
        // chord: count `L` segments (16 per knot span, >8 per curve).
        let segments = svg.match_indices("L ").count();
        assert!(
            segments > 24,
            "three flattened curves produce many segments: {segments}"
        );
    }

    fn page_fixture(shapes: Vec<Shape>) -> (Document, Page) {
        let mut sheet = Sheet::default();
        sheet.set_cell("PageWidth", Cell::value("8.5"));
        sheet.set_cell("PageHeight", Cell::value("11"));
        let page = Page {
            id: 0,
            name: "T".into(),
            children: vec![IndexChild::Sheet {
                tag: "PageSheet".into(),
                sheet,
            }],
            shapes,
            ..Page::default()
        };
        (
            Document {
                pages: vec![page.clone()],
                ..Document::default()
            },
            page,
        )
    }

    fn shape_at(id: i64, pin: (f64, f64), w: f64, h: f64, angle_deg: f64) -> Shape {
        let mut sh = Shape {
            id,
            ..Shape::default()
        };
        for (n, v) in [
            ("PinX", pin.0),
            ("PinY", pin.1),
            ("Width", w),
            ("Height", h),
            ("LocPinX", w / 2.0),
            ("LocPinY", h / 2.0),
            ("Angle", angle_deg),
        ] {
            sh.sheet.set_cell(n, Cell::value(format!("{v}")));
        }
        sh
    }

    /// A geometry row built from (type, [(cell, value)]).
    fn geo_row(ix: i64, t: &str, cells: &[(&str, f64)]) -> Row {
        Row {
            t: Some(t.into()),
            ix,
            cells: cells
                .iter()
                .map(|(n, v)| (n.to_string(), Cell::value(format!("{v}"))))
                .collect(),
            ..Row::default()
        }
    }

    fn push_geo(s: &mut Shape, rows: Vec<Row>) {
        let mut sec = Section {
            name: "Geometry".into(),
            ix: 0,
            ..Section::default()
        };
        sec.rows = rows;
        s.sheet.children.push(SheetChild::Section(sec));
    }

    /// #219: the focused bounds of a sub-shape's own subtree — the
    /// damage region a localized edit re-rasters — stay inside the
    /// whole-subtree bounds, equal the member's own box (plus the
    /// stroke pad), and move with the member's cells.
    #[test]
    fn focused_bounds_scope_the_walk_to_one_subtree() {
        let mut member = shape_at(2, (0.4, 0.3), 0.5, 0.4, 0.0);
        push_geo(
            &mut member,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                geo_row(2, "LineTo", &[("X", 0.5), ("Y", 0.0)]),
                geo_row(3, "LineTo", &[("X", 0.5), ("Y", 0.4)]),
                geo_row(4, "LineTo", &[("X", 0.0), ("Y", 0.4)]),
                geo_row(5, "LineTo", &[("X", 0.0), ("Y", 0.0)]),
            ],
        );
        let mut group = shape_at(1, (2.0, 2.0), 2.0, 1.5, 0.0);
        group.type_ = Some("Group".into());
        let mut far = shape_at(3, (1.8, 1.3), 0.4, 0.3, 0.0);
        push_geo(
            &mut far,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                geo_row(2, "LineTo", &[("X", 0.4), ("Y", 0.3)]),
            ],
        );
        group.sub_shapes = vec![member, far];
        let (doc, page) = page_fixture(vec![group]);

        let whole = shape_ink_bounds(&doc, &page, 1).unwrap();
        let focused = shape_ink_bounds_focused(&doc, &page, 1, Some(2)).unwrap();
        // The member's box (0.5×0.4 at group-local (0.15..0.65, …))
        // plus the same stroke-bleed pad — not the whole group.
        assert!((focused.2 - (0.5 + 0.16)).abs() < 1e-9, "w={}", focused.2);
        assert!((focused.3 - (0.4 + 0.16)).abs() < 1e-9, "h={}", focused.3);
        // Containment: the focused region sits inside the subtree's.
        assert!(focused.0 >= whole.0 - 1e-9 && focused.1 >= whole.1 - 1e-9);
        assert!(focused.0 + focused.2 <= whole.0 + whole.2 + 1e-9);
        assert!(focused.1 + focused.3 <= whole.1 + whole.3 + 1e-9);

        // The walk is honest about WHICH member: the far sibling's
        // focused bounds sit elsewhere.
        let far_focus = shape_ink_bounds_focused(&doc, &page, 1, Some(3)).unwrap();
        assert!((far_focus.0 - focused.0).abs() > 0.5);
        // An unknown focus yields nothing.
        assert!(shape_ink_bounds_focused(&doc, &page, 1, Some(99)).is_none());
    }

    /// #188: SplineStart + SplineKnot rows render the EVALUATED
    /// B-spline — a multi-segment path that leaves the control-point
    /// chord — not the v0.2 single chord.
    #[test]
    fn spline_rows_render_the_evaluated_curve() {
        let mut s = shape_at(1, (2.0, 2.0), 4.0, 2.0, 0.0);
        // P0=(0,0); P1=SplineStart (1,1); P2/P3=SplineKnots; cubic.
        push_geo(
            &mut s,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                geo_row(
                    2,
                    "SplineStart",
                    &[
                        ("X", 1.0),
                        ("Y", 1.5),
                        ("A", 0.4),
                        ("B", 0.0),
                        ("C", 1.0),
                        ("D", 3.0),
                    ],
                ),
                geo_row(3, "SplineKnot", &[("X", 2.0), ("Y", 0.5), ("A", 0.6)]),
                geo_row(4, "SplineKnot", &[("X", 4.0), ("Y", 2.0), ("A", 1.0)]),
            ],
        );
        let (doc, page) = page_fixture(vec![s]);
        let svg = render_page(&doc, &page).unwrap();
        let path = svg.match_indices("L ").count();
        assert!(
            path > 8,
            "the spline flattens to many segments, not a chord: {} L segments",
            path
        );
        // The ink leaves the P0→P3 chord (the S bulge through P1/P2).
        // The chord from (0,0) to (4,2): at x=2 the chord y is 1; the
        // spline's belly passes well below (through ~(2, 0.9±)) — assert
        // some emitted coordinate off the chord line instead of exact.
        let off = svg
            .match_indices("L ")
            .map(|(i, _)| &svg[i + 2..i + 30])
            .filter_map(|seg| {
                let mut it = seg.split_whitespace();
                let x: f64 = it.next()?.parse().ok()?;
                let y: f64 = it.next()?.parse().ok()?;
                Some((x, y))
            })
            .find(|(x, y)| (0.5 * x + 2.0 - y).abs() > 0.05);
        assert!(off.is_some(), "some flattened point leaves the chord");
    }

    /// #188: the corpus NURBSTo row (E = NURBS(...) with the row-cell
    /// frame) renders off its chord — the v0.2 single chord is gone.
    #[test]
    fn nurbs_to_row_renders_from_the_e_formula() {
        let mut s = shape_at(1, (2.0, 2.0), 4.0, 2.0, 0.0);
        let e = "NURBS(1, 3, 0, 0, 0.35,0.9,0,1, 0.65,0.1,0,1)";
        push_geo(
            &mut s,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                geo_row(
                    2,
                    "NURBSTo",
                    &[
                        ("X", 4.0),
                        ("Y", 2.0),
                        ("A", 0.0),
                        ("B", 1.0),
                        ("C", 0.0),
                        ("D", 1.0),
                    ],
                ),
            ],
        );
        // The E cell with its formula text.
        {
            let sec = s
                .sheet
                .children
                .iter_mut()
                .find_map(|c| match c {
                    SheetChild::Section(sec) if sec.name == "Geometry" => Some(sec),
                    _ => None,
                })
                .unwrap();
            let r2 = sec.rows.iter_mut().find(|r| r.ix == 2).unwrap();
            r2.cells.push((
                "E".to_string(),
                Cell {
                    v: Some(e.to_string()),
                    f: Some(e.to_string()),
                    ..Cell::default()
                },
            ));
        }
        let (doc, page) = page_fixture(vec![s]);
        let svg = render_page(&doc, &page).unwrap();
        let segments = svg.match_indices("L ").count();
        assert!(
            segments > 8,
            "the NURBS flattens to many segments: {segments}"
        );
        // Interior points (0.35·4=1.4, 0.9·2=1.8) and (2.6, 0.2) pull
        // the curve off the (0,0)→(4,2) chord; some emitted point must
        // sit off it (chord: y = 0.5x).
        let off = svg
            .match_indices("L ")
            .map(|(i, _)| &svg[i + 2..i + 30])
            .filter_map(|seg| {
                let mut it = seg.split_whitespace();
                let x: f64 = it.next()?.parse().ok()?;
                let y: f64 = it.next()?.parse().ok()?;
                Some((x, y))
            })
            .find(|(x, y)| (0.5 * x + 2.0 - y).abs() > 0.05);
        assert!(off.is_some(), "some flattened point leaves the chord");
    }

    const PAD: f64 = 0.08;

    /// Text taller than its frame: the ink box covers the overflow on
    /// the side the alignment pushes it to (CodeRabbit on PR #325).
    /// Top-aligned overflows downward only, bottom-aligned upward only.
    #[test]
    fn ink_bounds_cover_aligned_text_overflow() {
        let bounds = |va: &str| {
            let mut s = shape_at(1, (4.0, 5.0), 1.0, 1.0, 0.0);
            s.sheet.text = Some("1\n2\n3\n4\n5\n6\n7".into());
            s.sheet.set_cell("VerticalAlign", Cell::value(va));
            let (doc, page) = page_fixture(vec![s]);
            let (_x, y, _w, h) = shape_ink_bounds(&doc, &page, 1).unwrap();
            (y, y + h)
        };
        // The frame spans y 5.5..6.5 (page_h 11, y-down).
        let (t0, t1) = bounds("0");
        assert!(
            (t0 - (5.5 - PAD)).abs() < 1e-9,
            "top stays at the frame: {t0}"
        );
        assert!(t1 > 6.5 + PAD + 0.1, "top-aligned overflow below: {t1}");
        let (b0, b1) = bounds("2");
        assert!(b0 < 5.5 - PAD - 0.1, "bottom-aligned overflow above: {b0}");
        assert!(
            (b1 - (6.5 + PAD)).abs() < 1e-9,
            "bottom stays at the frame: {b1}"
        );
        let (m0, m1) = bounds("1");
        assert!(
            ((5.5 - m0) - (m1 - 6.5)).abs() < 1e-9 && m1 > 6.5 + PAD,
            "middle overflows both ways evenly: {m0}..{m1}"
        );
    }

    #[test]
    fn ink_bounds_cover_rotation() {
        // 2×1 shape rotated 90° about its pin (the Angle cell is
        // radians, like render_shape reads it): the ink swaps extents —
        // a plain W×H box (what master_bbox computes) would clip it.
        let (doc, page) = page_fixture(vec![shape_at(
            1,
            (4.0, 5.0),
            2.0,
            1.0,
            std::f64::consts::FRAC_PI_2,
        )]);
        let (x, y, w, h) = shape_ink_bounds(&doc, &page, 1).unwrap();
        assert!((w - (1.0 + 2.0 * PAD)).abs() < 1e-9, "w={w}");
        assert!((h - (2.0 + 2.0 * PAD)).abs() < 1e-9, "h={h}");
        // Same center as the unrotated box: pin (4,5), page_h 11.
        assert!((x + w / 2.0 - 4.0).abs() < 1e-9);
        assert!((y + h / 2.0 - 6.0).abs() < 1e-9);
    }

    #[test]
    fn ink_bounds_union_subshapes_and_text() {
        // A 2×1 group whose sub-shape sits beyond the group's right
        // edge (local pin 2.5 of a 2-wide box) and whose text block is
        // wider than the shape: the union must cover both.
        let mut group = shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0);
        group
            .sub_shapes
            .push(shape_at(2, (2.5, 0.8), 1.0, 1.0, 0.0));
        group.sheet.text = Some("hello".into());
        group.sheet.set_cell("TxtWidth", Cell::value("3"));
        let (doc, page) = page_fixture(vec![group]);
        let (x, _y, w, h) = shape_ink_bounds(&doc, &page, 1).unwrap();
        // Group box page x∈[3,5]; sub-shape local x∈[2,3] → page
        // x∈[5,6]; text 3 wide centered → x∈[2.5,5.5].
        let right = x + w - PAD;
        assert!((right - 6.0).abs() < 1e-9, "right={right}");
        let left = x + PAD;
        assert!((left - 2.5).abs() < 1e-9, "left={left}");
        assert!(h > 1.0, "h={h}");
    }

    #[test]
    fn ink_bounds_cover_outdented_bullets() {
        // A bulleted block with IndFirst = −1 hangs its glyph a full
        // inch left of the text area — outside the 2-wide shape's box
        // AND its text frame (local x0 = 0 here). The ink box must
        // reach the glyph: the layer viewBox and the #236 cull are
        // built from these bounds, and a frame-only bound would clip
        // the bullet and skip the shape on a cull that covers only
        // the overhang (local CodeRabbit round 3).
        let mut s = shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0);
        s.sheet.text = Some("bullet".into());
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Paragraph".into(),
            rows: vec![Row {
                cells: vec![
                    ("Bullet".to_string(), Cell::value("1")),
                    ("IndFirst".to_string(), Cell::value("-1")),
                ],
                ..Row::default()
            }],
            ..Section::default()
        }));
        let (doc, page) = page_fixture(vec![s.clone()]);
        let (x, _y, _w, _h) = shape_ink_bounds(&doc, &page, 1).unwrap();
        // The shape box spans page x∈[3,5] (frame x0 = 0 local); the
        // bullet reaches local ins_l − 1 − its advance. Assert the
        // bound's left edge tracks that escape exactly (PAD included).
        let chain = [&s];
        let tb = resolve_text_block(&doc, &s, &chain, 2.0, 1.0);
        let b = tb.bullet.as_ref().expect("the block is bulleted");
        let want_left = 3.0 + (tb.insets.0 + tb.ind_first) - bullet_advance_in(&tb, b) - PAD;
        assert!(
            (x - want_left).abs() < 1e-9,
            "left edge covers the outdented bullet: {x} vs {want_left}"
        );
        // The cull regression: a rect covering ONLY the bullet
        // overhang intersects the ink bounds (the shape must not be
        // skipped when just its bullet is damaged) — vertically
        // inside the box, horizontally past the shape box's left.
        let overhang = (want_left, _y + _h / 2.0 - 0.05, 0.2, 0.1);
        assert!(
            boxes_intersect((x, _y, _w, _h), overhang),
            "a bullet-only rect still intersects the ink bounds"
        );
    }

    /// The rotated twin (final local CodeRabbit pass + Greptile round
    /// 8): under TxtAngle ≠ 0 the bounds must CONTAIN the rotated
    /// EXTENDED frame — the old centered circumscribed square covered
    /// only half of the one-sided bullet escape (and under-covered
    /// oblique rotations of the frame itself). The corners are
    /// rotated here independently of the production code (SVG rotate
    /// about the TxtPin, net = −TxtAngle at no flip).
    #[test]
    fn ink_bounds_cover_outdented_bullets_rotated() {
        let mut s = shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0);
        s.sheet.text = Some("bullet".into());
        s.sheet.set_cell(
            "TxtAngle",
            Cell::value(std::f64::consts::FRAC_PI_2.to_string()),
        );
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Paragraph".into(),
            rows: vec![Row {
                cells: vec![
                    ("Bullet".to_string(), Cell::value("1")),
                    ("IndFirst".to_string(), Cell::value("-1")),
                ],
                ..Row::default()
            }],
            ..Section::default()
        }));
        let (doc, page) = page_fixture(vec![s.clone()]);
        let (bx, by, bw, bh) = shape_ink_bounds(&doc, &page, 1).unwrap();
        // The escape: bullet origin 1 in left of the text area's left
        // inset, plus the glyph's own advance.
        let chain = [&s];
        let tb = resolve_text_block(&doc, &s, &chain, 2.0, 1.0);
        let b = tb.bullet.as_ref().expect("the block is bulleted");
        let e = -(tb.insets.0 + tb.ind_first) + bullet_advance_in(&tb, b);
        // Extended frame corners, shape-local y-down: [−e, 2] × [−1, 0]
        // (frame x0 = 0, th = 1). Net rotation −90° about the TxtPin
        // (1, −0.5): (dx, dy) → (dy, −dx). Shape-local → page: the
        // pin line (local y = 0) maps to page y 6.5, x = 3 + local.
        let pivot = (1.0, -0.5);
        let corners = [(-e, -1.0), (2.0, -1.0), (2.0, 0.0), (-e, 0.0)];
        let mut pts = Vec::new();
        for (cx, cy) in corners {
            let (dx, dy) = (cx - pivot.0, cy - pivot.1);
            let (rx, ry) = (pivot.0 + dy, pivot.1 - dx);
            pts.push((3.0 + rx, 6.5 + ry));
        }
        // The bounds must equal the EXACT hull of the rotated corners
        // union the shape box (page [3,5]×[5.5,6.5]) plus PAD —
        // containment alone let the round-8 ORDER bug pass (the
        // backwards composition built a giant hull around a
        // shape-local pivot that contained everything; Greptile
        // round 9).
        let hull_x = (
            pts.iter()
                .map(|p| p.0)
                .fold(f64::INFINITY, f64::min)
                .min(3.0),
            pts.iter()
                .map(|p| p.0)
                .fold(f64::NEG_INFINITY, f64::max)
                .max(5.0),
        );
        let hull_y = (
            pts.iter()
                .map(|p| p.1)
                .fold(f64::INFINITY, f64::min)
                .min(5.5),
            pts.iter()
                .map(|p| p.1)
                .fold(f64::NEG_INFINITY, f64::max)
                .max(6.5),
        );
        assert!(
            (bx - (hull_x.0 - PAD)).abs() < 1e-9
                && ((bx + bw) - (hull_x.1 + PAD)).abs() < 1e-9
                && (by - (hull_y.0 - PAD)).abs() < 1e-9
                && ((by + bh) - (hull_y.1 + PAD)).abs() < 1e-9,
            "the bounds are the exact rotated hull + PAD: got \
             [{bx:.4},{by:.4} {bw:.4}×{bh:.4}], want [{:.4},{:.4} \
             {:.4}×{:.4}] (e={e:.4})",
            hull_x.0 - PAD,
            hull_y.0 - PAD,
            hull_x.1 - hull_x.0 + 2.0 * PAD,
            hull_y.1 - hull_y.0 + 2.0 * PAD
        );
        // …and the fixture BITES on both prior forms: the far corner
        // sits past the old PIN-centered square's reach (the old
        // square centered on the TxtPin's page point (4, 6.0))…
        let far = pts[0].1;
        let old_reach = 6.0 + (2.0 + e) / 2.0;
        assert!(
            far > old_reach,
            "fixture: the far corner {far:.3} escapes the old square's \
             reach {old_reach:.3}"
        );
        // …and a wrong-order hull would tower over the tight one.
        assert!(
            bh < 6.0,
            "fixture: the tight hull is a few inches, not a giant \
             wrong-order hull (h={bh:.3})"
        );
    }

    /// Local CodeRabbit round 3 + Greptile round 7: paragraph cells
    /// inherit through the TEXT-STYLE stylesheet cascade like
    /// character cells do (`char_cell`'s walk) — a styled shape with
    /// no local or master Paragraph row still picks up the style's
    /// IndLeft/Bullet, a local cell still wins, a local row carrying
    /// only another cell does not shadow the style (per-cell
    /// inheritance), and INHERITED cells (F="Inh") never satisfy a
    /// level: their cached V is stale against the live parent, so the
    /// parent's current value wins (resolve_cell's rule).
    #[test]
    fn paragraph_cells_inherit_the_text_style_cascade() {
        // Parent style 8 carries the live values; child style 7
        // caches a STALE IndLeft under F="Inh" and links to 8.
        let parent = {
            let mut sh = Sheet::default();
            sh.children.push(SheetChild::Section(Section {
                name: "Paragraph".into(),
                rows: vec![Row {
                    cells: vec![
                        ("IndLeft".to_string(), Cell::value("0.25")),
                        ("Bullet".to_string(), Cell::value("2")),
                    ],
                    ..Row::default()
                }],
                ..Section::default()
            }));
            vsdx::model::StyleSheet {
                id: 8,
                name_u: None,
                attrs: Vec::new(),
                sheet: sh,
            }
        };
        let child = {
            let mut sh = Sheet::default();
            sh.children.push(SheetChild::Section(Section {
                name: "Paragraph".into(),
                rows: vec![Row {
                    cells: vec![(
                        "IndLeft".to_string(),
                        Cell {
                            v: Some("0.5".into()),
                            f: Some("Inh".into()),
                            ..Cell::default()
                        },
                    )],
                    ..Row::default()
                }],
                ..Section::default()
            }));
            vsdx::model::StyleSheet {
                id: 7,
                name_u: None,
                attrs: vec![("TextStyle".to_string(), "8".to_string())],
                sheet: sh,
            }
        };
        let mut doc = Document::default();
        doc.stylesheets.push(parent);
        doc.stylesheets.push(child);
        let base = || {
            let mut s = Shape::default();
            s.sheet.set_cell("Width", Cell::value("2.0"));
            s.sheet.text = Some("styled".into());
            s.text_style = Some(7);
            s
        };
        // Bare styled shape: the PARENT's live value wins through the
        // child's stale Inh cache.
        let s = base();
        let tb = resolve_text_block(&doc, &s, &[&s], 2.0, 1.0);
        assert!(
            (tb.ind_left - 0.25).abs() < 1e-12,
            "the parent's live IndLeft wins over the child's Inh cache: {}",
            tb.ind_left
        );
        assert!(tb.bullet.is_some(), "the style's Bullet arms the block");
        // A stale Inh cache on the SHAPE itself never wins either.
        let mut stale = base();
        stale.sheet.children.push(SheetChild::Section(Section {
            name: "Paragraph".into(),
            rows: vec![Row {
                cells: vec![(
                    "IndLeft".to_string(),
                    Cell {
                        v: Some("9".into()),
                        f: Some("Inh".into()),
                        ..Cell::default()
                    },
                )],
                ..Row::default()
            }],
            ..Section::default()
        }));
        let tb = resolve_text_block(&doc, &stale, &[&stale], 2.0, 1.0);
        assert!(
            (tb.ind_left - 0.25).abs() < 1e-12,
            "a shape-level Inh cache falls through to the style: {}",
            tb.ind_left
        );
        // A local cell WINS over the style.
        let mut local = base();
        local.sheet.children.push(SheetChild::Section(Section {
            name: "Paragraph".into(),
            rows: vec![Row {
                cells: vec![("IndLeft".to_string(), Cell::value("0.5"))],
                ..Row::default()
            }],
            ..Section::default()
        }));
        let tb = resolve_text_block(&doc, &local, &[&local], 2.0, 1.0);
        assert!(
            (tb.ind_left - 0.5).abs() < 1e-12,
            "the local IndLeft wins: {}",
            tb.ind_left
        );
        // A local row carrying ONLY HorzAlign leaves the style's
        // IndLeft standing (per-cell inheritance, PR #284 round 2).
        let mut shadow = base();
        shadow.sheet.children.push(SheetChild::Section(Section {
            name: "Paragraph".into(),
            rows: vec![Row {
                cells: vec![("HorzAlign".to_string(), Cell::value("0"))],
                ..Row::default()
            }],
            ..Section::default()
        }));
        let tb = resolve_text_block(&doc, &shadow, &[&shadow], 2.0, 1.0);
        assert!(
            (tb.ind_left - 0.25).abs() < 1e-12,
            "a HorzAlign-only local row never shadows the style: {}",
            tb.ind_left
        );
    }

    #[test]
    fn layer_svg_scopes_the_viewbox_and_skips_hidden() {
        let (doc, page) = page_fixture(vec![shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0)]);
        let bounds = (1.0, 2.0, 3.0, 4.0);
        let svg = render_shape_layer(&doc, &page, 1, &[], bounds).unwrap();
        assert!(
            svg.contains(r#"viewBox="1.00000 2.00000 3.00000 4.00000""#),
            "viewBox must be the given bounds: {svg}"
        );
        assert!(svg.contains("<g transform"), "the shape is drawn");
        // A hidden root emits the (empty) svg only — same skip the
        // page renderer applies to hidden members.
        let mut hidden_shape = shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0);
        hidden_shape.sheet.set_cell("LayerMember", Cell::value("2"));
        let (doc, page) = page_fixture(vec![hidden_shape]);
        let svg = render_shape_layer(&doc, &page, 1, &[2], bounds).unwrap();
        assert!(
            !svg.contains("<g transform"),
            "hidden root must not draw: {svg}"
        );
    }

    /// #236: the culled patch serialization omits every subtree node
    /// whose ink box misses the (padded) cull rect and keeps the ones
    /// it covers — the serialize+parse saving is real, and a
    /// regression to full serialization would re-include the far
    /// shape's ink. The group node itself always stays (its subtree
    /// box spans both leaves).
    #[test]
    fn culled_serialization_omits_shapes_outside_the_cull_rect() {
        // A group with two disjoint leaves: red left, green right.
        // Real Geometry rows, so the leaves actually paint.
        let rect_rows = |w: f64, h: f64| {
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                geo_row(2, "LineTo", &[("X", w), ("Y", 0.0)]),
                geo_row(3, "LineTo", &[("X", w), ("Y", h)]),
                geo_row(4, "LineTo", &[("X", 0.0), ("Y", h)]),
                geo_row(5, "LineTo", &[("X", 0.0), ("Y", 0.0)]),
            ]
        };
        let mut left = shape_at(2, (0.5, 0.5), 0.6, 0.4, 0.0);
        left.sheet.set_cell("FillForegnd", Cell::value("#ff2200"));
        push_geo(&mut left, rect_rows(0.6, 0.4));
        let mut right = shape_at(3, (2.5, 0.5), 0.6, 0.4, 0.0);
        right.sheet.set_cell("FillForegnd", Cell::value("#22cc00"));
        push_geo(&mut right, rect_rows(0.6, 0.4));
        let mut group = shape_at(1, (3.0, 5.0), 3.0, 1.0, 0.0);
        group.sub_shapes = vec![left, right];
        let (doc, page) = page_fixture(vec![group]);
        let bounds = shape_ink_bounds(&doc, &page, 1).unwrap();
        // The cull rect covers the LEFT leaf's x range only (the
        // leaves share their y range, so x does the separating).
        let cull = (bounds.0, bounds.1, 1.4, bounds.3);
        let (_, culled) =
            render_shape_layer_opts_culled(&doc, &page, 1, &[], bounds, None, None, cull, None)
                .unwrap();
        assert!(
            culled.contains("#ff2200"),
            "the covered leaf stays in the patch: {culled}"
        );
        assert!(
            !culled.contains("#22cc00"),
            "the far leaf is omitted from the patch serialization: {culled}"
        );
        // The full serialization keeps both.
        let (_, full) =
            render_shape_layer_opts(&doc, &page, 1, &[], bounds, None, None, None).unwrap();
        assert!(full.contains("#ff2200") && full.contains("#22cc00"));
    }

    /// The Text Block Format cell is `VerticalAlign` with 0 top,
    /// 1 middle, 2 bottom (Visio 2021 witness: three boxes with 0/1/2
    /// put the text top/middle/bottom). It maps onto the block's
    /// 1/2/3 encoding; out-of-range values clamp.
    #[test]
    fn vertical_align_cell_maps_top_middle_bottom() {
        let va = |v: Option<&str>| {
            let mut s = shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0);
            if let Some(v) = v {
                s.sheet.set_cell("VerticalAlign", Cell::value(v));
            }
            let (doc, page) = page_fixture(vec![s]);
            text_block(&doc, &page, 1).unwrap().valign
        };
        assert_eq!(va(Some("0")), 1, "0 = top");
        assert_eq!(va(Some("1")), 2, "1 = middle");
        assert_eq!(va(Some("2")), 3, "2 = bottom");
        assert_eq!(va(None), 2, "absent = middle");
        assert_eq!(va(Some("7")), 3, "clamped");
    }

    /// Text Block Format cells come from the TEXT style. A shape with
    /// a different line style must still pick up the text style's
    /// `VerticalAlign` and margins (Greptile/CodeRabbit on PR #325).
    #[test]
    fn text_block_cells_resolve_through_the_text_style() {
        let style = |id: i64, cells: &[(&str, &str)]| {
            let mut sh = Sheet::default();
            for (n, v) in cells {
                sh.set_cell(n, Cell::value(*v));
            }
            vsdx::model::StyleSheet {
                id,
                name_u: None,
                attrs: Vec::new(),
                sheet: sh,
            }
        };
        let mut doc = Document::default();
        doc.stylesheets.push(style(3, &[("LineWeight", "0.02")]));
        doc.stylesheets
            .push(style(4, &[("VerticalAlign", "0"), ("TopMargin", "0.2")]));
        let mut s = shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0);
        s.sheet.text = Some("styled".into());
        s.line_style = Some(3);
        s.fill_style = Some(3);
        s.text_style = Some(4);
        let tb = resolve_text_block(&doc, &s, &[&s], 2.0, 1.0);
        assert_eq!(tb.valign, 1, "the text style's VerticalAlign=0 is top");
        assert!(
            (tb.insets.2 - 0.2).abs() < 1e-12,
            "the text style's TopMargin: {}",
            tb.insets.2
        );
    }

    /// 1-D shapes (BeginX/EndX) fill their CLOSED sections — the
    /// block-arrow masters (Visio 2021 witness, t04) — while an open
    /// path keeps ignoring a decorative FillPattern.
    #[test]
    fn one_d_shape_fills_closed_geometry_only() {
        let render = |closed: bool| {
            let mut s = shape_at(1, (2.0, 2.0), 3.0, 1.0, 0.0);
            for (n, v) in [
                ("BeginX", "0.5"),
                ("BeginY", "2"),
                ("EndX", "3.5"),
                ("EndY", "2"),
            ] {
                s.sheet.set_cell(n, Cell::value(v));
            }
            s.sheet.set_cell("FillForegnd", Cell::value("#ed7d31"));
            s.sheet.set_cell("FillPattern", Cell::value("1"));
            let mut rows = vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.25)]),
                geo_row(2, "LineTo", &[("X", 2.5), ("Y", 0.25)]),
                geo_row(3, "LineTo", &[("X", 3.0), ("Y", 0.5)]),
                geo_row(4, "LineTo", &[("X", 2.5), ("Y", 0.75)]),
                geo_row(5, "LineTo", &[("X", 0.0), ("Y", 0.75)]),
            ];
            if closed {
                rows.push(geo_row(6, "LineTo", &[("X", 0.0), ("Y", 0.25)]));
            }
            push_geo(&mut s, rows);
            let (doc, page) = page_fixture(vec![s]);
            render_page(&doc, &page).unwrap()
        };
        assert!(render(true).contains(r##"fill="#ed7d31""##));
        assert!(!render(false).contains("#ed7d31"));
    }

    /// A closed curve followed by a second closed outline in the same
    /// section still fills: the curve row ends without a space, so the
    /// next `MoveTo` arrives glued to its last number (Greptile on PR
    /// #328).
    #[test]
    fn one_d_shape_fills_a_closed_curve_before_another_outline() {
        let mut s = shape_at(1, (2.0, 2.0), 3.0, 1.0, 0.0);
        for (n, v) in [
            ("BeginX", "0.5"),
            ("BeginY", "2"),
            ("EndX", "3.5"),
            ("EndY", "2"),
        ] {
            s.sheet.set_cell(n, Cell::value(v));
        }
        s.sheet.set_cell("FillForegnd", Cell::value("#ed7d31"));
        s.sheet.set_cell("FillPattern", Cell::value("1"));
        push_geo(
            &mut s,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.5)]),
                geo_row(
                    2,
                    "QuadBezTo",
                    &[("X", 1.0), ("Y", 0.5), ("A", 0.5), ("B", 1.0)],
                ),
                geo_row(
                    3,
                    "QuadBezTo",
                    &[("X", 0.0), ("Y", 0.5), ("A", 0.5), ("B", 0.0)],
                ),
                geo_row(4, "MoveTo", &[("X", 2.0), ("Y", 0.25)]),
                geo_row(5, "LineTo", &[("X", 3.0), ("Y", 0.25)]),
                geo_row(6, "LineTo", &[("X", 3.0), ("Y", 0.75)]),
                geo_row(7, "LineTo", &[("X", 2.0), ("Y", 0.25)]),
            ],
        );
        let (doc, page) = page_fixture(vec![s]);
        let svg = render_page(&doc, &page).unwrap();
        assert!(svg.contains(r##"fill="#ed7d31""##), "{svg}");
    }

    /// Closure is per subpath (Greptile on PR #328): two closed
    /// outlines with different starts both fill, an open subpath next
    /// to a closed one stays out of the fill, and a closed curve made
    /// of a single row fills too.
    #[test]
    fn closed_subpaths_are_judged_one_by_one() {
        let two = "M 0 0 L 1 0 L 1 1 L 0 0 M 5 5 L 6 5 L 6 6 L 5 5 ";
        assert_eq!(closed_subpaths(two).split_whitespace().count(), 24);
        let mixed = "M 0 0 L 1 0 L 1 1 M 5 5 L 6 5 L 6 6 L 5 5 ";
        let f = closed_subpaths(mixed);
        assert!(f.starts_with("M 5 5") && !f.contains("M 0 0"), "{f}");
        let curve = "M 0 0 C 1 0 0 1 0 0 ";
        assert_eq!(closed_subpaths(curve).trim(), curve.trim());
        assert_eq!(closed_subpaths("M 0 0 L 1 1 Z").trim(), "M 0 0 L 1 1 Z");
        assert!(closed_subpaths("M 0 0 L 1 0").is_empty());
        assert!(closed_subpaths("M 0 0").is_empty());
        // Curve rows end without a space, so the next `M` arrives
        // glued to the last number; both outlines still close.
        let glued = "M 0 0 C 1 0 0 1 0 0M 5 5 L 6 5 L 6 6 L 5 5 ";
        let f = closed_subpaths(glued);
        assert!(f.contains("M 0 0") && f.contains("M 5 5"), "{f}");
        let open_after_curve = "M 0 0 C 1 0 0 1 0 0M 5 5 L 0 0";
        let f = closed_subpaths(open_after_curve);
        assert!(f.contains("M 0 0") && !f.contains("M 5 5"), "{f}");
    }

    /// The geometry frame keeps the sign of a negative instance size:
    /// master width 2, instance width −4 → scale −2 → frame 2, so a
    /// fractional vertex at 0.5 lands at −2 after `emit_pt`'s scale,
    /// not at 1e12 (Greptile on PR #329).
    #[test]
    fn frame_dim_keeps_negative_scales() {
        assert!((frame_dim(-4.0, -2.0) - 2.0).abs() < 1e-12);
        assert!((0.5 * frame_dim(-4.0, -2.0) * -2.0 - -2.0).abs() < 1e-12);
        assert!((frame_dim(3.0, 1.5) - 2.0).abs() < 1e-12);
        assert_eq!(frame_dim(3.0, 0.0), 3.0, "a zero scale keeps the dim");
    }

    /// An instance PolylineTo row without its own A cell takes the
    /// master's POLYLINE() text; absolute vertices scale master →
    /// instance like X/Y (CodeRabbit on PR #329).
    #[test]
    fn polyline_to_inherits_the_master_polyline_text() {
        let poly_row = |x: f64, a: Option<&str>| {
            let mut r = geo_row(2, "PolylineTo", &[("X", x), ("Y", 0.0)]);
            if let Some(a) = a {
                r.cells.push((
                    "A".into(),
                    Cell {
                        v: Some(a.into()),
                        ..Cell::default()
                    },
                ));
            }
            r
        };
        let mut master = shape_at(1, (1.0, 0.5), 2.0, 1.0, 0.0);
        push_geo(
            &mut master,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                poly_row(2.0, Some("POLYLINE(1, 1, 0,1, 2,1)")),
            ],
        );
        let mut inst = shape_at(1, (4.0, 5.0), 4.0, 1.0, 0.0);
        inst.master = Some(7);
        push_geo(
            &mut inst,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                poly_row(4.0, None),
            ],
        );
        let (mut doc, page) = page_fixture(vec![inst]);
        doc.masters.push(vsdx::model::Master {
            id: 7,
            shapes: vec![master],
            ..vsdx::model::Master::default()
        });
        let svg = render_page(&doc, &page).unwrap();
        assert!(
            svg.contains("L 0.00000 -1.00000 L 4.00000 -1.00000 L 4.00000 -0.00000"),
            "the master's vertices, stretched to the instance width: {svg}"
        );
    }

    /// A master PolylineTo row and an instance row at the same IX.
    fn polyline_master_and_instance(
        inst_row: vsdx::Row,
        master_text: &str,
    ) -> (Document, vsdx::Page) {
        let mut a = geo_row(2, "PolylineTo", &[("X", 2.0), ("Y", 0.0)]);
        a.cells.push((
            "A".into(),
            Cell {
                v: Some(master_text.into()),
                ..Cell::default()
            },
        ));
        let mut master = shape_at(1, (1.0, 0.5), 2.0, 1.0, 0.0);
        push_geo(
            &mut master,
            vec![geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]), a],
        );
        let mut inst = shape_at(1, (4.0, 5.0), 4.0, 1.0, 0.0);
        inst.master = Some(7);
        push_geo(
            &mut inst,
            vec![geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]), inst_row],
        );
        let (mut doc, page) = page_fixture(vec![inst]);
        doc.masters.push(vsdx::model::Master {
            id: 7,
            shapes: vec![master],
            ..vsdx::model::Master::default()
        });
        (doc, page)
    }

    /// An instance that turns the master's PolylineTo row into an ArcTo
    /// keeps its own bend; the master's POLYLINE() text is not pushed
    /// into it (Greptile on PR #329).
    #[test]
    fn an_instance_arc_keeps_its_bend_over_a_master_polyline() {
        let arc = geo_row(2, "ArcTo", &[("X", 4.0), ("Y", 0.0), ("A", 0.5)]);
        let (doc, page) = polyline_master_and_instance(arc, "POLYLINE(1, 1, 0,1, 2,1)");
        let svg = render_page(&doc, &page).unwrap();
        assert!(
            !svg.contains("L 0.00000 -1.00000 L 4.00000 -1.00000"),
            "no master vertices on the arc: {svg}"
        );
        assert!(svg.contains(" A "), "the arc keeps its bend: {svg}");
    }

    /// The bounds walk the same inherited vertices the renderer draws:
    /// a master vertex 3 in above the baseline widens the ink box
    /// (Greptile on PR #329).
    #[test]
    fn inherited_polyline_vertices_bound_the_ink() {
        let row = geo_row(2, "PolylineTo", &[("X", 4.0), ("Y", 0.0)]);
        let (doc, page) = polyline_master_and_instance(row, "POLYLINE(1, 1, 0,3, 2,3)");
        let b = subtree_ink_boxes(&doc, &page, 1).unwrap()[&1];
        // (x, y, width, height): the shape is 1 in tall, the vertices
        // reach 3 in.
        assert!(b.3 > 2.5, "the box reaches the vertices: {b:?}");
    }

    /// A last vertex on the endpoint leaves no zero-length segment, so
    /// the end tangent and the arrowheads survive (CodeRabbit on PR
    /// #329).
    #[test]
    fn polyline_to_skips_a_vertex_on_its_endpoint() {
        // One copy of the endpoint, and several trailing copies
        // (Greptile on PR #329).
        for text in [
            "POLYLINE(1, 1, 1,0, 1,1)",
            "POLYLINE(1, 1, 1,0, 1,1, 1,1, 1,1)",
        ] {
            let mut s = shape_at(1, (2.0, 2.0), 1.0, 1.0, 0.0);
            let mut poly = geo_row(2, "PolylineTo", &[("X", 1.0), ("Y", 1.0)]);
            poly.cells.push((
                "A".into(),
                Cell {
                    v: Some(text.into()),
                    ..Cell::default()
                },
            ));
            push_geo(
                &mut s,
                vec![geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]), poly],
            );
            s.sheet.set_cell("EndArrow", Cell::value("2"));
            let (doc, page) = page_fixture(vec![s]);
            let svg = render_page(&doc, &page).unwrap();
            assert!(
                svg.contains("dc-ah"),
                "{text}: the arrowhead survives: {svg}"
            );
            assert!(
                !svg.contains("L 1.00000 -1.00000 L 1.00000 -1.00000"),
                "{text}: no repeated endpoint: {svg}"
            );
        }
    }

    /// PolylineTo walks its POLYLINE() vertices (Visio 2021 witness,
    /// t05): fractions of Width/Height for type 0, absolute local
    /// coordinates for type 1, then the row's X/Y endpoint — the
    /// stencils' router glyphs used to collapse to a single chord.
    #[test]
    fn polyline_to_draws_its_vertices() {
        let mut s = shape_at(1, (2.0, 2.0), 2.0, 1.0, 0.0);
        let mut poly = geo_row(2, "PolylineTo", &[("X", 0.0), ("Y", 0.0)]);
        poly.cells.push((
            "A".into(),
            Cell {
                v: Some("POLYLINE(0, 0, 1,0, 1,1)".into()),
                ..Cell::default()
            },
        ));
        push_geo(
            &mut s,
            vec![geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]), poly],
        );
        let (doc, page) = page_fixture(vec![s]);
        let svg = render_page(&doc, &page).unwrap();
        // (1,0)·(2,1) → (2,0) and (1,1)·(2,1) → (2,1) (y flipped).
        assert!(
            svg.contains("L 2.00000 -0.00000 L 2.00000 -1.00000 L 0.00000 -0.00000"),
            "{svg}"
        );
    }

    /// A 1×1 rectangle with closed geometry, for paint tests.
    fn rect_shape(id: i64, pin: (f64, f64)) -> Shape {
        let mut s = shape_at(id, pin, 1.0, 1.0, 0.0);
        push_geo(
            &mut s,
            vec![
                geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                geo_row(2, "LineTo", &[("X", 1.0), ("Y", 0.0)]),
                geo_row(3, "LineTo", &[("X", 1.0), ("Y", 1.0)]),
                geo_row(4, "LineTo", &[("X", 0.0), ("Y", 1.0)]),
                geo_row(5, "LineTo", &[("X", 0.0), ("Y", 0.0)]),
            ],
        );
        s
    }

    /// Fill/line transparency (Visio 2021 witness, t01): the *Trans
    /// cells are fractions; 0.5 paints at half alpha, 1 paints
    /// nothing at all (the stencils' invisible helper boxes), and
    /// opaque paint carries no opacity attribute.
    #[test]
    fn fill_and_line_transparency() {
        let render = |cells: &[(&str, &str)]| {
            let mut s = rect_shape(1, (2.0, 2.0));
            s.sheet.set_cell("FillForegnd", Cell::value("#ff0000"));
            s.sheet.set_cell("LineColor", Cell::value("#00ff00"));
            for (n, v) in cells {
                s.sheet.set_cell(n, Cell::value(*v));
            }
            let (doc, page) = page_fixture(vec![s]);
            render_page(&doc, &page).unwrap()
        };
        let opaque = render(&[]);
        assert!(opaque.contains("#ff0000") && !opaque.contains("opacity="));
        let half = render(&[("FillForegndTrans", "0.5"), ("LineColorTrans", "0.25")]);
        assert!(
            half.contains(r##"fill="#ff0000" fill-opacity="0.500""##),
            "{half}"
        );
        assert!(half.contains(r#"stroke-opacity="0.750""#), "{half}");
        let gone = render(&[("FillForegndTrans", "1"), ("LineColorTrans", "1")]);
        assert!(
            !gone.contains("#ff0000") && !gone.contains("#00ff00"),
            "100 % transparent paint is dropped: {gone}"
        );
        // A hatch pattern's dominant background color fades by
        // FillBkgndTrans, not FillForegndTrans.
        let bk = render(&[
            ("FillPattern", "2"),
            ("FillBkgnd", "#0000ff"),
            ("FillBkgndTrans", "0.5"),
        ]);
        assert!(bk.contains(r#"fill-opacity="0.500""#), "{bk}");
    }

    /// LineColorTrans also applies to the arrowheads: a half
    /// transparent line fades them, a fully transparent one hides them
    /// with the shaft (Greptile on PR #327).
    #[test]
    fn line_transparency_fades_and_hides_arrowheads() {
        let render = |trans: &str| {
            let mut s = shape_at(1, (2.0, 2.0), 1.0, 0.0, 0.0);
            push_geo(
                &mut s,
                vec![
                    geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                    geo_row(2, "LineTo", &[("X", 1.0), ("Y", 0.0)]),
                ],
            );
            s.sheet.set_cell("EndArrow", Cell::value("2"));
            s.sheet.set_cell("LineColor", Cell::value("#00ff00"));
            s.sheet.set_cell("LineColorTrans", Cell::value(trans));
            let (doc, page) = page_fixture(vec![s]);
            render_page(&doc, &page).unwrap()
        };
        let opaque = render("0");
        assert!(
            opaque.contains("dc-ah"),
            "an opaque line draws its head: {opaque}"
        );
        let half = render("0.5");
        assert!(
            half.contains("dc-ah")
                && half.contains(r#"<g fill-opacity="0.500" stroke-opacity="0.500">"#),
            "a half transparent line fades its head: {half}"
        );
        let gone = render("1");
        assert!(
            !gone.contains("dc-ah") && !gone.contains("#00ff00"),
            "a fully transparent line hides its head: {gone}"
        );
    }

    /// Visio 2013+ gradients (Visio 2021 witnesses t08–t10): the
    /// FillGradient stop rows drive the paint; Dir 0 runs along the
    /// angle (0° left → right), Dir 1–7 are radial from a box anchor,
    /// Dir 8–13 rectangular tiles clipped to the outline, and
    /// LineGradientEnabled paints the stroke.
    #[test]
    fn gradient_fill_directions_and_line_gradient() {
        let stops = |name: &str| {
            let mut sec = Section {
                name: name.into(),
                ..Section::default()
            };
            for (ix, (c, p)) in [("#ff0000", "0"), ("#0000ff", "1")].into_iter().enumerate() {
                sec.rows.push(Row {
                    ix: ix as i64,
                    cells: vec![
                        ("GradientStopColor".into(), Cell::value(c)),
                        ("GradientStopPosition".into(), Cell::value(p)),
                    ],
                    ..Row::default()
                });
            }
            SheetChild::Section(sec)
        };
        let render = |cells: &[(&str, &str)], line: bool| {
            let mut s = rect_shape(1, (2.0, 2.0));
            s.sheet
                .children
                .push(stops(if line { "LineGradient" } else { "FillGradient" }));
            for (n, v) in cells {
                s.sheet.set_cell(n, Cell::value(*v));
            }
            let (doc, page) = page_fixture(vec![s]);
            render_page(&doc, &page).unwrap()
        };
        let lin = render(
            &[("FillGradientEnabled", "1"), ("FillGradientAngle", "0")],
            false,
        );
        assert!(
            lin.contains(r#"x1="0.00000" y1="-0.50000" x2="1.00000" y2="-0.50000""#),
            "0° runs left → right across the box: {lin}"
        );
        assert!(lin.contains(r##"stop-color="#ff0000""##) && lin.contains("url(#dc-grad"));
        let rad = render(
            &[("FillGradientEnabled", "1"), ("FillGradientDir", "3")],
            false,
        );
        assert!(
            rad.contains(r#"<radialGradient id="#) && rad.contains(r#"cx="0.50000" cy="-0.50000""#),
            "{rad}"
        );
        let rect = render(
            &[("FillGradientEnabled", "1"), ("FillGradientDir", "10")],
            false,
        );
        assert!(rect.contains("<clipPath") && rect.matches("<linearGradient").count() == 4);
        // Disabled: the plain FillForegnd paint, no gradient.
        assert!(!render(&[("FillGradientEnabled", "0")], false).contains("Gradient id"));
        // A right-to-left connector (negative Width) keeps the
        // left → right sense of a 0° gradient.
        let mut neg = rect_shape(1, (2.0, 2.0));
        neg.sheet.set_cell("Width", Cell::value("-1"));
        neg.sheet.set_cell("FillGradientEnabled", Cell::value("1"));
        neg.sheet.set_cell("FillGradientAngle", Cell::value("0"));
        neg.sheet.children.push(stops("FillGradient"));
        let (doc, page) = page_fixture(vec![neg]);
        let neg = render_page(&doc, &page).unwrap();
        assert!(
            neg.contains(r#"x1="-1.00000" y1="-0.50000" x2="0.00000""#),
            "{neg}"
        );
        let stroke = render(&[("LineGradientEnabled", "1")], true);
        assert!(stroke.contains(r#"stroke="url(#dc-grad"#), "{stroke}");
    }

    /// Character runs (Visio 2021 witness c03): `<cp IX='n'/>` markers
    /// pick Character rows; a struck run gets a line through it, a red
    /// bold run its own positioned chunk, an underlined run a line under
    /// it. A block whose rows match the first row renders unchanged.
    #[test]
    fn character_runs_style_their_chunks() {
        let mut s = rect_shape(1, (2.0, 2.0));
        s.sheet.set_cell("Width", Cell::value("3"));
        s.sheet.text = Some("normal struck red".into());
        s.sheet.text_raw =
            Some("<cp IX='0'/>normal <cp IX='1'/>struck<cp IX='0'/> <cp IX='2'/>red".into());
        let mut sec = Section {
            name: "Character".into(),
            ..Section::default()
        };
        let row = |ix: i64, cells: &[(&str, &str)]| Row {
            ix,
            cells: cells
                .iter()
                .map(|(n, v)| (n.to_string(), Cell::value(*v)))
                .collect(),
            ..Row::default()
        };
        sec.rows.push(row(0, &[]));
        sec.rows.push(row(1, &[("Strikethru", "1")]));
        sec.rows
            .push(row(2, &[("Color", "#ff0000"), ("Style", "1")]));
        s.sheet.children.push(SheetChild::Section(sec));
        let (doc, page) = page_fixture(vec![s]);
        let svg = render_page(&doc, &page).unwrap();
        assert!(
            svg.contains(r##"fill="#ff0000" font-weight="bold">red</tspan>"##),
            "{svg}"
        );
        assert!(svg.contains(">struck</tspan>"), "{svg}");
        assert_eq!(
            svg.matches("<line ").count(),
            1,
            "one strike-through line: {svg}"
        );
    }

    /// A run set in a smaller size measures at that size when the line
    /// wraps (the network LLD's P2MP labels: 10 pt + 8 pt fit the
    /// 3.58 in block on one line in Visio; measured all at 10 pt they
    /// wrapped).
    #[test]
    fn smaller_runs_measure_at_their_own_size_when_wrapping() {
        let render = |tail: &str| {
            let mut s = rect_shape(1, (4.0, 2.0));
            for (n, v) in [
                ("Width", "3.5828"),
                ("LeftMargin", "0"),
                ("RightMargin", "0"),
            ] {
                s.sheet.set_cell(n, Cell::value(v));
            }
            s.sheet.text =
                Some("P2MP:vl2170:IPv4: 88.213.189.16/29 IPv6:2a00:d80:1420:1170::/64".into());
            s.sheet.text_raw = Some(
                "<cp IX='0'/>P2MP:vl2170:IPv4: 88.213.189.16/29 IPv6:<cp IX='1'/>2a00:d80:1420:1170::/64"
                    .into(),
            );
            let mut sec = Section {
                name: "Character".into(),
                ..Section::default()
            };
            let row = |ix: i64, size: &str| Row {
                ix,
                cells: vec![
                    ("Font".to_string(), Cell::value("Calibri")),
                    ("Size".to_string(), Cell::value(size)),
                ],
                ..Row::default()
            };
            sec.rows.push(row(0, "0.1388888888888889"));
            sec.rows.push(row(1, tail));
            s.sheet.children.push(SheetChild::Section(sec));
            let (doc, page) = page_fixture(vec![s]);
            render_page(&doc, &page).unwrap()
        };
        // Distinct baselines: a run's own chunk shares its line's y.
        let lines = |svg: &str| {
            let mut ys: Vec<&str> = svg
                .split(" y=\"")
                .skip(1)
                .filter_map(|t| t.split('"').next())
                .collect();
            ys.sort_unstable();
            ys.dedup();
            ys.len()
        };
        // The 8 pt tail fits on the first line; set at 10 pt it wraps.
        let mixed = render("0.1111111111111111");
        let uniform = render("0.1388888888888889");
        assert!(
            lines(&uniform) > lines(&mixed),
            "mixed: {mixed}\nuniform: {uniform}"
        );
    }

    /// One text block with character runs through `emit_text`: `raw`
    /// carries the `<cp IX='n'/>` markers, `rows` the Character rows by
    /// IX, `cells` extra sheet cells, `para` extra Paragraph cells.
    fn emit_runs(
        raw: &str,
        rows: &[(i64, &[(&str, &str)])],
        cells: &[(&str, &str)],
        para: &[(&str, &str)],
    ) -> String {
        let doc = Document::default();
        let mut s = Shape::default();
        let mut plain = String::new();
        let mut rest = raw;
        while let Some(i) = rest.find('<') {
            plain.push_str(&rest[..i]);
            rest = &rest[rest[i..].find('>').unwrap() + i + 1..];
        }
        plain.push_str(rest);
        s.sheet.text = Some(plain);
        s.sheet.text_raw = Some(raw.into());
        s.sheet.set_cell("Width", Cell::value("4.0"));
        s.sheet.set_cell("LeftMargin", Cell::value("0"));
        s.sheet.set_cell("RightMargin", Cell::value("0"));
        for (n, v) in cells {
            s.sheet.set_cell(n, Cell::value(*v));
        }
        let row = |ix: i64, cells: &[(&str, &str)]| Row {
            ix,
            cells: cells
                .iter()
                .map(|(n, v)| (n.to_string(), Cell::value(*v)))
                .collect(),
            ..Row::default()
        };
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Character".into(),
            rows: rows.iter().map(|(ix, c)| row(*ix, c)).collect(),
            ..Section::default()
        }));
        if !para.is_empty() {
            s.sheet.children.push(SheetChild::Section(Section {
                name: "Paragraph".into(),
                rows: vec![row(0, para)],
                ..Section::default()
            }));
        }
        let chain = [&s];
        let mut out = String::new();
        let w = cells
            .iter()
            .find(|(n, _)| *n == "Width")
            .map_or(4.0, |(_, v)| v.parse().unwrap());
        emit_text(
            &doc, &s, &chain, w, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        out
    }

    /// The `x`/`y` attribute values of every `<tspan>`, in order.
    fn tspan_xy(svg: &str) -> Vec<(f64, f64)> {
        svg.split("<tspan ")
            .skip(1)
            .filter_map(|t| {
                let get = |k: &str| {
                    t.split(&format!(" {k}=\""))
                        .nth(1)
                        .or_else(|| t.strip_prefix(&format!("{k}=\"")))?
                        .split('"')
                        .next()?
                        .parse::<f64>()
                        .ok()
                };
                Some((get("x")?, get("y")?))
            })
            .collect()
    }

    /// The wrap measures a paragraph set wholly in another size at that
    /// size, a bold run with the bold face, and survives a narrow CJK
    /// token whose split leaves nothing behind (Greptile on PR #350).
    #[test]
    fn wrap_runs_measure_uniform_sizes_bold_faces_and_empty_rests() {
        let face = crate::textmetrics::resolve(Some("Calibri"), false, false);
        let size = 0.1389;
        let text = "aaaa bbbb cccc";
        let n = text.chars().count();
        let w = face.width_in_runs(&[], text, size);
        let wrap = |usable: f64, runs: Option<&WrapRuns>| {
            wrap_block_text_scaled(&face, text, size, usable, &face, size, &[], 0.0, 0.0, runs)
                .len()
        };
        assert_eq!(wrap(w * 1.05, None), 1);
        let big = WrapRuns {
            factor: vec![2.0; n],
            face: vec![None; n],
            faces: Vec::new(),
        };
        assert!(wrap(w * 1.05, Some(&big)) > 1, "a 2x paragraph wraps");
        let bold_face = crate::textmetrics::resolve(Some("Calibri"), true, false);
        let bold_w = bold_face.width_in_runs(&[], text, size);
        assert!(bold_w > w * 1.01, "the bold face is wider");
        let bold = WrapRuns {
            factor: vec![1.0; n],
            face: vec![Some(0); n],
            faces: vec![bold_face],
        };
        assert!(wrap(w * 1.005, Some(&bold)) > 1, "a bold run wraps bold");
        let cjk = WrapRuns {
            factor: vec![1.0, 0.5],
            face: vec![None, None],
            faces: Vec::new(),
        };
        let lines = wrap_block_text_scaled(
            &face,
            "图，",
            size,
            0.01,
            &face,
            size,
            &[],
            0.0,
            0.0,
            Some(&cjk),
        );
        assert!(!lines.is_empty());
    }

    /// Mixed-size lines place and space by their runs (Greptile on PR
    /// #350): a right-aligned line with a smaller tail ends at the right
    /// edge, a 30 pt run pushes the next line down, a superscript run
    /// draws at its reduced size, and a bulleted line keeps its runs.
    #[test]
    fn mixed_size_runs_place_space_and_script() {
        let tail = |z: &str| {
            emit_runs(
                "<cp IX='0'/>right aligned <cp IX='1'/>tail",
                &[(0, &[]), (1, &[("Size", z), ("Color", "#ff0000")])],
                &[],
                &[("HorzAlign", "2")],
            )
        };
        let x0 = |svg: &str| tspan_xy(svg)[0].0;
        assert!(
            x0(&tail("0.1111")) > x0(&tail("0.1389")) + 1e-3,
            "a smaller tail starts further right"
        );
        let tall = |z: &str| {
            emit_runs(
                "<cp IX='0'/>first\n<cp IX='1'/>big<cp IX='2'/>\nthird",
                &[
                    (0, &[]),
                    (1, &[("Size", z), ("Color", "#ff0000")]),
                    (2, &[]),
                ],
                &[],
                &[],
            )
        };
        let gap = |svg: &str| {
            let ys: Vec<f64> = tspan_xy(svg).iter().map(|p| p.1).collect();
            ys[ys.len() - 1] - ys[0]
        };
        assert!(
            gap(&tall("0.4167")) > gap(&tall("0.1389")) + 0.2,
            "a 30 pt line takes 30 pt of height"
        );
        let sup = emit_runs(
            "<cp IX='0'/>H<cp IX='1'/>2",
            &[(0, &[("Pos", "1")]), (1, &[("Pos", "1"), ("Size", "0.2")])],
            &[],
            &[],
        );
        assert!(!sup.contains(r#"font-size="0.2000""#), "{sup}");
        let bullet = emit_runs(
            "<cp IX='0'/>bulleted <cp IX='1'/>red",
            &[(0, &[]), (1, &[("Color", "#ff0000")])],
            &[],
            &[("Bullet", "1")],
        );
        assert!(bullet.contains(r##"fill="#ff0000""##), "{bullet}");
    }

    /// A document-controlled `IX` does not size an allocation (Greptile
    /// on PR #350): only referenced rows get a style.
    #[test]
    fn huge_character_ix_values_stay_small() {
        let svg = emit_runs(
            "<cp IX='0'/>plain <cp IX='1000000000'/>red",
            &[(0, &[]), (1_000_000_000, &[("Color", "#ff0000")])],
            &[],
            &[],
        );
        assert!(svg.contains(r##"fill="#ff0000""##), "{svg}");
    }

    /// Legacy fill patterns (Visio 2021 witness t12): 2–7 draw 1/96 in
    /// lines at a 1/12 in period in FillForegnd over FillBkgnd, clipped
    /// to the outline; dither patterns mix the two colors.
    #[test]
    fn hatch_patterns_draw_lines_over_the_background() {
        let render = |pat: &str| {
            let mut s = rect_shape(1, (2.0, 2.0));
            s.sheet.set_cell("FillForegnd", Cell::value("#0000ff"));
            s.sheet.set_cell("FillBkgnd", Cell::value("#ffff00"));
            s.sheet.set_cell("FillPattern", Cell::value(pat));
            let (doc, page) = page_fixture(vec![s]);
            render_page(&doc, &page).unwrap()
        };
        let horiz = render("6");
        assert!(
            horiz.contains("<clipPath")
                && horiz.contains(r##"stroke="#0000ff" stroke-width="0.01042""##),
            "{horiz}"
        );
        assert!(horiz.contains(r##"fill="#ffff00""##));
        let dither = render("9");
        assert!(dither.contains(r##"fill="#808080""##), "{dither}");
    }

    /// An Ellipse row joins the shape's even-odd fill (Visio 2021
    /// witness, t07): a rectangle section plus an ellipse section is a
    /// square with a round hole — one fill path carrying both contours,
    /// no separately filled <ellipse> on top.
    #[test]
    fn ellipse_row_joins_the_even_odd_fill() {
        let mut s = rect_shape(1, (2.0, 2.0));
        s.sheet.set_cell("FillForegnd", Cell::value("#ff0000"));
        let mut sec = Section {
            name: "Geometry".into(),
            ix: 1,
            ..Section::default()
        };
        sec.rows.push(geo_row(
            1,
            "Ellipse",
            &[
                ("X", 0.5),
                ("Y", 0.5),
                ("A", 0.8),
                ("B", 0.5),
                ("C", 0.5),
                ("D", 0.8),
            ],
        ));
        s.sheet.children.push(SheetChild::Section(sec));
        let (doc, page) = page_fixture(vec![s]);
        let svg = render_page(&doc, &page).unwrap();
        assert!(!svg.contains("<ellipse"), "{svg}");
        let fill = svg
            .lines()
            .find(|l| l.contains(r#"fill-rule="evenodd""#))
            .expect("one even-odd fill path");
        assert_eq!(
            fill.matches('M').count(),
            2,
            "rect + ellipse contours: {fill}"
        );
        assert_eq!(fill.matches(" A ").count(), 2, "two half arcs: {fill}");
    }

    /// Rounding (Visio 2021 witness, t06): every corner of a straight
    /// walk becomes a tangent arc of the cell's radius, clamped to half
    /// the shorter leg; curved paths keep their authored shape.
    #[test]
    fn rounding_rounds_polyline_corners() {
        let sq = "M 0 0 L 2 0 L 2 -1 L 0 -1 L 0 0 ";
        let r = round_corners(sq, 0.25).expect("straight walk rounds");
        assert_eq!(r.matches(" A 0.25000 0.25000").count(), 4, "{r}");
        assert!(
            r.starts_with("M 0.25000 0.00000") && r.ends_with("Z "),
            "{r}"
        );
        // A radius larger than half the short leg clamps to it.
        let big = round_corners(sq, 5.0).unwrap();
        assert!(big.contains(" A 0.50000 0.50000"), "{big}");
        // An open walk keeps its endpoints and rounds the interior.
        let open = round_corners("M 0 0 L 1 0 L 1 -1 ", 0.25).unwrap();
        assert!(open.starts_with("M 0.00000 0.00000") && open.ends_with("L 1.00000 -1.00000 "));
        assert_eq!(open.matches(" A ").count(), 1);
        assert!(round_corners("M 0 0 C 1 1 2 2 3 3 ", 0.25).is_none());
        // Through the renderer: the cell reaches the stroked outline.
        let mut s = rect_shape(1, (2.0, 2.0));
        s.sheet.set_cell("Rounding", Cell::value("0.125"));
        let (doc, page) = page_fixture(vec![s]);
        let svg = render_page(&doc, &page).unwrap();
        assert_eq!(
            svg.matches(" A 0.12500 0.12500").count(),
            8,
            "fill + stroke"
        );
    }

    /// A spline samples into `L` segments but is a curve: a positive
    /// `Rounding` leaves it as drawn (Greptile on PR #344).
    #[test]
    fn rounding_leaves_sampled_splines_alone() {
        let render = |rounding: Option<&str>| {
            let mut s = shape_at(1, (2.0, 2.0), 4.0, 2.0, 0.0);
            push_geo(
                &mut s,
                vec![
                    geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                    geo_row(
                        2,
                        "SplineStart",
                        &[
                            ("X", 1.0),
                            ("Y", 1.5),
                            ("A", 0.3),
                            ("B", 0.0),
                            ("C", 1.0),
                            ("D", 3.0),
                        ],
                    ),
                    geo_row(3, "SplineKnot", &[("X", 2.5), ("Y", 0.4), ("A", 0.6)]),
                    geo_row(4, "SplineKnot", &[("X", 4.0), ("Y", 2.0), ("A", 1.0)]),
                ],
            );
            if let Some(r) = rounding {
                s.sheet.set_cell("Rounding", Cell::value(r));
            }
            let (doc, page) = page_fixture(vec![s]);
            render_page(&doc, &page).unwrap()
        };
        assert_eq!(render(Some("0.125")), render(None));
    }

    /// A formula-only `Rounding` on an instance of a resized master
    /// evaluates with the INSTANCE's size: `Width/10` on a 4 in wide
    /// instance is 0.4, not the 1 in master's 0.1 (Greptile on PR #344).
    #[test]
    fn rounding_formulas_use_the_instance_size() {
        // A formula-only cell on the instance, and the master's cell with
        // the master's cache `V='0.1'` (Greptile on PR #344, twice): both
        // evaluate with the instance's size.
        for on_master in [false, true] {
            let mut master = rect_shape(1, (1.0, 1.0));
            master.sheet.set_cell("Width", Cell::value("1"));
            master.sheet.set_cell("Height", Cell::value("1"));
            let mut inst = shape_at(1, (4.0, 5.0), 4.0, 4.0, 0.0);
            inst.master = Some(7);
            let cell = Cell {
                v: on_master.then(|| "0.1".to_string()),
                f: Some("Width/10".into()),
                ..Cell::default()
            };
            if on_master {
                master.sheet.set_cell("Rounding", cell);
            } else {
                inst.sheet.set_cell("Rounding", cell);
            }
            let (mut doc, page) = page_fixture(vec![inst]);
            doc.masters.push(vsdx::model::Master {
                id: 7,
                shapes: vec![master],
                ..vsdx::model::Master::default()
            });
            let svg = render_page(&doc, &page).unwrap();
            assert!(
                svg.contains(" A 0.40000 0.40000"),
                "on_master={on_master}: {svg}"
            );
            assert!(
                !svg.contains(" A 0.10000 0.10000"),
                "on_master={on_master}: {svg}"
            );
        }
    }

    /// A deleted spline row draws nothing and does not block the
    /// rounding of the straight walk that remains (Greptile on PR #344).
    #[test]
    fn deleted_curve_rows_do_not_block_rounding() {
        let mut s = rect_shape(1, (2.0, 2.0));
        s.sheet.set_cell("Rounding", Cell::value("0.125"));
        let mut gone = geo_row(6, "SplineKnot", &[("X", 0.5), ("Y", 0.5), ("A", 1.0)]);
        gone.attrs.push(("Del".into(), "1".into()));
        if let Some(SheetChild::Section(sec)) = s
            .sheet
            .children
            .iter_mut()
            .find(|c| matches!(c, SheetChild::Section(sec) if sec.name == "Geometry"))
        {
            sec.rows.push(gone);
        }
        let (doc, page) = page_fixture(vec![s]);
        let svg = render_page(&doc, &page).unwrap();
        assert!(svg.contains(" A 0.12500 0.12500"), "{svg}");
    }

    /// Stock monospace faces fall back to the vendored monospace
    /// Liberation Mono, never the proportional Carlito.
    #[test]
    fn monospace_stock_fonts_stay_monospace() {
        for f in ["Lucida Console", "Consolas", "Courier New"] {
            let chain = family_chain(Some(f));
            assert_eq!(chain[1], "Liberation Mono", "{f}: {chain:?}");
            assert_eq!(chain.last().map(String::as_str), Some("monospace"));
        }
        // Not a stock Visio font: it lays out as Calibri like any
        // unknown family (Greptile on PR #332 found its old mapping
        // unreachable).
        let chain = family_chain(Some("Lucida Sans Typewriter"));
        assert_eq!(chain[0], "Calibri", "{chain:?}");
    }

    /// A master size formula the instance cannot evaluate keeps the
    /// master's cached value, whether the instance omits the whole row
    /// or only the cell (Greptile on PR #343).
    #[test]
    fn unevaluable_master_formulas_stay_unscaled_on_both_paths() {
        let render = |omit_row: bool| {
            let mut x = geo_row(2, "LineTo", &[("Y", 0.0)]);
            x.cells.push((
                "X".into(),
                Cell {
                    v: Some("1.8".into()),
                    f: Some("Width-User.Missing".into()),
                    ..Cell::default()
                },
            ));
            let mut master = shape_at(1, (1.0, 0.5), 2.0, 1.0, 0.0);
            push_geo(
                &mut master,
                vec![
                    geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)]),
                    x,
                    geo_row(3, "LineTo", &[("X", 2.0), ("Y", 1.0)]),
                ],
            );
            let mut inst = shape_at(1, (4.0, 5.0), 4.0, 2.0, 0.0);
            inst.master = Some(7);
            let mut rows = vec![geo_row(1, "MoveTo", &[("X", 0.0), ("Y", 0.0)])];
            if !omit_row {
                rows.push(geo_row(2, "LineTo", &[("Y", 0.0)]));
            }
            rows.push(geo_row(3, "LineTo", &[("X", 4.0), ("Y", 2.0)]));
            push_geo(&mut inst, rows);
            let (mut doc, page) = page_fixture(vec![inst]);
            doc.masters.push(vsdx::model::Master {
                id: 7,
                shapes: vec![master],
                ..vsdx::model::Master::default()
            });
            render_page(&doc, &page).unwrap()
        };
        for omit_row in [true, false] {
            let svg = render(omit_row);
            assert!(
                svg.contains("L 1.80000 ") && !svg.contains("L 3.60000 "),
                "omit_row={omit_row}: {svg}"
            );
        }
    }

    /// Inherited master cells in local geometry rows (the network LLD's
    /// stretched U-Turn arrows): only a whole-identifier Width/Height
    /// reference scales with the instance.
    #[test]
    fn own_size_references_are_whole_identifiers() {
        assert!(refs_own_size("Width*0.5"));
        assert!(refs_own_size("Height-User.BarWidth"));
        assert!(refs_own_size("MIN(Width,Height)"));
        assert!(!refs_own_size("User.BarWidth"));
        assert!(!refs_own_size("Sheet.5!Width*1"));
        assert!(!refs_own_size("TxtHeight*0.5"));
        assert!(!refs_own_size("Controls.Row_1"));
    }

    /// A master's `LineCap` formula evaluates with the instance's size,
    /// not the master's cached value (Greptile on PR #342): `Width`
    /// cached as 1 (flat) on a 1 in master is 2 (extended) on a 2 in
    /// instance.
    #[test]
    fn line_cap_formulas_use_the_instance() {
        let mut master = rect_shape(1, (1.0, 1.0));
        master.sheet.set_cell("Width", Cell::value("1"));
        master.sheet.set_cell("Height", Cell::value("1"));
        master.sheet.set_cell(
            "LineCap",
            Cell {
                v: Some("1".into()),
                f: Some("Width".into()),
                ..Cell::default()
            },
        );
        let mut inst = shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0);
        inst.master = Some(7);
        let (mut doc, page) = page_fixture(vec![inst]);
        doc.masters.push(vsdx::model::Master {
            id: 7,
            shapes: vec![master],
            ..vsdx::model::Master::default()
        });
        let svg = render_page(&doc, &page).unwrap();
        assert!(svg.contains(r#"stroke-linecap="square""#), "{svg}");
        assert!(!svg.contains(r#"stroke-linecap="butt""#), "{svg}");
    }

    /// LineCap (Visio 2021 witness, b04): 0 round ends and corners
    /// (the default), 1 flat, 2 square-extended; a walk that returns to
    /// its start closes with `Z` so the start corner joins too.
    #[test]
    fn line_cap_and_closed_corner() {
        assert!(cap_attr(0, false).contains(r#"stroke-linecap="round""#));
        assert!(cap_attr(1, false).contains(r#"stroke-linecap="butt""#));
        assert!(cap_attr(2, false).contains(r#"stroke-linecap="square""#));
        assert!(
            !cap_attr(1, true).contains("linecap"),
            "dashes keep #235's caps"
        );
        // The join still follows LineCap when dashed (CodeRabbit on PR
        // #342).
        assert!(cap_attr(1, true).contains(r#"stroke-linejoin="miter""#));
        assert!(cap_attr(2, true).contains(r#"stroke-linejoin="miter""#));
        assert!(cap_attr(0, true).contains(r#"stroke-linejoin="round""#));
        assert!(close_returning_path("M 0 0 L 1 0 L 1 1 L 0 0").ends_with(" Z"));
        assert_eq!(
            close_returning_path("M 0 0 L 1 0 L 1 1"),
            "M 0 0 L 1 0 L 1 1"
        );
        let mut s = rect_shape(1, (2.0, 2.0));
        s.sheet.set_cell("LineCap", Cell::value("2"));
        let (doc, page) = page_fixture(vec![s]);
        let svg = render_page(&doc, &page).unwrap();
        assert!(svg.contains(r#"Z" fill="none""#), "{svg}");
        assert!(svg.contains(r#"stroke-linecap="square""#), "{svg}");
    }

    /// GitHub #22: the shared TextBlock resolution must agree with the
    /// defaults `emit_text` has always used — absent `Txt*` cells pin
    /// the block at the shape's center, size it to the shape, and take
    /// the Visio built-ins (10 pt, black, centered, 0.1/0.0555 in
    /// insets).
    #[test]
    fn text_block_defaults_match_emit_text() {
        let (doc, page) = page_fixture(vec![shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0)]);
        let tb = text_block(&doc, &page, 1).unwrap();
        assert_eq!(tb.pin, (1.0, 0.5), "pin defaults to the shape's center");
        assert_eq!(tb.size, (2.0, 1.0), "size defaults to Width/Height");
        assert_eq!(tb.loc_pin, (1.0, 0.5));
        assert_eq!(tb.angle, 0.0);
        assert!((tb.font_size - 0.13888).abs() < 1e-9, "10 pt default");
        assert_eq!(tb.color, "#000000");
        assert!(!tb.bold && !tb.italic);
        assert_eq!(tb.halign, 1, "Visio default HorzAlign = center");
        assert_eq!(tb.valign, 2, "middle");
        assert_eq!(tb.insets, (0.1, 0.1, 0.0555, 0.0555));
        assert!((tb.line_height - tb.font_size * 1.2).abs() < 1e-12);
        assert_eq!(tb.font, None);
    }

    /// Explicit `Txt*` cells and Character/Paragraph styling resolve
    /// through the chain (GitHub #22 — the editor overlay and the
    /// renderer read the same values).
    #[test]
    fn text_block_reads_txt_cells_and_style() {
        let mut sh = shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0);
        for (n, v) in [
            ("TxtPinX", 1.5),
            ("TxtPinY", 0.25),
            ("TxtWidth", 1.0),
            ("TxtHeight", 0.5),
            ("TxtLocPinX", 0.5),
            ("TxtLocPinY", 0.25),
        ] {
            sh.sheet.set_cell(n, Cell::value(format!("{v}")));
        }
        sh.sheet.children.push(SheetChild::Section(Section {
            name: "Character".into(),
            rows: vec![Row {
                cells: vec![
                    ("Font".into(), Cell::value("Arial")),
                    ("Size".into(), Cell::value("0.27778")), // 20 pt
                    ("Color".into(), Cell::value("#FF0000")),
                    ("Style".into(), Cell::value("1")), // bold
                ],
                ..Row::default()
            }],
            ..Section::default()
        }));
        sh.sheet.children.push(SheetChild::Section(Section {
            name: "Paragraph".into(),
            rows: vec![Row {
                cells: vec![("HorzAlign".into(), Cell::value("2"))],
                ..Row::default()
            }],
            ..Section::default()
        }));
        let (doc, page) = page_fixture(vec![sh]);
        let tb = text_block(&doc, &page, 1).unwrap();
        assert_eq!(tb.pin, (1.5, 0.25));
        assert_eq!(tb.size, (1.0, 0.5));
        assert_eq!(tb.loc_pin, (0.5, 0.25));
        assert_eq!(tb.font.as_deref(), Some("Arial"));
        assert!((tb.font_size - 0.27778).abs() < 1e-12);
        assert_eq!(tb.color, "#FF0000");
        assert!(tb.bold && !tb.italic);
        assert_eq!(tb.halign, 2);
    }

    /// The block's PAGE rect (the editor overlay's footprint): a
    /// default block on an unrotated top-level shape is the shape's
    /// own rect; an explicit smaller block lands pin-anchored
    /// (GitHub #22).
    #[test]
    fn text_block_page_rect_maps_local_block() {
        let (doc, page) = page_fixture(vec![shape_at(1, (4.0, 5.0), 2.0, 1.0, 0.0)]);
        let (x, y, w, h) = text_block_page_rect(&doc, &page, 1).unwrap();
        // Shape rect: pin (4,5) center → (3, 4.5)..(5, 5.5), y-up.
        assert!(
            (x - 3.0).abs() < 1e-9 && (y - 4.5).abs() < 1e-9,
            "x={x} y={y}"
        );
        assert!((w - 2.0).abs() < 1e-9 && (h - 1.0).abs() < 1e-9);

        // Off-center pin: TxtPin (1.5, 0.25) with a centered 1×0.5
        // block → shape-local (1.0, 0.0)..(2.0, 0.5) → page
        // (4.0, 4.5)..(5.0, 5.0) (pin − LocPin = (3.0, 4.5) offset).
        let mut sh = shape_at(2, (4.0, 5.0), 2.0, 1.0, 0.0);
        for (n, v) in [
            ("TxtPinX", 1.5),
            ("TxtPinY", 0.25),
            ("TxtWidth", 1.0),
            ("TxtHeight", 0.5),
        ] {
            sh.sheet.set_cell(n, Cell::value(format!("{v}")));
        }
        let (doc, page) = page_fixture(vec![sh]);
        let (x, y, w, h) = text_block_page_rect(&doc, &page, 2).unwrap();
        assert!(
            (x - 4.0).abs() < 1e-9 && (y - 4.5).abs() < 1e-9,
            "x={x} y={y}"
        );
        assert!((w - 1.0).abs() < 1e-9 && (h - 0.5).abs() < 1e-9);
    }

    /// Count the emitted text lines (tspans) of one shape.
    fn text_lines(out: &str) -> Vec<String> {
        out.split("<tspan")
            .skip(1)
            .map(|l| {
                l.split('>')
                    .nth(1)
                    .unwrap_or("")
                    .split('<')
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    /// A shape carrying the utilisation.vsdx specimen 1065's exact
    /// text-block cells (#21): TxtWidth 0.8159 in, 8 pt (0.1111 in)
    /// text, zero side margins (inherited from the connector master)
    /// — the authored three-line label the 0.5 em guess re-wrapped
    /// into four (15 chars > its 14-char cap at that width).
    fn shape_1065() -> Shape {
        let mut s = Shape::default();
        s.sheet.text = Some("If allograft\r\nIs implanted or\r\n opened".into());
        // The specimen's connector master inherits zero left/right
        // margins, so the usable width IS the block width.
        for (n, v) in [
            ("Width", "2.0"),
            ("Height", "1.0"),
            ("TxtWidth", "0.8159"),
            ("TxtHeight", "0.6"),
            ("LeftMargin", "0"),
            ("RightMargin", "0"),
        ] {
            s.sheet.set_cell(n, Cell::value(v));
        }
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Character".into(),
            rows: vec![Row {
                cells: vec![("Size".into(), Cell::value("0.1111"))],
                ..Row::default()
            }],
            ..Section::default()
        }));
        s
    }

    /// #21: measured wrap keeps the authored line "Is implanted or"
    /// on one line — real per-glyph advances at 8 pt measure ≈0.70 in
    /// against the 0.8159 in block, where the 0.5 em guess capped the
    /// line at 14 characters and split it. Since #144 the third line's
    /// leading space survives too.
    #[test]
    fn measured_wrap_keeps_the_utilisation_1065_lines() {
        let doc = Document::default();
        let s = shape_1065();
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        let lines = text_lines(&out);
        assert_eq!(
            lines,
            vec!["If allograft", "Is implanted or", " opened"],
            "the three authored lines survive character-exact: {out}"
        );
    }

    /// #144: lines are character-exact — a paragraph's leading space
    /// run survives (first paragraph included: only the trailing end
    /// of the block is trimmed), interior runs stay, a whitespace-only
    /// paragraph emits its spaces, an empty paragraph stays an empty
    /// line, and the emitted `<text>` carries xml:space="preserve" so
    /// the rasterizer keeps the spacing.
    #[test]
    fn wrap_is_character_exact_on_spaces() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.set_cell("Width", Cell::value("2.0"));
        s.sheet.set_cell("TxtWidth", Cell::value("2.0"));
        {
            let chain = [&s];
            let mut out = String::new();
            emit_text(
                &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
            );
            assert!(out.is_empty(), "no text, no text element: {out}");
        }

        // First-paragraph leading run, interior run, whitespace-only
        // paragraph, empty paragraph. The final paragraph's trailing
        // space is trimmed with the block's end (corpus texts end with
        // CRLF; that trailing break must not grow the line count).
        s.sheet.text = Some("  opened\r\na  b\n   \n\nend ".into());
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        assert_eq!(
            text_lines(&out),
            vec!["  opened", "a  b", "   ", "", "end"],
            "every authored space survives into its line: {out}"
        );
        assert!(
            out.contains(r#"<text text-anchor="middle" xml:space="preserve""#),
            "the text element carries xml:space=preserve: {out}"
        );
    }

    /// #257 text-direction: the paragraph base derivation — explicit
    /// U+202A/U+202B overrides decide before the first strong
    /// character; neutrals and numbers never decide.
    #[test]
    fn paragraph_base_follows_marks_then_first_strong() {
        assert!(!paragraph_base_rtl("hello world"), "latin → LTR");
        assert!(
            paragraph_base_rtl("\u{0633}\u{0644}\u{0627}"),
            "arabic → RTL"
        );
        assert!(paragraph_base_rtl("\u{202B}hello"), "RLE override → RTL");
        assert!(
            !paragraph_base_rtl("\u{202A}\u{0633}\u{0644}\u{0627}"),
            "LRE overrides arabic"
        );
        assert!(
            !paragraph_base_rtl("123 (x)"),
            "numbers+neutrals → LTR default"
        );
        assert!(!paragraph_base_rtl("   "), "all-neutral → LTR default");
    }

    /// PR #269 review: RTL paragraphs measure with the RUN-PAIRED
    /// faces (see textmetrics::split_runs) — in this test binary the
    /// vendored fallbacks are deliberately unregistered (deterministic
    /// pins), so every paragraph measures as ONE run through the same
    /// single-face path as before: the wrap's output for pure-Latin
    /// and pure-Persian paragraphs is unchanged. (#282 grew the
    /// fallback to the Vazirmatn→Noto chain; the assertion covers the
    /// whole chain at once.)
    #[test]
    fn rtl_wrap_stays_single_face_without_a_registered_fallback() {
        let latin = crate::textmetrics::resolve(None, false, false);
        let fallback = crate::textmetrics::resolve(Some("Vazirmatn"), false, false);
        let noto = crate::textmetrics::resolve(Some("Noto Sans SC"), false, false);
        let alef = '\u{0627}';
        assert!(
            !fallback.covers(alef),
            "unregistered vazirmatn covers nothing"
        );
        let chain: Vec<&crate::textmetrics::ResolvedFace> = vec![&fallback, &noto];
        let fa = "\u{0633}\u{0644}\u{0627}\u{0645}";
        for para in [format!("\u{202B}{fa}"), format!("\u{202B}abc {fa}")] {
            let lines = wrap_block_text(
                &latin, &para, 0.13889, 2.0, &latin, 0.13889, &chain, 0.0, 0.0,
            );
            assert_eq!(lines.len(), 1);
            assert!(lines[0].rtl, "the base is rtl");
            // No covering fallback → one run → width equals the plain
            // single-face measurement the wrap always used.
            let w = lines[0].text.clone();
            let expect = latin.width_in(&w, 0.13889);
            let got = latin.width_in_runs(&chain, &w, 0.13889);
            assert!(
                (expect - got).abs() < 1e-9,
                "single-run text keeps byte-identical widths: {expect} vs {got}"
            );
        }
    }

    /// #257 text-direction (PR #269 review): the Right-to-Left Text
    /// toggle round-trips — on marks every paragraph, off strips the
    /// marks AND pins still-RTL paragraphs with an explicit LRE so
    /// "off" is really LTR (the trim-then-ask direction test in the
    /// old dc-ui toggle could never see its own mark).
    #[test]
    fn rtl_toggle_round_trips_and_pins_natural_rtl_ltr() {
        // Latin: on marks every paragraph (CRLF normalizes), off
        // restores the original bytes.
        let on = toggle_rtl_marks("ab\r\ncd", true);
        assert_eq!(on, "\u{202B}ab\n\u{202B}cd");
        assert_eq!(toggle_rtl_marks(&on, false), "ab\ncd");
        // Naturally-RTL text: off strips the RLE and pins an LRE —
        // without the pin the first-strong scan would keep it RTL and
        // the toggle would be a visual no-op.
        let fa = "\u{0633}\u{0644}\u{0627}\u{0645}";
        let marked = format!("\u{202B}{fa}");
        assert_eq!(toggle_rtl_marks(&marked, false), format!("\u{202A}{fa}"));
        // Toggling back on replaces the pin with the RLE.
        assert_eq!(
            toggle_rtl_marks(&format!("\u{202A}{fa}"), true),
            format!("\u{202B}{fa}")
        );
        // An empty paragraph never gains a pin when switching off.
        assert_eq!(toggle_rtl_marks("\u{202B}", false), "");
        // An EMPTY shape pre-sets its direction: the bare mark is the
        // whole text and the first typed paragraph inherits the base.
        assert_eq!(toggle_rtl_marks("", true), "\u{202B}");
        assert_eq!(toggle_rtl_marks("", false), "");
        // The trailing terminator's empty paragraph takes NO mark —
        // a marked empty tail is content to the wrapper and would add
        // a TEXTHEIGHT line to "hello\r\n" (#148 gold, PR #269 review).
        assert_eq!(toggle_rtl_marks("hello\r\n", true), "\u{202B}hello\n");
        assert_eq!(
            toggle_rtl_marks("\u{202B}hello\n", false),
            "hello\n",
            "off restores the plain trailing break"
        );
    }

    /// #257 text-direction: a paragraph whose base is RTL wraps with
    /// the complex-script pair and emits the isolate-wrapped line at
    /// its visual right edge (start alignment), with direction=ltr
    /// pinned on the element.
    #[test]
    fn rtl_paragraph_wraps_and_emits_isolated_at_visual_right() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.set_cell("Width", Cell::value("3.0"));
        s.sheet.set_cell("TxtWidth", Cell::value("3.0"));
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Paragraph".into(),
            rows: vec![Row {
                ix: 0,
                cells: vec![("HorzAlign".into(), Cell::value("0"))], // start = reading order
                ..Row::default()
            }],
            ..Section::default()
        }));
        s.sheet.text = Some(
            "\u{0633}\u{0644}\u{0627}\u{0645} \u{0639}\u{0644}\u{06CC}\u{06A9}\u{0645}".into(),
        );
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 3.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        // The isolate pair wraps the line content.
        assert!(
            out.contains('\u{2067}'),
            "the RTL line carries an RLI: {out}"
        );
        assert!(
            out.contains('\u{2069}'),
            "the RTL line carries a PDI: {out}"
        );
        // direction=ltr pins the element base for attribute-honoring viewers.
        assert!(
            out.contains(r#"direction="ltr""#),
            "element base pinned: {out}"
        );
        // Start alignment under an RTL base puts the line's visual
        // left edge at (right edge - shaped width), NOT at left_x —
        // the tspan x must sit right of the block's left inset edge.
        let x: f64 = out
            .split(r#"<tspan x=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a tspan x");
        // Block pin (1.5,0.5) centered: left edge = tx - lpx + ins_l
        // = 1.5 - 1.5 + 0.1 = 0.1; the RTL line's visual left must be
        // strictly inside the block (right_x - lw > 0.1).
        assert!(x > 0.1, "RTL start line sits at its visual right: x={x}");
        // A second shape with the SAME text but end alignment (2)
        // anchors the line at the block's LEFT edge instead.
        if let Some(SheetChild::Section(sec)) = s.sheet.children.last_mut() {
            sec.rows[0].cells[0] = ("HorzAlign".into(), Cell::value("2"));
        }
        let chain = [&s];
        let mut out2 = String::new();
        emit_text(
            &doc, &s, &chain, 3.0, 1.0, 1.0, 1.0, false, None, None, &mut out2,
        );
        let x2: f64 = out2
            .split(r#"<tspan x=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a tspan x");
        assert!(
            (x2 - 0.1).abs() < 1e-6,
            "RTL end line anchors at the visual left inset edge: x2={x2}"
        );
        // PR #269 review: the end-aligned line carries its OWN start
        // anchor (the inherited element "end" would push the glyphs
        // left of the block), and the tspan's font-family is the
        // complete attribute — the old double wrapper nested the
        // quotes. Both are pinned by parsing the emission through the
        // same usvg the rasterizer runs.
        assert!(
            out2.contains(r#" text-anchor="start""#),
            "the end-aligned RTL line anchors itself start: {out2}"
        );
        let full = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10" viewBox="0 0 10 10">{out2}</svg>"#
        );
        // usvg's roxmltree stage rejects malformed attributes — the
        // nested-quote font-family this fixes failed here.
        resvg::usvg::Tree::from_str(&full, &resvg::usvg::Options::default())
            .unwrap_or_else(|e| panic!("RTL emission parses as well-formed SVG: {e}\n{full}"));
    }

    /// #257 text-direction (PR #269 review): RTL lines under a larger
    /// ComplexScriptSize take proportionally taller slots — with
    /// default 1.2 em spacing and 2 em glyphs, the flat line height
    /// overlapped adjacent lines.
    #[test]
    fn larger_complex_script_lines_take_taller_slots() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.set_cell("Width", Cell::value("3.0"));
        s.sheet.set_cell("TxtWidth", Cell::value("3.0"));
        s.sheet.set_cell("TxtHeight", Cell::value("1.2"));
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Character".into(),
            rows: vec![Row {
                ix: 0,
                cells: vec![("ComplexScriptSize".into(), Cell::value("-200"))],
                ..Row::default()
            }],
            ..Section::default()
        }));
        // Two RTL paragraphs (the explicit mark decides the base).
        let fa = "\u{0633}\u{0644}\u{0627}\u{0645}";
        s.sheet.text = Some(format!("{fa}\r\n{fa}"));
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 3.0, 1.2, 1.5, 0.6, false, None, None, &mut out,
        );
        let ys: Vec<f64> = out
            .split(r#" y=""#)
            .skip(1)
            .filter_map(|rest| rest.split('"').next().and_then(|v| v.parse().ok()))
            .collect();
        assert_eq!(ys.len(), 2, "two rtl lines: {out}");
        let (tb, _) = {
            let chain = [&s];
            let tb = resolve_text_block(&doc, &s, &chain, 3.0, 1.2);
            (tb, chain)
        };
        let slot = line_slot_height(&tb, true);
        let naive = tb.line_height;
        assert!(
            slot > naive * 1.5,
            "the double-size cs glyphs take a taller slot: {slot} vs {naive}"
        );
        assert!(
            (ys[1] - ys[0] - slot).abs() < 2e-3,
            "the second line advances by the taller slot: {} vs {slot}",
            ys[1] - ys[0]
        );
        // TEXTHEIGHT agrees with the painted slots (the refreshed
        // cache feeds TxtHeight the same sum).
        let m = crate::textfunc::SheetTextMeasure::new(&doc, &s, &[&s], format!("{fa}\r\n{fa}"));
        let h = {
            use vsdx::formula::TextMeasure as _;
            m.text_height(&format!("{fa}\r\n{fa}"), 3.0).unwrap()
        };
        let ins_tb = tb.insets.2 + tb.insets.3;
        assert!(
            (h - (2.0 * slot + ins_tb)).abs() < 1e-3,
            "TEXTHEIGHT sums the slots (+ insets): {h} vs {}",
            2.0 * slot + ins_tb
        );
    }

    /// PR #269 review: a SMALLER Latin line followed by a larger
    /// complex-script line — the larger line's baseline must rise by
    /// its own ascent, not advance by the Latin slot (which overlapped
    /// the Latin glyphs).
    #[test]
    fn mixed_latin_then_larger_cs_lines_keep_their_ascents() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.set_cell("Width", Cell::value("3.0"));
        s.sheet.set_cell("TxtWidth", Cell::value("3.0"));
        s.sheet.set_cell("TxtHeight", Cell::value("1.4"));
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Character".into(),
            rows: vec![Row {
                ix: 0,
                cells: vec![("ComplexScriptSize".into(), Cell::value("-200"))],
                ..Row::default()
            }],
            ..Section::default()
        }));
        let fa = "\u{0633}\u{0644}\u{0627}\u{0645}";
        s.sheet.text = Some(format!("one\r\n\u{202B}{fa}"));
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 3.0, 1.4, 1.5, 0.7, false, None, None, &mut out,
        );
        let ys: Vec<f64> = out
            .split(r#" y=""#)
            .skip(1)
            .filter_map(|r| r.split('"').next().and_then(|v| v.parse().ok()))
            .collect();
        assert_eq!(ys.len(), 2, "one latin + one rtl line: {out}");
        let tb = resolve_text_block(&doc, &s, &chain, 3.0, 1.4);
        let size = tb.font_size;
        let delta = ys[1] - ys[0];
        // Latin slot (1.2 em) + (cs ascent − latin ascent) = 2.0 em —
        // the old flat advance was the Latin slot's 1.2 em.
        assert!(
            (delta - 2.0 * size).abs() < 2e-3,
            "the larger line rises by its own ascent: {delta} vs {}",
            2.0 * size
        );
    }

    /// #257 text-direction: LTR-only blocks keep the pre-#257 byte
    /// form (no direction attribute, no isolates) — golden stability.
    #[test]
    fn ltr_blocks_emit_without_direction_marks() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.set_cell("Width", Cell::value("2.0"));
        s.sheet.set_cell("TxtWidth", Cell::value("2.0"));
        s.sheet.text = Some("plain latin".into());
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        assert!(!out.contains("direction="), "no direction attr: {out}");
        assert!(!out.contains('\u{2067}'), "no isolates: {out}");
    }

    /// #257 text-direction: ComplexScriptFont/Size resolve through the
    /// first Character row with the spec's zero/absent semantics —
    /// a CS face rides the RTL tspan's font-family, and its size swaps
    /// the glyph size when it differs.
    #[test]
    fn complex_script_cells_feed_the_rtl_line() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.set_cell("Width", Cell::value("3.0"));
        s.sheet.set_cell("TxtWidth", Cell::value("3.0"));
        {
            let chain = [&s];
            let tb = resolve_text_block(&doc, &s, &chain, 3.0, 1.0);
            assert_eq!(tb.cs_font, None, "absent cell → None");
            assert_eq!(tb.cs_size, None, "absent size → None (Size fallback)");
        }
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Character".into(),
            rows: vec![Row {
                ix: 0,
                cells: vec![
                    ("ComplexScriptFont".into(), Cell::value("Vazirmatn")),
                    ("ComplexScriptSize".into(), Cell::value("0.2")),
                ],
                ..Row::default()
            }],
            ..Section::default()
        }));
        let (font, size) = {
            let chain = [&s];
            let tb = resolve_text_block(&doc, &s, &chain, 3.0, 1.0);
            (tb.cs_font, tb.cs_size)
        };
        assert_eq!(font.as_deref(), Some("Vazirmatn"));
        assert_eq!(size, Some(0.2));
        // The negative-size semantics: <0 = |v|·Size/100.
        let size0 = {
            let chain = [&s];
            resolve_text_block(&doc, &s, &chain, 3.0, 1.0).font_size
        };
        if let Some(SheetChild::Section(sec)) = s.sheet.children.last_mut() {
            sec.rows[0].cells[1] = ("ComplexScriptSize".into(), Cell::value("-150"));
        }
        let size = {
            let chain = [&s];
            resolve_text_block(&doc, &s, &chain, 3.0, 1.0).cs_size
        };
        assert_eq!(size, Some(1.5 * size0), "negative → |v|·Size/100");
    }

    /// #144 review: when a block is too narrow for `"a "` and `" b"`,
    /// the wrap breaks inside the `"a  b"` space run — the surviving
    /// space is a line of its own, not an empty line and not a dropped
    /// character.
    #[test]
    fn narrow_wrap_breaking_in_a_space_run_keeps_the_space() {
        let doc = Document::default();
        let size = 0.13889; // the text-block default (10 pt)
        let face = crate::textmetrics::resolve(None, false, false);
        let usable = face.width_in("a", size) + 0.4 * face.width_in(" ", size);
        let mut s = Shape::default();
        s.sheet.text = Some("a  b".into());
        s.sheet.set_cell("Width", Cell::value("2.0"));
        s.sheet
            .set_cell("TxtWidth", Cell::value(format!("{usable:.6}")));
        s.sheet.set_cell("LeftMargin", Cell::value("0"));
        s.sheet.set_cell("RightMargin", Cell::value("0"));
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        assert_eq!(
            text_lines(&out),
            vec!["a", " ", "b"],
            "the surviving space is its own line: {out}"
        );
    }

    /// #21: a run that genuinely overflows still wraps — measurement
    /// replaces the guess, it does not disable wrapping.
    #[test]
    fn measured_wrap_splits_genuinely_overflowing_runs() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.text = Some("considerable label text beyond the block".into());
        s.sheet.set_cell("Width", Cell::value("0.8"));
        s.sheet.set_cell("Height", Cell::value("1.0"));
        s.sheet.set_cell("TxtWidth", Cell::value("0.8"));
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        let lines = text_lines(&out);
        assert!(lines.len() > 1, "must wrap: {lines:?}");
        // The greedy invariant: every line except the last is full —
        // adding the next word would overflow.
        let usable = 0.8 - 0.1 - 0.1;
        let face = crate::textmetrics::resolve(None, false, false);
        for pair in lines.windows(2) {
            let joined = format!("{} {}", pair[0], pair[1]);
            assert!(
                face.width_in(&joined, 0.13889) > usable,
                "line {:?} wrapped too early (usable {usable}): {lines:?}",
                pair[0]
            );
        }
    }

    /// #21: a single word wider than the block hard-splits at the
    /// longest fitting prefix (never a zero-length line, never a
    /// stuck loop).
    #[test]
    fn measured_wrap_hard_splits_single_long_words() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.text = Some("aaaaaaaaaaaaaaaaaaaaaaaaaa".into());
        s.sheet.set_cell("Width", Cell::value("0.5"));
        s.sheet.set_cell("TxtWidth", Cell::value("0.5"));
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        let lines = text_lines(&out);
        assert!(
            lines.len() > 1,
            "a 26-em-ish run cannot fit 0.3 in: {lines:?}"
        );
        assert!(
            lines.iter().all(|l| !l.is_empty()),
            "hard splits keep every line non-empty: {lines:?}"
        );
        let joined: String = lines.concat();
        assert_eq!(joined, "aaaaaaaaaaaaaaaaaaaaaaaaaa", "no characters lost");
    }

    /// The emergency hard-split's boundary rules run to a FIXPOINT:
    /// glue (U+00A0) may move the split past the position the
    /// forbidden-head pass already cleared, which used to strand a
    /// closer at the head of the rest — `图⍽，！色` must never open a
    /// line with `，` or `！` (CodeRabbit/Greptile round 5).
    #[test]
    fn emergency_split_never_opens_a_line_with_a_closer() {
        let face = crate::textmetrics::resolve(None, false, false);
        // Narrower than any glyph: every token takes the emergency
        // path.
        let lines = wrap_block_text(
            &face,
            "图\u{00A0}，！色",
            0.13889,
            0.05,
            &face,
            0.13889,
            &[],
            0.0,
            0.0,
        );
        let joined: String = lines.iter().map(|l| l.text.clone()).collect();
        assert_eq!(joined, "图\u{00A0}，！色", "no characters lost");
        assert!(lines.len() >= 2, "the emergency splitter fired");
        for l in &lines {
            let first = l.text.chars().next().expect("lines are non-empty");
            assert!(
                !crate::textmetrics::line_head_forbidden(first),
                "no line opens with a forbidden closer: {:?}",
                lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>()
            );
        }
    }

    /// The emergency splitter keeps a combining mark with its base:
    /// `カ` + U+3099 never splits between them, without consuming
    /// the following `ク` (CodeRabbit round 5).
    #[test]
    fn emergency_split_keeps_combining_marks_with_their_base() {
        let face = crate::textmetrics::resolve(None, false, false);
        let lines = wrap_block_text(
            &face,
            "カ\u{3099}ク",
            0.13889,
            0.05,
            &face,
            0.13889,
            &[],
            0.0,
            0.0,
        );
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["カ\u{3099}", "ク"],
            "the dakuten stays on its base; the break before ク survives"
        );
    }

    /// #282 ja (CodeRabbit round 2): the emergency splitter never
    /// leaves a STANDALONE sound mark (ﾞ ﾟ — Lm class, glued only by
    /// the kinsoku head rule) to open a line of over-long halfwidth
    /// kana.
    #[test]
    fn emergency_split_never_opens_a_line_with_a_sound_mark() {
        let face = crate::textmetrics::resolve(None, false, false);
        let lines = wrap_block_text(
            &face,
            "ｱｲｶ\u{FF9E}ｸｻ",
            0.13889,
            0.05,
            &face,
            0.13889,
            &[],
            0.0,
            0.0,
        );
        let joined: String = lines.iter().map(|l| l.text.clone()).collect();
        assert_eq!(joined, "ｱｲｶ\u{FF9E}ｸｻ", "no characters lost");
        assert!(lines.len() >= 2, "the emergency splitter fired");
        for l in &lines {
            let first = l.text.chars().next().expect("lines are non-empty");
            assert!(
                !crate::textmetrics::line_head_forbidden(first),
                "no line opens with a forbidden starter: {:?}",
                lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>()
            );
        }
    }

    /// CRLF and lone CR each break exactly one line (carried over
    /// from the guess era — the normalizer is unchanged).
    #[test]
    fn crlf_and_lone_cr_each_break_one_line() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.text = Some("one\r\ntwo\rthree".into());
        s.sheet.set_cell("Width", Cell::value("8.0"));
        s.sheet.set_cell("TxtWidth", Cell::value("8.0"));
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 8.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        assert_eq!(
            text_lines(&out),
            vec!["one", "two", "three"],
            "no empty paragraph between CRLF pairs"
        );
    }

    /// #22 review: the in-place editor anchors its caret on the
    /// renderer's own first-baseline formula — pin the numbers so the
    /// helper and `emit_text` can never drift (block 1 in tall, 10 pt
    /// text, default insets, two wrapped lines of 0.1667 in each).
    #[test]
    fn text_first_baseline_matches_the_valign_bands() {
        let mut tb = TextBlock::default();
        tb.size.1 = 1.0;
        tb.font_size = 0.13889;
        tb.insets = (0.1, 0.1, 0.0555, 0.0555);
        let total = 2.0 * 0.16667;
        let ascent = 0.13889 * 0.8;
        tb.valign = 1; // top
        assert!((text_first_baseline(&tb, total, tb.font_size) - (0.0555 + ascent)).abs() < 1e-9);
        tb.valign = 2; // middle (default)
        assert!(
            (text_first_baseline(&tb, total, tb.font_size) - ((1.0 - total) / 2.0 + ascent)).abs()
                < 1e-9
        );
        tb.valign = 3; // bottom
        assert!(
            (text_first_baseline(&tb, total, tb.font_size) - (1.0 - 0.0555 - total + ascent)).abs()
                < 1e-9
        );
        // More wrapped lines raise the first line in the middle band.
        let two = text_first_baseline(&tb, total, tb.font_size);
        let four = text_first_baseline(&tb, total * 2.0, tb.font_size);
        assert!(four < two, "taller text starts its first line higher");
        // PR #269 review: a first line of larger complex-script glyphs
        // ascends from a higher baseline, and its slot (not the flat
        // line height) is what the next line advances by.
        tb.line_height = 0.16667;
        tb.cs_size = Some(tb.font_size * 2.0);
        let eff = effective_cs_size(&tb);
        assert_eq!(eff, tb.font_size * 2.0);
        assert_eq!(line_slot_height(&tb, true), tb.line_height * 2.0);
        assert_eq!(
            line_slot_height(&tb, false),
            tb.line_height,
            "ltr keeps the latin slot"
        );
        // A SMALLER complex-script size never shrinks the slot.
        tb.cs_size = Some(tb.font_size * 0.5);
        assert_eq!(line_slot_height(&tb, true), tb.line_height);
    }

    /// GitHub #22's no-ghost guarantee: `render_page_no_text` drops
    /// exactly the named shape's text and keeps everything else.
    #[test]
    fn render_page_no_text_suppresses_only_that_shape() {
        let mut a = shape_at(1, (2.0, 8.0), 1.0, 1.0, 0.0);
        a.sheet.text = Some("alpha".into());
        let mut b = shape_at(2, (6.0, 8.0), 1.0, 1.0, 0.0);
        b.sheet.text = Some("beta".into());
        let (doc, page) = page_fixture(vec![a, b]);
        let full = render_page(&doc, &page).unwrap();
        assert!(full.contains("alpha") && full.contains("beta"));
        let skipped = render_page_no_text(&doc, &page, &[], 1).unwrap();
        assert!(!skipped.contains("alpha"), "edited shape's text is gone");
        assert!(skipped.contains("beta"), "other shapes keep their text");
        assert!(skipped.contains("<g transform"), "geometry keeps rendering");
    }

    /// #199 + #185: the gallery matches the 600 dpi Visio print (the
    /// authoritative oracle in Analysis/arrow-gallery-visio2016.md) —
    /// the open-head set 14–22 strokes instead of filling, the
    /// dimension set composes ticks with rings/dots, 12 is the single
    /// wide V while 43 is the double chevron, the rear arcs of
    /// 5/6/8/18 are circular, and the bare ends 46–50 have no head.
    #[test]
    fn arrow_gallery_matches_the_visio_print() {
        // The open heads 14–22: stroked paths, never filled.
        for kind in [14, 15, 16, 17, 18, 19, 20, 21, 22] {
            let Some(parts) = arrow_spec(kind) else {
                panic!("arrow {kind} should exist");
            };
            assert!(
                parts.iter().all(|p| !p.filled),
                "arrow {kind} is an open head"
            );
        }
        // 22 is the stroked diamond (4.5 × 2.25 units).
        let parts = arrow_spec(22).unwrap();
        assert!(
            parts[0].d.contains("L-2.25,-1.125 L-4.5,0"),
            "diamond profile"
        );
        // #239: 38's diamond STROKES (the 600 dpi print's interior runs
        // a gap — the earlier "filled" read was the low-res gallery's);
        // its dot stays filled, like 34's ring stays open.
        let d38 = arrow_spec(38).unwrap();
        assert!(d38[0].circle.is_some() && d38[0].filled, "38's dot fills");
        assert!(
            !d38[1].filled && d38[1].circle.is_none(),
            "38's diamond strokes open"
        );
        // 12 is ONE wide open V; 43 is the double chevron (the print
        // correction — the old table had the two swapped up).
        let v12 = arrow_spec(12).unwrap();
        assert_eq!(v12[0].d, "M-3,-1 L0,0 L-3,1", "12 is the single wide V");
        let v43 = arrow_spec(43).unwrap();
        assert_eq!(v43[0].d.matches('M').count(), 2, "43 is the double chevron");
        // The 5/6/8/18 rears are circular arcs (exact cubic handles);
        // 5/8 concave, 6/18 convex.
        for kind in [5, 8, 6, 18] {
            let parts = arrow_spec(kind).unwrap();
            assert!(
                parts.iter().all(|p| p.d.contains('C')),
                "arrow {kind} carries the circular rear arc"
            );
        }
        let concave = arrow_spec(5).unwrap()[0].d;
        let convex = arrow_spec(6).unwrap()[0].d;
        assert!(concave.contains("C-1.68524"), "5 bows toward the tip");
        assert!(convex.contains("C-2.31476"), "6 bows away from the tip");
        // 39/40: double triangles — filled vs stroked.
        let filled = arrow_spec(39).unwrap();
        assert!(filled.iter().all(|p| p.filled));
        assert_eq!(filled[0].d.matches('M').count(), 2, "two heads");
        let open = arrow_spec(40).unwrap();
        assert!(open.iter().all(|p| !p.filled));
        // 20/41: rings; 42: a filled dot.
        for kind in [20, 41] {
            let parts = arrow_spec(kind).unwrap();
            assert!(parts.iter().all(|p| p.circle.is_some() && !p.filled));
        }
        let dot_parts = arrow_spec(42).unwrap();
        assert!(dot_parts[0].circle.is_some() && dot_parts[0].filled);
        // 25/26: two / three tick strokes in one part.
        let t2 = arrow_spec(25).unwrap();
        assert_eq!(t2[0].d.matches('M').count(), 2);
        let t3 = arrow_spec(26).unwrap();
        assert_eq!(t3[0].d.matches('M').count(), 3);
        // 35: a filled-dot part plus a tick part (composite).
        let comp = arrow_spec(35).unwrap();
        assert_eq!(comp.len(), 2);
        assert!(comp[0].filled && comp[0].circle.is_some());
        assert!(!comp[1].filled && comp[1].circle.is_none());
        // 9 leans "/" through the tip; 23 leans "\" behind it (the
        // print's two slanted ticks oppose).
        assert_eq!(arrow_spec(9).unwrap()[0].d, "M1,-1 L-1,1");
        assert_eq!(arrow_spec(23).unwrap()[0].d, "M-2.9,-1 L-0.9,1");
        // 46–50: bare ends.
        for kind in [46, 47, 48, 49, 50] {
            assert!(arrow_spec(kind).is_none(), "arrow {kind} is bare");
        }
    }

    /// #199 + #185 smoke: a line with EndArrow renders the head from
    /// the verified table at its measured size — the open diamond 22
    /// strokes with its shaft trimmed to the rear, the classic
    /// triangle 4 fills with the shaft under its fill, the zero-weight
    /// line uses the C[size] + 0.75 pt unit, and the bare end 46
    /// carries no head.
    #[test]
    fn arrow_markers_render_from_the_verified_table() {
        let fixture = |kind: i64| -> String {
            let mut shape = Shape {
                id: 1,
                ..Shape::default()
            };
            shape
                .sheet
                .set_cell("EndArrow", Cell::value(kind.to_string()));
            shape.sheet.children.push(SheetChild::Section(Section {
                name: "Geometry".into(),
                rows: vec![
                    Row {
                        t: Some("MoveTo".into()),
                        ix: 0,
                        cells: vec![
                            ("X".into(), Cell::value("0")),
                            ("Y".into(), Cell::value("0")),
                        ],
                        ..Row::default()
                    },
                    Row {
                        t: Some("LineTo".into()),
                        ix: 1,
                        cells: vec![
                            ("X".into(), Cell::value("2")),
                            ("Y".into(), Cell::value("0")),
                        ],
                        ..Row::default()
                    },
                ],
                ..Section::default()
            }));
            let mut page = Page {
                id: 1,
                name: "Page-1".into(),
                ..Page::default()
            };
            page.shapes.push(shape);
            let mut doc = Document::default();
            doc.pages.push(page);
            render_page(&doc, &doc.pages[0]).expect("render")
        };
        // The unit at the default 0.75 pt pen, size 2: 0.75/72 +
        // 2.52/72 = 0.045417 in.
        let unit = 0.75 / 72.0 + 2.52 / 72.0;
        // #239: the diamonds trim TWO PENS short of their rear vertex
        // (the ink-tangency rule), so 22's setback is 4.5·unit − 2·pen.
        let pen = 0.75 / 72.0;
        // The open diamond 22 strokes, and its setback trims the shaft
        // to 2 − (4.5·unit − 2·pen).
        let open22 = fixture(22);
        assert!(open22.contains("dc-ah"), "head attached: {open22}");
        assert!(open22.contains("M0,0 L-2.25,-1.125 L-4.5,0 L-2.25,1.125 Z"));
        assert!(open22.contains("fill=\"none\" stroke=\"#000000\""));
        let cut = 2.0 - (4.5 * unit - 2.0 * pen);
        assert!(
            open22.contains(&format!("{cut:.5}")),
            "the shaft trims two pens short of the diamond's rear: {open22}"
        );
        // 34 (ring + open diamond) trims the same rule under its 6-unit
        // rear; 38's diamond STROKES (the #239 fixture pass: the print's
        // interior is hollow) and trims identically.
        let comp34 = fixture(34);
        let cut34 = 2.0 - (6.0 * unit - 2.0 * pen);
        assert!(
            comp34.contains(&format!("{cut34:.5}")),
            "34 trims two pens short of its rear: {comp34}"
        );
        let comp38 = fixture(38);
        assert!(
            comp38.contains("M-1.5,0 L-3.75,-1.125 L-6,0 L-3.75,1.125 Z\" fill=\"none\""),
            "38's diamond strokes open (the print's interior is hollow): {comp38}"
        );
        assert!(
            comp38.contains(&format!("{cut34:.5}")),
            "38 trims with the diamond rule: {comp38}"
        );
        // The classic triangle 4 fills; its 2-unit setback hides under
        // the fill (trim to 2 − 2·unit).
        let classic4 = fixture(4);
        assert!(classic4.contains("dc-ah"), "head attached: {classic4}");
        assert!(classic4.contains("<path d=\"M0,0 L-2,-1 L-2,1 Z\" fill=\"#000000\"/>"));
        assert!(classic4.contains(&format!("{:.5}", 2.0 - 2.0 * unit)));
        // The bare end 46 carries no head at all.
        let bare46 = fixture(46);
        assert!(!bare46.contains("dc-ah"), "bare ends carry no head");
    }

    /// #185: the shaft trim — a straight line shortens from each end
    /// by the physical setbacks, closed loops and multi-subpath data
    /// stay untouched, a cut that would consume the path keeps the
    /// original, a quadratic splits exactly onto the original curve,
    /// and an arc sub-span lands on the cut angle.
    #[test]
    fn trim_path_cuts_open_contours_only_and_exactly() {
        // Straight line: symmetric and one-sided trims.
        assert_eq!(
            trim_path("M 0 0 L 1 0 ", 0.1, 0.1),
            "M 0.10000 0.00000 L 0.90000 0.00000"
        );
        assert_eq!(
            trim_path("M 0 0 L 1 0 ", 0.0, 0.25),
            "M 0.00000 0.00000 L 0.75000 0.00000"
        );
        assert_eq!(
            trim_path("M 0 0 L 1 0 ", 0.25, 0.0),
            "M 0.25000 0.00000 L 1.00000 0.00000"
        );
        // A closed loop never trims.
        let closed = "M 0 0 L 1 0 L 1 1 L 0 0";
        assert_eq!(trim_path(closed, 0.1, 0.1), closed);
        // Multi-subpath data never trims.
        let multi = "M 0 0 L 1 0 M 2 0 L 3 0";
        assert_eq!(trim_path(multi, 0.1, 0.1), multi);
        // A cut that would consume the whole path keeps the original.
        assert_eq!(trim_path("M 0 0 L 1 0 ", 0.6, 0.6), "M 0 0 L 1 0 ");
        // Malformed data comes back untouched.
        assert_eq!(trim_path("garbage", 1.0, 1.0), "garbage");
        // A quadratic splits exactly: the trimmed sub-curve's endpoint
        // is the original at the cut parameter, and its own middle is
        // the original at half the kept span.
        let q = "M 0 0 Q 1 2 2 0 ";
        let samples = segment_samples('Q', &[1.0, 2.0, 2.0, 0.0], (0.0, 0.0));
        let total = polyline_length(&samples);
        let t_cut = sample_time_at_length(&samples, total - 0.5);
        let trimmed = trim_path(q, 0.0, 0.5);
        let expected_end = quad_at((0.0, 0.0), (1.0, 2.0), (2.0, 0.0), t_cut);
        assert!(
            trimmed.contains(&format!("{:.5} {:.5}", expected_end.0, expected_end.1)),
            "the cut lands on the curve: {trimmed}"
        );
        let q_part = trimmed.split("Q ").nth(1).expect("sub-quadratic");
        let nums: Vec<f64> = q_part
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        let got = quad_at((0.0, 0.0), (nums[0], nums[1]), (nums[2], nums[3]), 0.5);
        let want = quad_at((0.0, 0.0), (1.0, 2.0), (2.0, 0.0), t_cut * 0.5);
        assert!(
            (got.0 - want.0).abs() < 1e-4 && (got.1 - want.1).abs() < 1e-4,
            "the sub-quadratic tracks the original: {got:?} vs {want:?}"
        );
        // An arc sub-span: trimming a semicircle by a quarter of its
        // length lands the endpoint at the 270° point (1, -1), small
        // arc, sweep preserved.
        let a = "M 0 0 A 1 1 0 0 1 2 0 ";
        let trimmed = trim_path(a, 0.0, std::f64::consts::PI / 2.0);
        let a_part = trimmed.split("A ").nth(1).expect("sub-arc");
        let nums: Vec<f64> = a_part
            .split_whitespace()
            .map(|v| v.parse().unwrap())
            .collect();
        assert!(
            (nums[5] - 1.0).abs() < 5e-3 && (nums[6] + 1.0).abs() < 5e-3,
            "the arc cut lands at the quarter point: {trimmed}"
        );
        assert_eq!(nums[3], 0.0, "the quarter arc is not large");
        assert_eq!(nums[4], 1.0, "the sweep survives the split");
    }

    // ------------------------------------------------------------------
    // Wave-4 text stack (GitHub #232 #230 #229 #231 #234 #235).

    /// #232 stock-font rule: a family outside Visio's stock set
    /// resolves the CALIBRI chain outright (named first, Carlito
    /// substituted — Visio's own SVG renames unknown families
    /// Calibri), while stock families keep their own chain and our
    /// infrastructure faces stay themselves.
    #[test]
    fn non_stock_fonts_resolve_the_calibri_chain() {
        for name in [
            "Roboto",
            "Helvetica",
            "Futura",
            "Fira Mono",
            "Certainly Not Installed",
        ] {
            let chain = family_chain(Some(name));
            assert_eq!(
                chain,
                vec![
                    "Calibri".to_string(),
                    "Carlito".to_string(),
                    "sans-serif".to_string()
                ],
                "{name} lays out in Calibri (#232): {chain:?}"
            );
        }
        // Stock families keep their own chain (Arial still aliases to
        // its metric twin).
        assert_eq!(
            family_chain(Some("Arial"))[..2],
            ["Arial".to_string(), "Liberation Sans".to_string()]
        );
        assert_eq!(family_chain(Some("Calibri"))[0], "Calibri");
        // The app infrastructure faces stay named (#232 exemption) —
        // the RTL fallback must never regress to Calibri.
        assert_eq!(family_chain(Some("Vazirmatn"))[0], "Vazirmatn");
        assert_eq!(
            family_chain(Some("Liberation Serif"))[0],
            "Liberation Serif"
        );
        // No Font cell: Visio's default family is Calibri anyway.
        assert_eq!(family_chain(None)[0], "Calibri");
    }

    /// #230: Character.Pos 1/2 draws at the measured reduced em and
    /// shifts the baseline (Carlito inherits Calibri's 0.667 /
    /// +0.3 / −0.14 numbers); an unwitnessed family keeps the
    /// nominal draw. The LINE SLOTS stay nominal — TEXTHEIGHT
    /// retains the nominal size.
    #[test]
    fn character_pos_scripts_the_block() {
        let emit_pos = |pos: i32, font: Option<&str>| {
            let doc = Document::default();
            let mut s = Shape::default();
            s.sheet.text = Some("H2O".into());
            s.sheet.set_cell("Width", Cell::value("2.0"));
            s.sheet.children.push(SheetChild::Section(Section {
                name: "Character".into(),
                rows: vec![Row {
                    cells: font
                        .map(|f| {
                            vec![
                                ("Pos".to_string(), Cell::value(pos.to_string())),
                                ("Font".to_string(), Cell::value(f)),
                            ]
                        })
                        .unwrap_or_else(|| vec![("Pos".to_string(), Cell::value(pos.to_string()))]),
                    ..Row::default()
                }],
                ..Section::default()
            }));
            let chain = [&s];
            let mut out = String::new();
            emit_text(
                &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
            );
            out
        };
        // Baseline: no Pos cell → nominal draw, no shift.
        let plain = emit_pos(0, None);
        assert!(
            plain.contains(r#"font-size="0.1389""#),
            "nominal size: {plain}"
        );
        // Superscript on the default (Calibri) family: 2/3 em draw,
        // baseline raised 0.3 em from the nominal baseline.
        let sup = emit_pos(1, None);
        assert!(
            sup.contains(r#"font-size="0.0926""#),
            "superscript draws at 0.667 × 10 pt: {sup}"
        );
        let sup_y: f64 = sup
            .split(r#" y=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a y coordinate in the emitted text");
        let plain_y: f64 = plain
            .split(r#" y=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a y coordinate in the emitted text");
        assert!(
            (plain_y - sup_y - 0.3 * 0.13888).abs() < 5e-4,
            "superscript raises the baseline 0.3 em: {plain_y} vs {sup_y}"
        );
        // Subscript lowers by 0.14 em (both measured families).
        let sub = emit_pos(2, None);
        let sub_y: f64 = sub
            .split(r#" y=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a y coordinate");
        assert!(
            (sub_y - plain_y - 0.14 * 0.13888).abs() < 5e-4,
            "subscript lowers the baseline 0.14 em: {sub_y} vs {plain_y}"
        );
        // Arial's witnessed numbers differ (1331/2048, 0.477 em
        // raise) — Liberation Sans inherits them.
        let sup_arial = emit_pos(1, Some("Arial"));
        let arial_y: f64 = sup_arial
            .split(r#" y=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a y coordinate");
        assert!(
            (plain_y - arial_y - 0.477 * 0.13888).abs() < 5e-4,
            "Arial superscripts raise 0.477 em: {arial_y} vs {plain_y}"
        );
        // An unwitnessed stock family keeps the nominal draw.
        let times = emit_pos(1, Some("Times New Roman"));
        assert!(
            times.contains(r#"font-size="0.1389""#),
            "Times New Roman has no witnessed Pos metrics: {times}"
        );
    }

    /// #229: TextBkgnd paints ONE rectangle spanning the laid-out
    /// lines — above geometry, below text — through the +1 palette
    /// encoding (1–24 → palette 0–23), the 0/255 no-paint rule, the
    /// fractional transparency (V=0.5 = 50 %), and the corpus's theme-default formula
    /// (falls to its cached V like every THEMEVAL we don't model).
    #[test]
    fn text_bkgnd_paints_the_laid_out_extent() {
        let emit_bkgnd = |bkgnd: &str, trans: Option<&str>| {
            let doc = Document::default();
            let mut s = Shape::default();
            s.sheet.text = Some("boxed".into());
            s.sheet.set_cell("Width", Cell::value("2.0"));
            s.sheet.set_cell("TextBkgnd", Cell::value(bkgnd));
            if let Some(tr) = trans {
                s.sheet.set_cell("TextBkgndTrans", Cell::value(tr));
            }
            let chain = [&s];
            let mut out = String::new();
            emit_text(
                &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
            );
            out
        };
        // 0 and 255 paint nothing.
        assert!(!emit_bkgnd("0", None).contains("<rect"));
        assert!(!emit_bkgnd("255", None).contains("<rect"));
        // 1 → palette entry 0 (black); 5 → palette entry 4 (blue).
        let black = emit_bkgnd("1", None);
        assert!(
            black.contains(r##"fill="#000000""##) && black.contains("<rect"),
            "TextBkgnd 1 is palette entry 0: {black}"
        );
        assert!(
            emit_bkgnd("5", None).contains(r##"fill="#0000ff""##),
            "TextBkgnd 5 is palette entry 4"
        );
        // 50 % transparency renders as 128/255 ≈ 0.502 alpha.
        let half = emit_bkgnd("1", Some("0.5"));
        assert!(
            half.contains(r#"fill-opacity="0.502""#),
            "50 % trans → 128/255 alpha: {half}"
        );
        // The corpus's theme-default formula: THEMEGUARD(
        // THEMEVAL("BackgroundColor")+1) with cached V=#ffffff — the
        // un-modeled THEMEVAL falls to the cached V (white).
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.text = Some("boxed".into());
        s.sheet.set_cell("Width", Cell::value("2.0"));
        s.sheet.set_cell(
            "TextBkgnd",
            Cell {
                v: Some("#ffffff".into()),
                f: Some(r#"THEMEGUARD(THEMEVAL("BackgroundColor")+1)"#.into()),
                ..Cell::default()
            },
        );
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        assert!(
            out.contains(r##"fill="#ffffff""##),
            "the theme-default label paints its cached white: {out}"
        );
        // The rect sits BEFORE the <text> (below text, above the
        // geometry emitted earlier).
        let rect_at = out.find("<rect").expect("a rect");
        let text_at = out.find("<text").expect("a text");
        assert!(rect_at < text_at, "background below text: {out}");
        // Geometry (PR review wave 4, center-edge fix PR #284): the
        // rect spans the LINES — its y is the FIRST LINE SLOT's top
        // (middle-aligned here: top_edge −1 + (th − total_h)/2 =
        // −0.5833), and its x is the centered line's LEFT edge:
        // band [0.1, 1.9] centers at 1.0, so the text (0.342 wide)
        // starts at 0.829 — the background sits BEHIND the text, not
        // at the left margin.
        let rect = &out[rect_at..text_at];
        assert!(
            rect.contains(r#"x="0.8290""#),
            "rect behind the centered text: {rect}"
        );
        assert!(
            rect.contains(r#"y="-0.5833""#),
            "rect at the first line slot: {rect}"
        );
        let rect_w: f64 = rect
            .split(r#"width=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a rect width");
        assert!(
            (rect_w - 0.3420).abs() < 2e-3,
            "the rect spans the widest laid-out line: {rect_w}"
        );
        // An all-RTL block (explicit RLE) paints its background too —
        // the span joins the union from the RTL branch (PR review).
        let mut s = Shape::default();
        s.sheet.text = Some("\u{202B}\u{0633}\u{0644}\u{0627}\u{0645}".into());
        s.sheet.set_cell("Width", Cell::value("2.0"));
        s.sheet.set_cell("TextBkgnd", Cell::value("1"));
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        assert!(
            out.contains(r##"fill="#000000""##) && out.contains("<rect"),
            "an all-RTL block paints its text background: {out}"
        );
    }

    /// #231: a bulleted paragraph's text starts at the computed tab
    /// stop (bullet glyph as a path, then the text left-anchored at
    /// the stop); TextPosAfterBullet only applies when it lies past
    /// the bullet's start (the measured Visio-2021 rule), and a
    /// custom bullet string draws as text.
    #[test]
    fn bullets_place_text_at_the_tab_stop() {
        let emit_bullet = |bullet: &str, extra: &[(&str, &str)]| {
            let doc = Document::default();
            let mut s = Shape::default();
            s.sheet.text = Some("bulleted".into());
            s.sheet.set_cell("Width", Cell::value("4.0"));
            // Zero L/R margins: the text-area left edge sits at 0, so
            // the tab-stop offsets below assert in TEXT-AREA
            // coordinates exactly as the #231 witness reports them.
            s.sheet.set_cell("LeftMargin", Cell::value("0"));
            s.sheet.set_cell("RightMargin", Cell::value("0"));
            s.sheet.children.push(SheetChild::Section(Section {
                name: "Paragraph".into(),
                rows: vec![Row {
                    cells: {
                        let mut cells = vec![
                            ("Bullet".to_string(), Cell::value(bullet)),
                            ("HorzAlign".to_string(), Cell::value("0")),
                        ];
                        for (n, v) in extra {
                            cells.push((n.to_string(), Cell::value(*v)));
                        }
                        cells
                    },
                    ..Row::default()
                }],
                ..Section::default()
            }));
            let chain = [&s];
            let mut out = String::new();
            emit_text(
                &doc, &s, &chain, 4.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
            );
            out
        };
        // Built-in style 1 draws a PATH dot (never a text glyph), at
        // the line start (bullet offset 0 with zero margins).
        let out = emit_bullet("1", &[]);
        assert!(
            out.contains(r#"<path transform="translate(0.0000,"#),
            "path bullet: {out}"
        );
        // The path is a SIBLING of <text>, not a child (PR #284
        // review: SVG's text content model has no place for shapes —
        // usvg drops a <path> inside <text>, and the bullet would
        // reserve its gap but paint nothing).
        let path_at = out.find("<path").expect("a path");
        let text_open_at = out.find("<text").expect("a text");
        let text_close_at = out.rfind("</text>").expect("the text close");
        assert!(
            path_at < text_open_at || path_at > text_close_at,
            "the bullet path must live outside the text element: {out}"
        );
        // The text starts AT the tab stop — left-anchored past the
        // bullet (DefaultTabStop 0.5 default).
        let tx: f64 = out
            .split(r#"<tspan x=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a tspan x");
        assert!(
            (tx - 0.5).abs() < 1e-3,
            "after-bullet text sits on the 0.5 tab stop: {tx}"
        );
        // The measured rule (the #231 witness verbatim): IndFirst
        // 0.25 with TextPosAfterBullet 0.1 — TPAB does not lie past
        // the bullet's start (0.25), so it is rejected and the next
        // tab stop (0.5) wins; Visio does not add TPAB to IndFirst
        // (that would read 0.35). TextPosAfterBullet 0.6 past the
        // bullet's start wins outright.
        let early: f64 = emit_bullet("1", &[("IndFirst", "0.25"), ("TextPosAfterBullet", "0.1")])
            .split(r#"<tspan x=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a tspan x");
        assert!(
            (early - 0.5).abs() < 1e-3,
            "0.1 < the stop → tab wins: {early}"
        );
        let late: f64 = emit_bullet("1", &[("TextPosAfterBullet", "0.6")])
            .split(r#"<tspan x=""#)
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|v| v.parse().ok())
            .expect("a tspan x");
        assert!(
            (late - 0.6).abs() < 1e-3,
            "0.6 past the stop → wins: {late}"
        );
        // A custom bullet string stays TEXT in its own face.
        let custom = emit_bullet("–", &[]);
        assert!(
            custom.contains(">&ndash;</tspan>") || custom.contains(">\u{2013}</tspan>"),
            "custom bullets stay text: {custom}"
        );
        // Bullet 0: no bullet machinery at all.
        assert!(!emit_bullet("0", &[]).contains("<path"));

        // The centered paragraph's custom bullet: the element anchor
        // is middle, the bullet tspan must override to start.
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.text = Some("bulleted".into());
        s.sheet.set_cell("Width", Cell::value("4.0"));
        s.sheet.set_cell("LeftMargin", Cell::value("0"));
        s.sheet.set_cell("RightMargin", Cell::value("0"));
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Paragraph".into(),
            rows: vec![Row {
                cells: vec![
                    ("Bullet".to_string(), Cell::value("–")),
                    // NO HorzAlign cell → the default center.
                ],
                ..Row::default()
            }],
            ..Section::default()
        }));
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 4.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        // The element anchor is middle; only the bullet tspan pins
        // "start" (the main text tspan's override carries no
        // font-size, so this substring matches the bullet alone).
        assert!(
            out.contains(r#"text-anchor="start" font-size"#),
            "the centered custom bullet pins its own start anchor: {out}"
        );
    }

    /// Final local CodeRabbit pass: numeric Bullet values that fail
    /// the style filter are UNSUPPORTED STYLES, not custom text —
    /// "-1"/"1.5" arm nothing (the old filter conflated them with
    /// non-numeric strings and painted the literal as a glyph).
    #[test]
    fn unsupported_numeric_bullets_stay_off() {
        let doc = Document::default();
        let emit = |bullet: &str| {
            let mut s = Shape::default();
            s.sheet.text = Some("bulleted".into());
            s.sheet.set_cell("Width", Cell::value("4.0"));
            s.sheet.set_cell("LeftMargin", Cell::value("0"));
            s.sheet.set_cell("RightMargin", Cell::value("0"));
            s.sheet.children.push(SheetChild::Section(Section {
                name: "Paragraph".into(),
                rows: vec![Row {
                    cells: vec![
                        ("Bullet".to_string(), Cell::value(bullet)),
                        ("HorzAlign".to_string(), Cell::value("0")),
                    ],
                    ..Row::default()
                }],
                ..Section::default()
            }));
            let chain = [&s];
            let mut out = String::new();
            emit_text(
                &doc, &s, &chain, 4.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
            );
            out
        };
        for v in ["-1", "1.5", "256", "0.0"] {
            let out = emit(v);
            assert!(
                !out.contains(r#"<path transform="translate("#),
                "{v} draws no round-set path bullet: {out}"
            );
            // …and no tab stop is reserved — the text starts at the
            // margin like an unbULLETED block.
            let tx: f64 = out
                .split(r#"<tspan x=""#)
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .and_then(|x| x.parse().ok())
                .unwrap_or(-1.0);
            assert!((tx).abs() < 1e-3, "{v} reserves no bullet tab stop: x={tx}");
        }
        // A genuinely non-numeric string still arms the custom bullet.
        let out = emit("\u{2022}");
        assert!(
            out.contains(r#"<path transform="translate("#) || out.contains('\u{2022}'),
            "a custom string bullet still draws"
        );
    }

    /// #235 terminal-NBSP hang: a trailing no-break space never
    /// wraps its word — the overflow check measures the line without
    // the NBSP's advance, while the NBSP stays in the line content.
    #[test]
    fn terminal_nbsp_hangs_past_the_margin() {
        let latin = crate::textmetrics::resolve(None, false, false);
        let fallback = crate::textmetrics::resolve(Some("Vazirmatn"), false, false);
        // Grow a word until word+NBSP just exceeds a 0.5 in line
        // while the word alone still fits: the hang rule must keep
        // that word whole (the NBSP paints past the margin), where
        // the plain measurement would hard-split it.
        let mut word = String::from("m");
        while latin.width_in(&format!("{word}\u{a0}"), 0.13889) <= 0.5 {
            word.push('n');
        }
        assert!(
            latin.width_in(&word, 0.13889) <= 0.5,
            "fixture: the word alone fits the margin"
        );
        let chain: Vec<&crate::textmetrics::ResolvedFace> = vec![&fallback];
        let solo = wrap_block_text(
            &latin,
            &format!("{word}\u{a0}"),
            0.13889,
            0.5,
            &latin,
            0.13889,
            &chain,
            0.0,
            0.0,
        );
        assert_eq!(
            solo.len(),
            1,
            "word+NBSP never hard-splits on the hanging advance: {:?}",
            solo.iter().map(|l| &l.text).collect::<Vec<_>>()
        );
        assert_eq!(
            solo[0].text,
            format!("{word}\u{a0}"),
            "the NBSP stays content"
        );
        let lines = wrap_block_text(
            &latin,
            &format!("{word}\u{a0} next"),
            0.13889,
            0.5,
            &latin,
            0.13889,
            &chain,
            0.0,
            0.0,
        );
        assert_eq!(
            lines[0].text,
            format!("{word}\u{a0}"),
            "the NBSP binds its word onto the first line (#235)"
        );
        // And a plain space there DOES wrap.
        let lines_plain = wrap_block_text(
            &latin,
            &format!("{word} next"),
            0.13889,
            0.5,
            &latin,
            0.13889,
            &chain,
            0.0,
            0.0,
        );
        assert_eq!(
            lines_plain[0].text, word,
            "the control: a plain trailing space wraps"
        );
    }

    /// #282 zh-Hans: CJK paragraphs wrap between ideographs under the
    /// kinsoku rules — no spaces needed — and a closing punct never
    /// STARTS a line. The measurement face is irrelevant to the break
    /// STRUCTURE (Carlito's missing-glyph advances are uniform here);
    /// the tokenizer owns the opportunities, so the unregistered test
    /// world pins them fine.
    #[test]
    fn cjk_paragraph_wraps_between_ideographs_with_kinsoku() {
        let latin = crate::textmetrics::resolve(None, false, false);
        let fallback = crate::textmetrics::resolve(Some("Vazirmatn"), false, false);
        let chain: Vec<&crate::textmetrics::ResolvedFace> = vec![&fallback];
        let size = 0.13889;
        let w1 = latin.width_in("图", size);
        assert!(w1 > 0.0);
        // A width fitting two ideographs: the paragraph breaks every
        // two glyphs — between ideographs, never inside one.
        let lines = wrap_block_text(
            &latin,
            "图表形状",
            size,
            w1 * 2.5,
            &latin,
            size,
            &chain,
            0.0,
            0.0,
        );
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec!["图表", "形状"],
            "ideograph boundaries are the break opportunities"
        );
        // Kinsoku across widths: the sentence enders stay glued to
        // the token they close and never open a line.
        for usable in [w1 * 1.2, w1 * 1.8, w1 * 2.5] {
            let lines = wrap_block_text(
                &latin,
                "画图，上色。",
                size,
                usable,
                &latin,
                size,
                &chain,
                0.0,
                0.0,
            );
            for l in &lines {
                assert!(
                    !l.text.starts_with(['，', '。']),
                    "a closing punct never starts a line: {:?}",
                    l.text
                );
            }
        }
        // The Latin path is untouched: the same engine, pure spaces.
        let lines = wrap_block_text(
            &latin,
            "ab cd",
            size,
            latin.width_in("ab", size) * 1.2,
            &latin,
            size,
            &chain,
            0.0,
            0.0,
        );
        assert_eq!(
            lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            vec!["ab", "cd"],
            "Latin keeps the space-driven breaks"
        );
    }

    /// #234: the composed width quantizes the size then rounds every
    /// glyph advance onto the same grid — for a single glyph that is
    /// exactly one grid-rounded product, and the terminal-run space
    /// + slack make TEXTWIDTH strictly wider than the bare advance.
    #[test]
    fn composed_width_rounds_on_the_size_grid() {
        let f = crate::textmetrics::resolve(None, false, false);
        let size = 0.13889;
        let q = crate::textmetrics::quantize18(size);
        let adv_em = f.width_in("x", 1.0);
        let expect = (q * adv_em * 262_144.0).round() / 262_144.0;
        let got = f.composed_width_in("x", size);
        assert!(
            (got - expect).abs() < 1e-12,
            "one glyph: {got} vs grid-rounded {expect}"
        );
        // The space advance itself lands on the grid.
        let sp = f.space_advance_in(size);
        assert!(
            (sp * 262_144.0 - (sp * 262_144.0).round()).abs() < 1e-9,
            "the terminal space is a grid step"
        );
    }

    // ---- #267 field substitution ----

    /// The corpus's common shape: a master-root Field row referenced
    /// by a self-closing marker on an instanced shape, with the
    /// User row the Value formula reads living on the SAME sheet.
    #[test]
    fn field_marker_renders_the_substituted_value() {
        let mut doc = Document::default();
        let mut m = vsdx::Master {
            id: 2,
            name_u: Some("Card".into()),
            ..vsdx::Master::default()
        };
        let mut root = Shape {
            id: 1,
            ..Shape::default()
        };
        root.sheet.set_cell("Width", Cell::value("2.0"));
        root.sheet.set_cell("Height", Cell::value("1.0"));
        root.sheet.children.push(SheetChild::Section(Section {
            name: "User".into(),
            rows: vec![Row {
                n: Some("Label".into()),
                cells: vec![("Value".to_string(), Cell::value("Hello"))],
                ..Row::default()
            }],
            ..Section::default()
        }));
        // The Field row, in the corpus's verbatim cell shape.
        root.sheet.children.push(SheetChild::Section(Section {
            name: "Field".into(),
            rows: vec![Row {
                ix: 0,
                cells: vec![
                    (
                        "Value".to_string(),
                        Cell {
                            v: Some(String::new()),
                            u: Some("STR".into()),
                            f: Some("User.Label".into()),
                            ..Cell::default()
                        },
                    ),
                    (
                        "Format".to_string(),
                        Cell {
                            v: Some("esc(0)".into()),
                            u: Some("STR".into()),
                            f: Some("FIELDPICTURE(0)".into()),
                            ..Cell::default()
                        },
                    ),
                ],
                ..Row::default()
            }],
            ..Section::default()
        }));
        root.sheet.text_raw = Some("<fld IX='0'/>".into());
        root.sheet.text = Some(String::new());
        m.shapes.push(root);
        doc.masters.push(m);

        // An instance of the master carrying the marker's text.
        let mut inst = Shape {
            id: 5,
            master: Some(2),
            ..Shape::default()
        };
        inst.sheet.set_cell("Width", Cell::value("2.0"));
        inst.sheet.set_cell("Height", Cell::value("1.0"));
        inst.sheet.text_raw = Some("due <fld IX='0'/>".into());
        inst.sheet.text = Some("due ".into());
        let page_sheet = |w: &str, h: &str| {
            let mut sheet = Sheet::default();
            sheet.set_cell("PageWidth", Cell::value(w));
            sheet.set_cell("PageHeight", Cell::value(h));
            sheet
        };
        let mut page = vsdx::Page {
            id: 0,
            name: "Page-1".into(),
            ..vsdx::Page::default()
        };
        page.children.push(IndexChild::Sheet {
            tag: "PageSheet".into(),
            sheet: page_sheet("8.5", "11.0"),
        });
        page.shapes.push(inst);
        doc.pages.push(page);

        let svg = render_page(&doc, &doc.pages[0]).unwrap();
        assert!(
            svg.contains("due Hello"),
            "the marker substitutes the evaluated field: {svg}"
        );
    }

    /// Unresolvable fields fall back marker content → cached V →
    /// empty, never erroring the render.
    #[test]
    fn field_marker_falls_back_to_the_cached_display() {
        let doc = Document::default();
        let mut s = Shape::default();
        s.sheet.set_cell("Width", Cell::value("2.0"));
        s.sheet.set_cell("Height", Cell::value("1.0"));
        // A Value formula that cannot evaluate (unknown cell), no
        // cached V: the marker's own content serves.
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Field".into(),
            rows: vec![Row {
                ix: 0,
                cells: vec![(
                    "Value".to_string(),
                    Cell {
                        u: Some("STR".into()),
                        f: Some("User.Missing".into()),
                        ..Cell::default()
                    },
                )],
                ..Row::default()
            }],
            ..Section::default()
        }));
        s.sheet.text_raw = Some("value: <fld IX='0'>4/17</fld>".into());
        s.sheet.text = Some("value: 4/17".into());
        let chain = [&s];
        let mut out = String::new();
        emit_text(
            &doc, &s, &chain, 2.0, 1.0, 1.0, 1.0, false, None, None, &mut out,
        );
        assert!(
            out.contains("value: 4/17"),
            "unresolvable field serves its cached display: {out}"
        );
    }

    /// Page facts thread through the walk: PAGENUMBER()/PAGENAME()
    /// evaluate live.
    #[test]
    fn field_page_number_renders_live() {
        let mut doc = Document::default();
        let mut s = Shape {
            id: 1,
            ..Shape::default()
        };
        s.sheet.set_cell("Width", Cell::value("2.0"));
        s.sheet.set_cell("Height", Cell::value("1.0"));
        s.sheet.children.push(SheetChild::Section(Section {
            name: "Field".into(),
            rows: vec![Row {
                ix: 0,
                cells: vec![(
                    "Value".to_string(),
                    Cell {
                        u: Some("STR".into()),
                        f: Some("PAGENUMBER()".into()),
                        ..Cell::default()
                    },
                )],
                ..Row::default()
            }],
            ..Section::default()
        }));
        s.sheet.text_raw = Some("<fld IX='0'/>".into());
        s.sheet.text = Some(String::new());
        let mut sheet = Sheet::default();
        sheet.set_cell("PageWidth", Cell::value("8.5"));
        sheet.set_cell("PageHeight", Cell::value("11.0"));
        let mut page = vsdx::Page {
            id: 0,
            name: "Flowchart".into(),
            ..vsdx::Page::default()
        };
        page.children.push(IndexChild::Sheet {
            tag: "PageSheet".into(),
            sheet,
        });
        page.shapes.push(s);
        doc.pages.push(page);
        // A second page so the first is page 1 of 2 — PAGENUMBER is
        // the 1-based position.
        doc.pages.push(vsdx::Page {
            id: 1,
            name: "Page-2".into(),
            ..vsdx::Page::default()
        });

        let svg = render_page(&doc, &doc.pages[0]).unwrap();
        assert!(
            svg.contains(">1<"),
            "PAGENUMBER renders the 1-based position: {svg}"
        );
    }
}
