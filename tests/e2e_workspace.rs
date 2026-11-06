//! The reference cachet binary against the demo transcript in
//! crates/edgefront/tests/fixtures/e2e_workspace.txt, which the edgefront
//! binary must match too (crates/edgefront/tests/e2e_workspace.rs).

#[path = "../crates/edgefront/tests/e2e_workspace/scenario.rs"]
mod scenario;

#[test]
fn demo_steps_match_the_fixture() {
    scenario::check(&scenario::Bin {
        path: env!("CARGO_BIN_EXE_cachet"),
        program: "cachet",
        fixture: concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/crates/edgefront/tests/fixtures/e2e_workspace.txt"
        ),
    });
}
