//! End to end: examples/demo.sh's five steps against the edgefront binary,
//! with every Cache-Status checked byte for byte against
//! tests/fixtures/e2e_workspace.txt. The reference cachet binary runs the
//! same steps against the same fixture in the root package's
//! tests/e2e_workspace.rs. See tests/e2e_workspace/scenario.rs.

#[path = "e2e_workspace/scenario.rs"]
mod scenario;

#[test]
fn demo_steps_match_the_fixture() {
    scenario::check(&scenario::Bin {
        path: env!("CARGO_BIN_EXE_edgefront"),
        program: "edgefront",
        fixture: concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/e2e_workspace.txt"
        ),
    });
}
