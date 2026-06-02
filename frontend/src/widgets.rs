use std::cell::RefCell;

use wasm_bindgen::JsCast;
use web_sys::{HtmlElement, MouseEvent};

use walloftext_shared::{ClientMsg, RegionRecord, WorldCoords};

use crate::dom::{El, Listener, coord_span, debug_log, document, log, sb_msg, window};
use crate::render::schedule_render;
use crate::state::{
    AuthState, DIRTY_BITS, HoveredLink, dirty, mark_dirty, read_board, read_ui, read_vp, send_msg,
    with_ui,
};

pub trait Widget {
    fn id(&self) -> &'static str;
    fn has_panel(&self) -> bool {
        true
    }
    fn btn_id(&self) -> Option<&'static str> {
        None
    }
    fn on_open(&self) {}
    fn init_events(&self) {}
    fn dirty_bit(&self) -> u32 {
        dirty::ALL
    }
    fn render(&self) {}
}

pub fn widgets() -> Vec<Box<dyn Widget>> {
    vec![
        Box::new(HelpPanel),
        Box::new(StatsPanel),
        Box::new(OnlinePanel),
        Box::new(RegionsPanel),
        Box::new(AuthPanel),
        Box::new(LogPanel),
        Box::new(SnapPanel),
        Box::new(ColorBar),
        Box::new(StatusBar),
        Box::new(SelInfoWidget),
        Box::new(InfoPopupWidget),
    ]
}

pub fn render_dirty() {
    let bits = DIRTY_BITS.with(|d| {
        let b = d.get();
        d.set(0);
        b
    });
    if bits == 0 {
        return;
    }
    for w in widgets() {
        if bits & w.dirty_bit() != 0 {
            w.render();
        }
    }
}

pub fn render_all() {
    mark_dirty(dirty::ALL);
    schedule_render();
}

pub fn render_popups() {
    mark_dirty(dirty::POPUP | dirty::STATUSBAR);
    schedule_render();
}

struct StatusBar;
impl Widget for StatusBar {
    fn id(&self) -> &'static str {
        "statusbar"
    }
    fn has_panel(&self) -> bool {
        false
    }
    fn dirty_bit(&self) -> u32 {
        dirty::STATUSBAR | dirty::POPUP
    }
    fn render(&self) {
        let (x, y) = read_vp(|vp| match vp.hover {
            Some(pos) => (pos.x as i64, pos.y as i64),
            None => read_ui(|ui| (ui.cursor.pos.x as i64, ui.cursor.pos.y as i64)),
        });
        let zoom = read_vp(|vp| vp.zoom);
        El::from_id("sb-pos").map(|el| el.text(&format!("{},{}", x, y)));
        El::from_id("sb-zoom").map(|el| el.text(&format!("{}%", (zoom * 100.0).round() as u32)));
    }
}

struct SelInfoWidget;
impl Widget for SelInfoWidget {
    fn id(&self) -> &'static str {
        "sel-info"
    }
    fn has_panel(&self) -> bool {
        false
    }
    fn dirty_bit(&self) -> u32 {
        dirty::SEL_INFO
    }
    fn render(&self) {
        let (active, sel) = read_ui(|ui| (ui.cursor.sel.active, ui.cursor.sel.rect()));
        if let Some(info) = El::from_id("sel-info") {
            if active {
                let _ = info
                    .text(&format!(
                        "({},{})->({},{}) {}x{}  [Ctrl+P=protect | Ctrl+C=copy]",
                        sel.a.x,
                        sel.a.y,
                        sel.b.x,
                        sel.b.y,
                        sel.width(),
                        sel.height()
                    ))
                    .css("display", "block");
            } else {
                let _ = info.css("display", "none");
            }
        }
    }
}

