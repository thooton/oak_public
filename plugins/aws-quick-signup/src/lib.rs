//! The AWS Quick sign-up entry on Home. AWS owns the forms and verification;
//! Oak's private browser shows them and sends the user's Chromium input.

use oak_sdk::egui::{self, Align, Layout, Rect, Ui, UiBuilder, pos2, vec2};
use oak_sdk::view::{Action, Notice, Page, Tone};
use oak_sdk::{App, HomeTab, Hooks, Info, Plugin, Registrar};

const ID: &str = "aws-quick-signup";
const ENTRY: &str = "https://signup.aws.amazon.com/billing/signup/service?app=AmazonQuickSuite&tier=free&funnel=boost#/validation";

struct Signup;

oak_sdk::export_plugin!(Signup);

impl Plugin for Signup {
    fn info(&self) -> Info {
        Info::new(
            ID,
            "AWS Quick sign-up",
            env!("CARGO_PKG_VERSION"),
            "Create an AWS Quick account in Oak's private browser.",
        )
    }

    fn register(&self, registrar: &mut Registrar) {
        registrar.home_tab(SignupPage);
        registrar.hooks(HomeButton);
    }
}

struct HomeButton;

impl Hooks for HomeButton {
    fn home_scene(&self, ui: &mut Ui, app: &mut dyn App) {
        let home = ui.max_rect();
        let top_right = pos2(home.right() - 20.0, home.top() + 18.0);
        let corner = Rect::from_min_size(top_right - vec2(180.0, 0.0), vec2(180.0, 32.0));
        let clicked = ui
            .scope_builder(
                UiBuilder::new()
                    .max_rect(corner)
                    .layout(Layout::right_to_left(Align::Center)),
                |ui| Action::primary("AWS Quick sign-up").show(ui).clicked(),
            )
            .inner;
        if clicked {
            app.go_page(ID);
        }
    }
}

struct SignupPage;

impl HomeTab for SignupPage {
    fn id(&self) -> String {
        ID.into()
    }
    fn label(&self) -> String {
        "AWS Quick sign-up".into()
    }
    fn link(&self) -> bool {
        true
    }
    fn page(&self) -> bool {
        true
    }

    fn show(&self, ui: &mut Ui, app: &mut dyn App) {
        let back = Page::new("AWS Quick sign-up")
            .subtitle(
                "Create an AWS Quick account. To connect it to Oak, sign in from Home separately.",
            )
            .back("Home")
            .width(1200.0)
            .no_scroll()
            .actions(|ui: &mut egui::Ui| {
                ui.hyperlink_to("Open in default browser", ENTRY);
            })
            .show(ui, |ui| {
                if let Err(error) = app.private_browser(ui, ENTRY) {
                    Notice::new(Tone::Bad, &error).show(ui);
                }
            });
        if back {
            app.go_home("");
        }
    }
}
