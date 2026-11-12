//! How providers look: their logos, and the provider boxes on Home (each
//! provider's accounts summed: their limits, credits, or "free").

use super::*;
use crate::composer::{framed_box, kv, meter};
use crate::providers::{Kind, Metered};
use crate::widgets::chevron_v;

/// Provider `p`'s logo (from assets/), as a texture.
pub(crate) fn logo(ctx: &egui::Context, p: Kind) -> egui::TextureHandle {
    let id = Id::unique(("provider-logo", p.id()));
    if let Some(tex) = ctx.data(|d| d.get_temp::<egui::TextureHandle>(id)) {
        return tex;
    }
    let bytes: &[u8] = match p {
        Kind::Claude => include_bytes!("../assets/logo-claude.png"),
        Kind::Codex => include_bytes!("../assets/logo-codex.png"),
        Kind::OpenRouter => include_bytes!("../assets/logo-openrouter.png"),
        Kind::NodeClub => include_bytes!("../assets/logo-nodeclub.png"),
        Kind::Freebuff => include_bytes!("../assets/logo-freebuff.png"),
        Kind::Local => include_bytes!("../assets/logo-local.png"),
    };
    let img = image::load_from_memory(bytes).expect("built-in logo decodes").to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let tex = ctx.load_texture(format!("logo-{}", p.id()), egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw()), egui::TextureOptions::LINEAR);
    ctx.data_mut(|d| d.insert_temp(id, tex.clone()));
    tex
}

/// Paints `p`'s logo in `rect`.
pub(crate) fn paint_logo(ui: &Ui, p: Kind, rect: Rect) {
    let tex = logo(ui.ctx(), p);
    ui.painter().image(tex.id(), rect, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
}

/// `p`'s logo as an image `size` points square (for buttons).
pub(crate) fn logo_image(ctx: &egui::Context, p: Kind, size: f32) -> egui::Image<'static> {
    egui::Image::new(&logo(ctx, p)).fit_to_exact_size(vec2(size, size))
}

/// A limit summed over a provider's accounts: its label, the percent used
/// of all of them together (out of 100, each account counting by its
/// weight: `weight`), how many accounts have it, and when the first of
/// them resets.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Summed {
    pub(crate) label: String,
    pub(crate) used: f32,
    pub(crate) accounts: usize,
    pub(crate) next_reset: Option<i64>,
}

/// How much an account's limits count against its provider's others, by
/// its plan's multiplier over the base plan (Claude Pro, ChatGPT Plus).
/// A plan whose size isn't known counts as the smaller one it may be.
pub(crate) fn weight(a: &api::Account) -> f32 {
    let plan = a.plan.to_ascii_lowercase();
    match a.provider {
        // "Max 5x", "Max 20x", or "Max" from before OAK told them apart.
        Kind::Claude if plan.starts_with("max") => if plan.ends_with("20x") { 20.0 } else { 5.0 },
        // Team seats: Premium 6.25x Pro, Standard 1.25x ("claude_team" from
        // before OAK told them apart counts as Standard).
        Kind::Claude if plan == "team premium" => 6.25,
        Kind::Claude if plan == "team standard" || plan == "claude_team" => 1.25,
        // ChatGPT's raw plan types: "pro" is Pro 20x ($200), "prolite" Pro
        // 5x ($100), "promax" a larger Pro; a Business premium seat 5x.
        Kind::Codex => match plan.as_str() {
            "pro" | "promax" => 20.0,
            p if p.ends_with("prolite") => 5.0,
            _ => 1.0,
        },
        _ => 1.0,
    }
}

/// Each limit's label summed over `limits` (one list per account, with its
/// weight), in the order they first come, at most two.
pub(crate) fn sum_limits<'a>(limits: impl IntoIterator<Item = (&'a [api::Limit], f32)>, now: i64) -> Vec<Summed> {
    // The weighted percents and the weights, beside `out`.
    let mut out: Vec<Summed> = vec![];
    let mut totals: Vec<(f32, f32)> = vec![];
    for (list, w) in limits {
        for l in list.iter().take(2) {
            // A limit whose reset has passed is unused again.
            let used = if l.resets_at.is_some_and(|r| r <= now) { 0.0 } else { l.percent.clamp(0.0, 100.0) };
            let reset = l.resets_at.filter(|&r| r > now);
            match out.iter().position(|s| s.label == l.label) {
                Some(i) => {
                    let s = &mut out[i];
                    totals[i].0 += used * w;
                    totals[i].1 += w;
                    s.accounts += 1;
                    s.next_reset = match (s.next_reset, reset) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                }
                None => {
                    out.push(Summed { label: l.label.to_string(), used, accounts: 1, next_reset: reset });
                    totals.push((used * w, w));
                }
            }
        }
    }
    for (s, (used, w)) in out.iter_mut().zip(totals) {
        s.used = if w > 0.0 { used / w } else { 0.0 };
    }
    out.truncate(2);
    out
}