struct InfoPopupWidget;
impl Widget for InfoPopupWidget {
    fn id(&self) -> &'static str {
        "info-popup"
    }
    fn has_panel(&self) -> bool {
        false
    }
    fn dirty_bit(&self) -> u32 {
        dirty::POPUP
    }
    fn render(&self) {
        if !crate::render::is_glyph_mode() {
            crate::dom::hide_tooltips();
            return;
        }
        let (hover, hover_sx, hover_sy, hover_above, hovered_link) = read_vp(|vp| {
            (
                vp.hover,
                vp.hover_sx,
                vp.hover_sy,
                vp.hover_above,
                vp.hovered_link.clone(),
            )
        });
        let Some(pos) = hover else {
            crate::dom::hide_tooltips();
            return;
        };

        let tooltip_x = hover_sx + 14.0;
        let anchor_y = hover_sy;

        let region = read_board(|b| b.regions.values().find(|r| r.bounds.contains(pos)).cloned());
        let cell = read_board(|b| {
            b.chunk_manager
                .get_cell(pos)
                .map(|c| (c.ch, b.author_name(c.author_id), c.ts))
        });

        let rg_h = if let Some(ref r) = region {
            El::from_id("rg-label").map(|e| e.text(&r.label));
            El::from_id("rg-owner").map(|e| e.text(&r.owner));
            El::from_id("rg-size")
                .map(|e| e.text(&format!("{}x{}", r.bounds.width(), r.bounds.height())));
            El::from_id("rg-bounds").map(|e| {
                e.text(&format!(
                    "({},{})->({},{})",
                    r.bounds.a.x, r.bounds.a.y, r.bounds.b.x, r.bounds.b.y
                ))
            });
            El::from_id("rginfo")
                .map(|e| {
                    let h = e.offset_height();
                    let _ = e.css("opacity", "1");
                    h
                })
                .unwrap_or(0.0)
        } else {
            El::from_id("rginfo").map(|e| e.css("opacity", "0"));
            0.0
        };

        let px_h = if let Some((ch, ref author, ts)) = cell {
            El::from_id("px-ch").map(|e| e.text(&ch.to_string()));
            El::from_id("px-au").map(|e| e.text(author));
            if let Some(el) = El::from_id("px-ts") {
                let _ = el.text(
                    &js_sys::Date::new(&(ts as f64 * 1000.0).into())
                        .to_locale_string("default", &js_sys::Object::new())
                        .as_string()
                        .unwrap_or_default(),
                );
            }
            El::from_id("px-xy").map(|e| e.text(&format!("({},{})", pos.x, pos.y)));
            El::from_id("pxinfo")
                .map(|e| {
                    let h = e.offset_height();
                    let _ = e.css("opacity", "1");
                    h
                })
                .unwrap_or(0.0)
        } else {
            El::from_id("pxinfo").map(|e| e.css("opacity", "0"));
            0.0
        };

        let li_h = if let Some(ref lnk) = hovered_link {
            let (dest, action) = match lnk {
                HoveredLink::Coord(pos) => (format!("{},{}", pos.x, pos.y), "go to coords"),
                HoveredLink::Url(url) => (url.clone(), "open in new tab"),
            };
            El::from_id("link-dest").map(|e| e.text(&dest));
            El::from_id("link-action").map(|e| e.text(action));
            El::from_id("linkinfo")
                .map(|e| {
                    let h = e.offset_height();
                    let _ = e.css("opacity", "1");
                    h
                })
                .unwrap_or(0.0)
        } else {
            El::from_id("linkinfo").map(|e| e.css("opacity", "0"));
            0.0
        };

        let gap = |h: f64| if h > 0.0 { 4.0 } else { 0.0 };
        let pos = |id: &str, x: f64, y: f64| {
            El::from_id(id).map(|e| {
                e.css("left", &format!("{}px", x))
                    .css("top", &format!("{}px", y))
            });
        };

        if hover_above {
            let rg_top = anchor_y - rg_h;
            let px_top = rg_top - gap(rg_h) - px_h;
            let li_top = px_top - gap(px_h) - li_h;
            if rg_h > 0.0 {
                pos("rginfo", tooltip_x, rg_top);
            }
            if px_h > 0.0 {
                pos("pxinfo", tooltip_x, px_top);
            }
            if li_h > 0.0 {
                pos("linkinfo", tooltip_x, li_top);
            }
        } else {
            let px_top = anchor_y + rg_h + gap(rg_h);
            let li_top = px_top + px_h + gap(px_h);
            if rg_h > 0.0 {
                pos("rginfo", tooltip_x, anchor_y);
            }
            pos("pxinfo", tooltip_x, px_top);
            if li_h > 0.0 {
                pos("linkinfo", tooltip_x, li_top);
            }
        }
    }
}

struct HelpPanel;
impl Widget for HelpPanel {
    fn id(&self) -> &'static str {
        "panel-help"
    }
    fn dirty_bit(&self) -> u32 {
        0
    }
    fn btn_id(&self) -> Option<&'static str> {
        Some("btn-help")
    }
}

struct StatsPanel;
impl Widget for StatsPanel {
    fn id(&self) -> &'static str {
        "panel-stats"
    }
    fn dirty_bit(&self) -> u32 {
        dirty::STATS
    }
    fn btn_id(&self) -> Option<&'static str> {
        Some("btn-stats")
    }
    fn on_open(&self) {
        send_msg(&ClientMsg::FetchStats);
        render_all();
    }
    fn render(&self) {
        let last_stats = read_board(|b| b.last_stats.clone());
        let Some((total, online, ref authors)) = last_stats else {
            return;
        };
        El::from_id("sb-cells").map(|el| el.text(&total.to_string()));
        El::from_id("sb-online").map(|el| el.text(&online.to_string()));
        if !MiniTUI::is_open("panel-stats") {
            return;
        }
        if let Some(mut container) = El::from_id("stats-body") {
            container = container.set_inner_html("");
            let stat_row = |label: &str, val: String| {
                El::div()
                    .class("stat-row")
                    .add(El::span().class("sl").text(label))
                    .add(El::span().class("sv").text(&val))
            };
            container = container.add(stat_row("total", total.to_string()));
            container = container.add(stat_row("online", online.to_string()));
            if !authors.is_empty() {
                container = container.add(El::div().class("tui-section").text("top authors"));
                for (i, a) in authors.iter().enumerate() {
                    let row = El::div()
                        .class("auth-row")
                        .add(El::span().class("ar").text(&(i + 1).to_string()))
                        .add(El::span().class("an").text(&a.name))
                        .add(El::span().class("ac").text(&a.count.to_string()));
                    container = container.add(row);
                }
            }
        }
    }
}

