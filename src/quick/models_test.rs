//! Checks the model list the picker is built from, against the real profile
//! (read only): every provider's models, and Quick's among them.
//!
//! OAK_DIR=<profile> cargo test --lib quick_models -- --ignored --nocapture

#[test]
#[ignore]
fn quick_models_are_listed() {
    let dir = std::env::var("OAK_DIR").expect("set OAK_DIR to the real profile");
    println!("profile: {dir}");
    crate::api::load_auth();
    println!("quick on: {}", crate::quick::on());
    println!("quick account: {:?}", crate::quick::account().map(|a| (a.email, a.label)));
    println!("quick host saved: {:?}", crate::quick::login::saved_host());
    match crate::quick::login::host() {
        Ok(h) => println!("host: {h}"),
        Err(e) => println!("host ERR: {e}"),
    }
    let quick: Vec<_> = crate::quick::MODELS.iter().map(|m| m.id).collect();
    println!("quick models declared: {quick:?}");
    let listed: Vec<String> = crate::api::known_models().into_iter().map(|m| format!("{} ({})", m.id, m.provider.id())).collect();
    println!("\nknown_models ({}):", listed.len());
    for m in &listed {
        println!("  {m}");
    }
    let has_quick = crate::api::known_models().iter().any(|m| m.provider == crate::providers::Kind::Quick);
    println!("\nQuick in the picker: {has_quick}");
    assert!(has_quick, "Quick's models are missing from the picker");
}