/// What a provider box shows under its account count.
pub(crate) enum Shown {
    Limits(Vec<Summed>),
    /// Dollars left (when known), spent through Oak, all told, and why
    /// what is left couldn't be checked.
    Credits(Option<f64>, f64, Option<String>),
    Free,
    /// Not known yet.
    Loading,
    /// Nothing to sum: why.
    Note(&'static str),
}

/// Provider `p`'s box on Home: its name, "N accounts", what its accounts
/// have summed, its logo at the top right and a caret at the bottom
/// (`open`: its accounts show below). The size of an account box.
pub(crate) fn provider_box(ui: &mut Ui, t: &Theme, p: Kind, accounts: usize, shown: &Shown, open: bool, w: f32) -> egui::Response {
    let r = framed_box(ui, t, t.line, w, |ui| {
        let small = |s: &str, c: Color32| RichText::new(s.to_string()).small().color(c);
        // The name, and the logo level with it in the top right corner
        // (as far from the right edge as the text is from the left).
        ui.horizontal(|ui| {
            ui.set_height(14.0);
            ui.label(small(p.name(), t.ink2));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.add(logo_image(ui.ctx(), p, 14.0)));
        });
        let n = if accounts == 1 { "1 account".to_string() } else { format!("{accounts} accounts") };
        ui.label(small(&n, t.ink3));
        let w = ui.available_width();
        match shown {
            Shown::Limits(sums) => {
                for s in sums {
                    let full = s.used >= 90.0;
                    let reset = s.next_reset.map(|r| format!(", next resets in {}", crate::time::until(r))).unwrap_or_default();
                    let r = ui
                        .scope(|ui| {
                            ui.spacing_mut().item_spacing.y = 1.0;
                            kv(ui, t, &s.label, &format!("{}%", s.used.floor()), if full { t.error } else { t.ink });
                            meter(ui, t, w, s.used / 100.0, if full { t.error } else { t.accent });
                        })
                        .response;
                    let how = match p {
                        Kind::Claude => ", each plan counting by its size (Pro 1, Team Standard 1.25, Max 5x 5, Team Premium 6.25, Max 20x 20)",
                        Kind::Codex => ", each plan counting by its size (Plus 1, Pro 5x 5, Pro 20x 20)",
                        _ => "",
                    };
                    r.on_hover_text(format!("{}% of the {} {} limits used, all together{how}{reset}.", s.used.floor(), s.accounts, s.label));
                }
            }
            Shown::Credits(left, spent, error) => {
                match (left, error) {
                    (Some(l), _) => kv(ui, t, "Available credits", &openrouter::dollars(*l), if *l < 1.0 { t.error } else { t.ink }),
                    (None, Some(e)) => kv(ui, t, "Available credits", "unknown", t.error).on_hover_text(e.as_str()),
                    (None, None) => kv(ui, t, "Available credits", "…", t.ink3),
                }
                .on_hover_text("Credits left on the keys' accounts, less what Oak spent since they were checked");
                kv(ui, t, "Credits spent", &openrouter::dollars(*spent), t.ink).on_hover_text("Spent through Oak, all time");
            }
            Shown::Free => {
                ui.label(small("free", t.ink3));
            }
            Shown::Loading => {
                ui.label(small("loading usage…", t.ink3));
            }
            Shown::Note(n) => {
                ui.label(small(n, t.ink3));
            }
        }
    })
    .response;
    // The caret, in the bottom padding.
    let resp = ui.interact(r.rect, Id::unique(("provider-box", p.id())), Sense::click());
    let lit = resp.hovered();
    if lit {
        ui.painter().rect_stroke(r.rect, 0.0, Stroke::new(1.0, t.ink3), egui::StrokeKind::Inside);
    }
    chevron_v(ui.painter(), egui::pos2(r.rect.center().x, r.rect.bottom() - 4.5), open, if lit { t.ink } else { t.ink3 });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// What `p`'s accounts add up to, for its box.
pub(crate) fn shown(p: Kind) -> Shown {
    let now = api::now_ms();
    match p.metered() {
        Metered::Free => Shown::Free,
        Metered::Credits => match openrouter::account() {
            Some(a) => Shown::Credits(a.left, a.spent, a.error),
            None => Shown::Loading,
        },
        Metered::Limits => {
            let all: Vec<api::Account> = match p {
                Kind::Codex => codex::accounts().into_iter().map(|(_, a)| a).collect(),
                _ => api::accounts(),
            };
            // API keys have no limits.
            let plans: Vec<&api::Account> = all.iter().filter(|a| !a.api_key).collect();
            if plans.is_empty() {
                return Shown::Note("API keys, pay as you go");
            }
            let sums = sum_limits(plans.iter().map(|a| (a.limits.as_slice(), weight(a))), now);
            match () {
                _ if !sums.is_empty() => Shown::Limits(sums),
                _ if plans.iter().all(|a| a.fetched) => Shown::Note("usage unavailable"),
                _ => Shown::Loading,
            }
        }
    }
}

/// How many accounts `p` has.
pub(crate) fn account_count(p: Kind) -> usize {
    match p {
        Kind::Claude => api::accounts().len(),
        Kind::Codex => codex::accounts().len(),
        Kind::OpenRouter => openrouter::accounts().len(),
        Kind::NodeClub => crate::nodeclub::accounts().len(),
        Kind::Freebuff => crate::freebuff::accounts().len(),
        Kind::Local => local::on() as usize,
    }
}

/// Paints `color` fading in over `rect`, from none at its top to full at
/// its bottom (a list running out under a pinned button).
pub(crate) fn fade(p: &egui::Painter, rect: Rect, color: Color32) {
    let mut mesh = egui::Mesh::default();
    let clear = Color32::from_rgba_unmultiplied(color.r(), color.g(), color.b(), 0);
    mesh.colored_vertex(rect.left_top(), clear);
    mesh.colored_vertex(rect.right_top(), clear);
    mesh.colored_vertex(rect.left_bottom(), color);
    mesh.colored_vertex(rect.right_bottom(), color);
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(1, 2, 3);
    p.add(mesh);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limit(label: &'static str, percent: f32, resets_at: Option<i64>) -> api::Limit {
        api::Limit { label, percent, resets_at }
    }

    /// Five accounts used up on the 5-hour limit are 100% of 100%.
    #[test]
    fn limits_add_up_over_the_accounts() {
        let now = 1_000;
        let lists: Vec<Vec<api::Limit>> = (0..5).map(|i| vec![limit("5h", 100.0, Some(now + 10 + i)), limit("Week", 20.0, Some(now + 100))]).collect();
        let s = sum_limits(lists.iter().map(|l| (l.as_slice(), 1.0)), now);
        assert_eq!(s[0], Summed { label: "5h".into(), used: 100.0, accounts: 5, next_reset: Some(now + 10) });
        assert_eq!((s[1].used, s[1].accounts), (20.0, 5));
        // A reset that has passed counts as unused.
        let past = [vec![limit("5h", 80.0, Some(now - 1))], vec![limit("5h", 30.0, None)]];
        let s = sum_limits(past.iter().map(|l| (l.as_slice(), 1.0)), now);
        assert_eq!((s[0].used, s[0].accounts, s[0].next_reset), (15.0, 2, None));
    }

    /// Accounts count by their plans' multipliers: a used-up Max 5x beside
    /// an unused Pro is 5/6 used.
    #[test]
    fn plans_count_by_their_multipliers() {
        let lists = [(vec![limit("5h", 100.0, None)], 5.0), (vec![limit("5h", 0.0, None)], 1.0)];
        let s = sum_limits(lists.iter().map(|(l, w)| (l.as_slice(), *w)), 0);
        assert!((s[0].used - 500.0 / 6.0).abs() < 1e-3);
        let w = |provider, plan: &str| weight(&api::Account { provider, plan: plan.into(), ..Default::default() });
        assert_eq!([w(Kind::Claude, "Max 20x"), w(Kind::Claude, "Max 5x"), w(Kind::Claude, "Max"), w(Kind::Claude, "Pro")], [20.0, 5.0, 5.0, 1.0]);
        assert_eq!([w(Kind::Claude, "Team Premium"), w(Kind::Claude, "Team Standard"), w(Kind::Claude, "claude_team")], [6.25, 1.25, 1.25]);
        assert_eq!(
            [api::team_seat("team_tier_1", "default_claude_max_5x"), api::team_seat("", "default_raven"), api::team_seat("", "default_claude_max_5x"), api::team_seat("team_standard", "")],
            ["Team Premium", "Team Standard", "Team Premium", "Team Standard"]
        );
        assert_eq!([w(Kind::Codex, "Pro"), w(Kind::Codex, "Prolite"), w(Kind::Codex, "Self_serve_business_prolite"), w(Kind::Codex, "Plus")], [20.0, 5.0, 5.0, 1.0]);
    }
}