struct OnlinePanel;
impl Widget for OnlinePanel {
    fn id(&self) -> &'static str {
        "panel-online"
    }
    fn dirty_bit(&self) -> u32 {
        dirty::ONLINE
    }
    fn btn_id(&self) -> Option<&'static str> {
        Some("btn-online")
    }
    fn render(&self) {
        let users: Vec<(u32, String, Option<WorldCoords>)> = read_board(|b| {
            b.online_users
                .iter()
                .map(|(&uid, (name, _))| {
                    let pos = b.remote_cursors.get(&uid).map(|&(p, _)| p);
                    (uid, name.clone(), pos)
                })
                .collect()
        });

        if !MiniTUI::is_open("panel-online") {
            return;
        }

        if let Some(mut body) = El::from_id("online-body") {
            body = body.set_inner_html("");
            if users.is_empty() {
                body = body.add(
                    El::div()
                        .class("stat-row")
                        .add(El::span().text("no others online")),
                );
            }
            for (_, name, pos) in users {
                let mut row = El::div()
                    .class("online-row")
                    .add(El::span().class("on").text(&name));
                if let Some(pos) = pos {
                    row = row.add(coord_span(pos));
                }
                body = body.add(row);
            }
        }
    }
}

struct RegionsPanel;
impl Widget for RegionsPanel {
    fn id(&self) -> &'static str {
        "panel-regions"
    }
    fn dirty_bit(&self) -> u32 {
        dirty::REGIONS
    }
    fn btn_id(&self) -> Option<&'static str> {
        Some("btn-regions")
    }
    fn on_open(&self) {
        send_msg(&ClientMsg::FetchRegions);
        render_all();
    }
    fn render(&self) {
        let (my_author, my_id, regions) = read_board(|b| {
            let (my_author, my_id) = read_ui(|ui| (ui.my_author.clone(), ui.my_id));
            let regions: Vec<RegionRecord> = b.regions.values().cloned().collect();
            (my_author, my_id, regions)
        });

        let mut container = match El::from_id("regions-list") {
            Some(el) => el,
            None => return,
        };
        container = container.set_inner_html("");

        if regions.is_empty() {
            let _ = container.add(
                El::div()
                    .style("color:var(--dim);padding:3px 0")
                    .text("no regions"),
            );
            return;
        }

        let build_row = |r: &RegionRecord| -> El {
            let is_mine = r.owner_id == my_id;
            let mut row = El::div()
                .class("region-row")
                .add(El::span().class("rr-id").text(&format!("#{}", r.id)))
                .add(El::span().class("rr-label").text(&r.label))
                .add(
                    El::span()
                        .class("rr-coords")
                        .add(coord_span(r.bounds.a))
                        .add(El::span().text("->"))
                        .add(coord_span(r.bounds.b)),
                )
                .add(
                    El::span()
                        .class("rr-own")
                        .text(if is_mine { "you" } else { &r.owner }),
                );
            if is_mine {
                row = row.add(
                    El::span()
                        .class("rr-del")
                        .attr("data-id", &r.id.to_string())
                        .text("X"),
                );
            }
            row
        };

        let mine: Vec<&RegionRecord> = regions.iter().filter(|r| r.owner == my_author).collect();
        let others: Vec<&RegionRecord> = regions.iter().filter(|r| r.owner != my_author).collect();

        if !mine.is_empty() {
            container = container.add(El::div().class("tui-section").text("yours"));
            for r in mine {
                container = container.add(build_row(r));
            }
        }
        if !others.is_empty() {
            container = container.add(El::div().class("tui-section").text("others"));
            for r in others {
                container = container.add(build_row(r));
            }
        }
    }
    fn init_events(&self) {
        if let Some(b) = El::from_id("ri-add") {
            b.on("click", |_: web_sys::Event| {
                let raw = El::from_id("ri-label")
                    .map(|e| e.get_value())
                    .unwrap_or_default();
                let label = if raw.is_empty() {
                    "protected".into()
                } else {
                    raw
                };
                if !read_ui(|ui| ui.cursor.sel.active) {
                    sb_msg("select area first");
                    return;
                }
                let sel = read_ui(|ui| ui.cursor.sel.rect());
                debug_log!(
                    "AddRegion ({},{})-({},{}) label={}",
                    sel.a.x,
                    sel.a.y,
                    sel.b.x,
                    sel.b.y,
                    label
                );
                send_msg(&ClientMsg::AddRegion { bounds: sel, label });
            });
        }
        if let Some(el) = El::from_id("regions-list") {
            el.on("click", |e: MouseEvent| {
                let t: web_sys::Element = match e.target().and_then(|t| t.dyn_into().ok()) {
                    Some(t) => t,
                    None => return,
                };
                if t.class_list().contains("rr-del")
                    && let Some(id) = t
                        .get_attribute("data-id")
                        .and_then(|s| s.parse::<u32>().ok())
                {
                    debug_log!("RemoveRegion id={}", id);
                    send_msg(&ClientMsg::RemoveRegion { id });
                }
            });
        }
    }
}

struct AuthPanel;
impl Widget for AuthPanel {
    fn id(&self) -> &'static str {
        "panel-auth"
    }
    fn dirty_bit(&self) -> u32 {
        dirty::AUTH
    }
    fn btn_id(&self) -> Option<&'static str> {
        Some("btn-auth")
    }
    fn render(&self) {
        let auth = read_ui(|ui| ui.auth.clone());
        let anon_name = read_ui(|ui| ui.my_anon_name.clone());
        match auth {
            AuthState::Anonymous => {
                El::from_id("sb-id").map(|el| el.text(&anon_name));
                El::from_id("auth-anon").map(|el| el.remove_css("display"));
                El::from_id("auth-user").map(|el| el.css("display", "none"));
                El::from_id("auth-quota").map(|el| el.css("display", "none"));
                El::from_id("auth-form").map(|el| el.remove_css("display"));
                El::from_id("auth-loggedin").map(|el| el.css("display", "none"));
            }
            AuthState::LoggedIn {
                username,
                protected_cells,
            } => {
                El::from_id("sb-id").map(|el| el.text(&username));
                El::from_id("auth-anon").map(|el| el.css("display", "none"));
                El::from_id("auth-user").map(|el| el.remove_css("display"));
                El::from_id("auth-username").map(|el| el.text(&username));
                El::from_id("auth-quota-n").map(|el| el.text(&protected_cells.to_string()));
                El::from_id("auth-quota").map(|el| el.remove_css("display"));
                El::from_id("auth-form").map(|el| el.css("display", "none"));
                El::from_id("auth-loggedin").map(|el| el.css("display", "block"));
            }
        }
    }
    fn init_events(&self) {
        let read_credentials = || -> Option<(String, String)> {
            let u = El::from_id("auth-user-input")?.get_value();
            let p = El::from_id("auth-pass")?.get_value();
            if u.is_empty() || p.is_empty() {
                sb_msg("enter credentials");
                return None;
            }
            Some((u, p))
        };

        if let Some(b) = El::from_id("btn-login") {
            b.on("click", move |_: web_sys::Event| {
                if let Some((u, p)) = read_credentials() {
                    debug_log!("Login attempt: user={}", u);
                    send_msg(&ClientMsg::Login {
                        username: u,
                        password: p,
                    });
                }
            });
        }
        if let Some(b) = El::from_id("btn-logout") {
            b.on("click", |_: web_sys::Event| {
                debug_log!("logout");
                send_msg(&ClientMsg::Logout);
                let anon = read_ui(|ui| ui.my_anon_name.clone());
                with_ui(|ui| {
                    ui.my_author = anon;
                    ui.auth = AuthState::Anonymous;
                });
                render_all();
                log("logged out", "w");
            });
        }
    }
}

struct LogPanel;
impl Widget for LogPanel {
    fn id(&self) -> &'static str {
        "panel-log"
    }
    fn dirty_bit(&self) -> u32 {
        dirty::LOG
    }
    fn btn_id(&self) -> Option<&'static str> {
        Some("btn-log")
    }
}

struct SnapPanel;
impl Widget for SnapPanel {
    fn id(&self) -> &'static str {
        "panel-snap"
    }
    fn dirty_bit(&self) -> u32 {
        dirty::SNAP | dirty::STATUSBAR
    }
    fn btn_id(&self) -> Option<&'static str> {
        Some("btn-snap")
    }
    fn render(&self) {
        let (cur_x, cur_y, bookmarks) = read_ui(|ui| {
            (
                ui.cursor.pos.x as i64,
                ui.cursor.pos.y as i64,
                ui.bookmarks.clone(),
            )
        });
        El::from_id("snap-cur").map(|el| el.text(&format!("cursor: {}, {}", cur_x, cur_y)));
        let mut container = match El::from_id("snap-bookmarks") {
            Some(el) => el,
            None => return,
        };
        container = container.set_inner_html("");
        if bookmarks.is_empty() {
            let _ = container.add(
                El::div()
                    .style("color:var(--dim);font-size:10px")
                    .text("no bookmarks"),
            );
        } else {
            for (i, (n, x, y)) in bookmarks.iter().enumerate() {
                let row = El::div()
                    .class("snap-bm")
                    .attr("data-i", &i.to_string())
                    .add(El::span().class("snap-bm-label").text(n))
                    .add(
                        El::span()
                            .class("snap-bm-coords")
                            .text(&format!("({},{})", x, y)),
                    )
                    .add(
                        El::span()
                            .class("snap-bm-del")
                            .attr("data-i", &i.to_string())
                            .text("X"),
                    );
                container = container.add(row);
            }
        }
    }
    fn init_events(&self) {
        if let Some(b) = El::from_id("snap-go") {
            b.on("click", |_: web_sys::Event| {
                let x: i64 = El::from_id("snap-x")
                    .map(|e| e.get_value())
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or(0);
                let y: i64 = El::from_id("snap-y")
                    .map(|e| e.get_value())
                    .unwrap_or_default()
                    .parse()
                    .unwrap_or(0);
                crate::input::goto(WorldCoords::from_i64(x, y));
            });
        }
        if let Some(b) = El::from_id("snap-bm-add") {
            b.on("click", |_: web_sys::Event| {
                let raw = El::from_id("snap-bm-name")
                    .map(|e| e.get_value())
                    .unwrap_or_default();
                let (cx, cy) = read_ui(|ui| (ui.cursor.pos.x as i64, ui.cursor.pos.y as i64));
                let name = if raw.is_empty() {
                    format!("({},{})", cx, cy)
                } else {
                    raw
                };
                with_ui(|ui| ui.bookmarks.push((name, cx, cy)));
                El::from_id("snap-bm-name").map(|e| e.set_value(""));
                render_all();
                MiniTUI::save_layout();
            });
        }
        if let Some(el) = El::from_id("snap-bookmarks") {
            el.on("click", |e: MouseEvent| {
                let t: HtmlElement = match e.target().and_then(|t| t.dyn_into().ok()) {
                    Some(t) => t,
                    None => return,
                };
                if t.class_list().contains("snap-bm-del") {
                    if let Some(i) = t.dataset().get("i").and_then(|s| s.parse::<usize>().ok()) {
                        with_ui(|ui| {
                            if i < ui.bookmarks.len() {
                                ui.bookmarks.remove(i);
                            }
                        });
                        render_all();
                        MiniTUI::save_layout();
                    }
                } else {
                    let bm_el = if t.class_list().contains("snap-bm") {
                        Some(t)
                    } else {
                        t.closest(".snap-bm")
                            .ok()
                            .flatten()
                            .and_then(|e| e.dyn_into::<HtmlElement>().ok())
                    };
                    if let Some(bm) = bm_el
                        && let Some(i) = bm.dataset().get("i").and_then(|s| s.parse::<usize>().ok())
                    {
                        let (x, y) =
                            read_ui(|ui| ui.bookmarks.get(i).map(|b| (b.1, b.2)).unwrap_or((0, 0)));
                        crate::input::goto(WorldCoords::from_i64(x, y));
                    }
                }
            });
        }
    }
}

struct ColorBar;
impl Widget for ColorBar {
    fn id(&self) -> &'static str {
        "colorbar"
    }
    fn dirty_bit(&self) -> u32 {
        dirty::COLOR
    }
    fn btn_id(&self) -> Option<&'static str> {
        Some("btn-color")
    }
    fn render(&self) {
        let color = read_ui(|ui| ui.color);
        let hex = color.to_hex();
        if let Some(root) = document().document_element()
            && let Ok(he) = root.dyn_into::<HtmlElement>()
        {
            let acc2 = format!(
                "#{:02x}{:02x}{:02x}",
                (color.0 as f64 * 0.78) as u8,
                (color.1 as f64 * 0.78) as u8,
                (color.2 as f64 * 0.78) as u8,
            );
            let _ = he.style().set_property("--accent", &hex);
            let _ = he.style().set_property("--acc2", &acc2);
        }
        if let Ok(nl) = document().query_selector_all(".csw") {
            for i in 0..nl.length() {
                if let Some(node) = nl.item(i) {
                    let el = El {
                        node: node.dyn_into().unwrap(),
                    };
                    let dc = el.dataset_get("color").unwrap_or_default();
                    if dc == hex {
                        el.add_class("active");
                    } else {
                        el.remove_class("active");
                    }
                }
            }
        }
        El::from_id("cust-color").map(|e| e.set_value(&hex));
    }
    fn init_events(&self) {
        use web_sys::HtmlInputElement;

        if let Ok(swatches) = document().query_selector_all(".csw") {
            for i in 0..swatches.length() {
                if let Some(node) = swatches.item(i) {
                    let he: HtmlElement = node.dyn_into().unwrap();
                    let color = he.dataset().get("color").unwrap_or_default();
                    he.on("click", move |_: web_sys::Event| {
                        with_ui(|ui| ui.color = walloftext_shared::Rgb::from_hex(&color));
                        mark_dirty(dirty::COLOR);
                        schedule_render();
                        crate::state::resend_cursor();
                        MiniTUI::save_layout();
                    });
                }
            }
        }
        if let Some(cc) = El::from_id("cust-color") {
            cc.on("input", |e: web_sys::Event| {
                let val = e
                    .target()
                    .and_then(|t| t.dyn_into::<HtmlInputElement>().ok())
                    .map(|i| i.value())
                    .unwrap_or_default();
                with_ui(|ui| ui.color = walloftext_shared::Rgb::from_hex(&val));
                mark_dirty(dirty::COLOR);
                schedule_render();
                crate::state::resend_cursor();
                MiniTUI::save_layout();
            });
        }
    }
}

type DragInfo = (String, f64, f64, f64, f64);

thread_local! {
    static DRAG: RefCell<Option<DragInfo>> = const { RefCell::new(None) };
}

pub struct MiniTUI;

impl MiniTUI {
    fn btn_for(panel_id: &str) -> Option<&'static str> {
        widgets()
            .into_iter()
            .find(|w| w.id() == panel_id)
            .and_then(|w| w.btn_id())
    }

    pub fn open(panel_id: &str) {
        debug_log!("panel open: {}", panel_id);
        if let Some(el) = El::from_id(panel_id) {
            let el = el.add_class("visible");
            let he: Option<HtmlElement> = el.node.clone().dyn_into().ok();
            if let Some(btn_id) = Self::btn_for(panel_id)
                && let Some(btn) = El::from_id(btn_id)
            {
                let btn = btn.add_class("active");
                if el.dataset_get("userMoved").is_none() {
                    let br = btn.rect();
                    let offset_h = el.offset_height();
                    let _ = el
                        .remove_css("right")
                        .remove_css("bottom")
                        .css("left", &format!("{}px", br.left().max(0.0)))
                        .css(
                            "top",
                            &format!("{}px", (br.top() - offset_h - 4.0).max(0.0)),
                        );
                }
            }
            if let Some(ref he) = he {
                Self::clamp_panel(he);
            }
            for w in widgets() {
                if w.id() == panel_id {
                    w.on_open();
                    break;
                }
            }
        }
        Self::save_layout();
    }

    pub fn close(panel_id: &str) {
        debug_log!("panel close: {}", panel_id);
        if let Some(el) = El::from_id(panel_id) {
            el.remove_class("visible");
        }
        if let Some(btn_id) = Self::btn_for(panel_id)
            && let Some(btn) = El::from_id(btn_id)
        {
            btn.remove_class("active");
        }
        Self::save_layout();
    }

    pub fn toggle(panel_id: &str) {
        if let Some(el) = El::from_id(panel_id) {
            if el.has_class("visible") {
                Self::close(panel_id);
            } else {
                Self::open(panel_id);
            }
        }
    }

    pub fn is_open(panel_id: &str) -> bool {
        El::from_id(panel_id)
            .map(|p| p.has_class("visible"))
            .unwrap_or(false)
    }

    fn clamp_pos(panel: &HtmlElement, x: f64, y: f64) -> (f64, f64) {
        let vw = window()
            .inner_width()
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(800.0);
        let vh = window()
            .inner_height()
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(600.0);
        let pw = panel.offset_width() as f64;
        let ph = panel.offset_height() as f64;
        (
            x.max(0.0).min((vw - pw).max(0.0)),
            y.max(0.0).min((vh - ph).max(0.0)),
        )
    }

    fn clamp_panel(panel: &HtmlElement) {
        let style = panel.style();
        let x = style
            .get_property_value("left")
            .unwrap_or_default()
            .trim_end_matches("px")
            .parse::<f64>()
            .unwrap_or(0.0);
        let y = style
            .get_property_value("top")
            .unwrap_or_default()
            .trim_end_matches("px")
            .parse::<f64>()
            .unwrap_or(0.0);
        let (cx, cy) = Self::clamp_pos(panel, x, y);
        if (cx - x).abs() > 0.5 || (cy - y).abs() > 0.5 {
            let _ = style.set_property("left", &format!("{}px", cx));
            let _ = style.set_property("top", &format!("{}px", cy));
        }
    }

    pub fn clamp_all() {
        let doc = document();
        for w in widgets() {
            if !w.has_panel() {
                continue;
            }
            if let Some(el) = doc.get_element_by_id(w.id())
                && el.class_list().contains("visible")
                && let Ok(he) = el.dyn_into::<HtmlElement>()
                && he.dataset().get("userMoved").is_some()
            {
                Self::clamp_panel(&he);
            }
        }
    }

    fn find_drag_panel(target: web_sys::Element) -> Option<HtmlElement> {
        let mut cur = Some(target);
        while let Some(node) = cur {
            if node.class_list().contains("tui-close") {
                return None;
            }
            if node.class_list().contains("tui-title") {
                let parent = node.parent_element()?;
                if parent.class_list().contains("tui-widget") {
                    return parent.dyn_into().ok();
                }
                return None;
            }
            if node.class_list().contains("tui-widget") {
                return None;
            }
            cur = node.parent_element();
        }
        None
    }

    fn begin_drag(panel: &HtmlElement, mx: f64, my: f64) {
        let rect = panel.get_bounding_client_rect();
        let _ = panel.style().set_property("right", "");
        let _ = panel.style().set_property("bottom", "");
        let _ = panel
            .style()
            .set_property("left", &format!("{}px", rect.left()));
        let _ = panel
            .style()
            .set_property("top", &format!("{}px", rect.top()));
        let _ = panel.dataset().set("userMoved", "1");
        DRAG.with(|d| {
            *d.borrow_mut() = Some((panel.id(), mx, my, rect.left(), rect.top()));
        });
    }

    fn move_drag(panel_id: &str, ex: f64, ey: f64, ox: f64, oy: f64, mx: f64, my: f64) {
        if let Some(panel) = document().get_element_by_id(panel_id) {
            let he: HtmlElement = panel.dyn_into().unwrap();
            let (x, y) = MiniTUI::clamp_pos(&he, ex + mx - ox, ey + my - oy);
            let _ = he.style().set_property("left", &format!("{}px", x));
            let _ = he.style().set_property("top", &format!("{}px", y));
        }
    }

    pub fn init_drag() {
        document().on_capture("mousedown", |e: MouseEvent| {
            let target: web_sys::Element = match e.target().and_then(|t| t.dyn_into().ok()) {
                Some(t) => t,
                None => return,
            };
            if let Some(panel) = MiniTUI::find_drag_panel(target) {
                e.prevent_default();
                MiniTUI::begin_drag(&panel, e.client_x() as f64, e.client_y() as f64);
            }
        });
        document().on("mousemove", |e: MouseEvent| {
            DRAG.with(|d| {
                if let Some((ref id, ox, oy, ex, ey)) = *d.borrow() {
                    MiniTUI::move_drag(
                        id,
                        ex,
                        ey,
                        ox,
                        oy,
                        e.client_x() as f64,
                        e.client_y() as f64,
                    );
                }
            });
        });
        document().on("mouseup", |_: MouseEvent| {
            if DRAG.with(|d| d.borrow_mut().take().is_some()) {
                MiniTUI::save_layout();
            }
        });

        document().on_prevent("touchstart", |e: web_sys::TouchEvent| {
            let t = match e.touches().get(0) {
                Some(t) => t,
                None => return,
            };
            let target: web_sys::Element = match e.target().and_then(|t| t.dyn_into().ok()) {
                Some(t) => t,
                None => return,
            };
            if let Some(panel) = MiniTUI::find_drag_panel(target) {
                e.prevent_default();
                MiniTUI::begin_drag(&panel, t.client_x() as f64, t.client_y() as f64);
            }
        });
        document().on_prevent("touchmove", |e: web_sys::TouchEvent| {
            if !DRAG.with(|d| d.borrow().is_some()) {
                return;
            }
            let t = match e.touches().get(0) {
                Some(t) => t,
                None => return,
            };
            DRAG.with(|d| {
                if let Some((ref id, ox, oy, ex, ey)) = *d.borrow() {
                    MiniTUI::move_drag(
                        id,
                        ex,
                        ey,
                        ox,
                        oy,
                        t.client_x() as f64,
                        t.client_y() as f64,
                    );
                }
            });
            e.prevent_default();
        });
        document().on("touchend", |_: web_sys::TouchEvent| {
            if DRAG.with(|d| d.borrow_mut().take().is_some()) {
                MiniTUI::save_layout();
            }
        });
    }

    pub fn init_close_buttons() {
        let doc = document();
        if let Ok(btns) = doc.query_selector_all(".tui-close") {
            for i in 0..btns.length() {
                if let Some(node) = btns.item(i) {
                    let he: HtmlElement = node.clone().dyn_into().unwrap();
                    let mut pid = String::new();
                    let mut cur = he.parent_element();
                    while let Some(p) = cur {
                        if p.class_list().contains("tui-widget") {
                            pid = p.id();
                            break;
                        }
                        cur = p.parent_element();
                    }
                    if pid.is_empty() {
                        continue;
                    }
                    node.on("click", move |_: web_sys::Event| MiniTUI::close(&pid));
                }
            }
        }
    }

    pub fn init_panel_toggle_buttons() {
        for w in widgets() {
            if !w.has_panel() {
                continue;
            }
            let pid = w.id().to_string();
            if let Some(btn_id) = w.btn_id()
                && let Some(btn) = El::from_id(btn_id)
            {
                btn.on("click", move |_: web_sys::Event| MiniTUI::toggle(&pid));
            }
        }
    }

    pub fn save_layout() {
        let storage = match window().local_storage().ok().flatten() {
            Some(s) => s,
            None => return,
        };
        let doc = document();
        let panels_obj = js_sys::Object::new();
        for w in widgets() {
            if !w.has_panel() {
                continue;
            }
            let id = w.id();
            if let Some(el) = doc.get_element_by_id(id) {
                let he: HtmlElement = el.clone().dyn_into().unwrap();
                let entry = js_sys::Object::new();
                js_sys::Reflect::set(
                    &entry,
                    &"open".into(),
                    &el.class_list().contains("visible").into(),
                )
                .unwrap();
                if he.dataset().get("userMoved").is_some() {
                    let l = he.style().get_property_value("left").unwrap_or_default();
                    let t = he.style().get_property_value("top").unwrap_or_default();
                    js_sys::Reflect::set(&entry, &"left".into(), &l.into()).unwrap();
                    js_sys::Reflect::set(&entry, &"top".into(), &t.into()).unwrap();
                }
                js_sys::Reflect::set(&panels_obj, &id.into(), &entry.into()).unwrap();
            }
        }
        let state_obj = js_sys::Object::new();
        js_sys::Reflect::set(&state_obj, &"panels".into(), &panels_obj.into()).unwrap();

        let color = read_ui(|ui| ui.color.to_hex());
        js_sys::Reflect::set(&state_obj, &"color".into(), &color.into()).unwrap();

        let bm_arr = js_sys::Array::new();
        read_ui(|ui| {
            for (name, x, y) in &ui.bookmarks {
                let o = js_sys::Object::new();
                js_sys::Reflect::set(&o, &"n".into(), &name.as_str().into()).unwrap();
                js_sys::Reflect::set(&o, &"x".into(), &(*x).into()).unwrap();
                js_sys::Reflect::set(&o, &"y".into(), &(*y).into()).unwrap();
                bm_arr.push(&o.into());
            }
        });
        js_sys::Reflect::set(&state_obj, &"bookmarks".into(), &bm_arr.into()).unwrap();

        let json = js_sys::JSON::stringify(&state_obj.into())
            .unwrap()
            .as_string()
            .unwrap_or_default();
        let _ = storage.set_item("walloftext_state", &json);
    }

    pub fn load_layout() {
        let storage = match window().local_storage().ok().flatten() {
            Some(s) => s,
            None => return,
        };
        let raw = match storage.get_item("walloftext_state").ok().flatten() {
            Some(s) => s,
            None => {
                Self::open("panel-help");
                return;
            }
        };
        let parsed = match js_sys::JSON::parse(&raw) {
            Ok(v) => v,
            Err(_) => {
                Self::open("panel-help");
                return;
            }
        };

        if let Ok(c) = js_sys::Reflect::get(&parsed, &"color".into())
            && let Some(hex) = c.as_string()
        {
            with_ui(|ui| ui.color = walloftext_shared::Rgb::from_hex(&hex));
            mark_dirty(dirty::COLOR);
        }

        if let Ok(bm) = js_sys::Reflect::get(&parsed, &"bookmarks".into())
            && let Some(arr) = bm.dyn_ref::<js_sys::Array>()
        {
            with_ui(|ui| {
                ui.bookmarks.clear();
                for i in 0..arr.length() {
                    let o = arr.get(i);
                    let n = js_sys::Reflect::get(&o, &"n".into())
                        .ok()
                        .and_then(|v| v.as_string())
                        .unwrap_or_default();
                    let x = js_sys::Reflect::get(&o, &"x".into())
                        .ok()
                        .and_then(|v| v.as_f64())
                        .map(|v| walloftext_shared::clamp_coord(v as i64) as i64)
                        .unwrap_or(0);
                    let y = js_sys::Reflect::get(&o, &"y".into())
                        .ok()
                        .and_then(|v| v.as_f64())
                        .map(|v| walloftext_shared::clamp_coord(v as i64) as i64)
                        .unwrap_or(0);
                    ui.bookmarks.push((n, x, y));
                }
            });
        }

        if let Ok(panels_json) = js_sys::Reflect::get(&parsed, &"panels".into()) {
            for w in widgets() {
                if !w.has_panel() {
                    continue;
                }
                let id = w.id();
                if let Ok(info) = js_sys::Reflect::get(&panels_json, &id.into()) {
                    if info.is_undefined() || info.is_null() {
                        continue;
                    }
                    let open = js_sys::Reflect::get(&info, &"open".into())
                        .ok()
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let left = js_sys::Reflect::get(&info, &"left".into())
                        .ok()
                        .and_then(|v| v.as_string());
                    let top = js_sys::Reflect::get(&info, &"top".into())
                        .ok()
                        .and_then(|v| v.as_string());
                    if let Some(el) = El::from_id(id) {
                        let el = if let (Some(l), Some(t)) = (left, top) {
                            if !l.is_empty() {
                                el.css("left", &l)
                                    .css("top", &t)
                                    .remove_css("right")
                                    .remove_css("bottom")
                                    .dataset_set("userMoved", "1")
                            } else {
                                el
                            }
                        } else {
                            el
                        };
                        if let Ok(he) = el.node.clone().dyn_into::<HtmlElement>()
                            && he.dataset().get("userMoved").is_some()
                        {
                            Self::clamp_panel(&he);
                        }
                        if open {
                            el.add_class("visible");
                            if let Some(btn_id) = w.btn_id() {
                                El::from_id(btn_id).map(|e| e.add_class("active"));
                            }
                        }
                    }
                }
            }
        }

        render_all();
    }
}
