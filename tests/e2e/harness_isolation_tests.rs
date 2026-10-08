//! The e2e harness must keep every gateway off the developer's real `~/.syscity`.
//!
//! This is a guard, not a feature test. Nothing else catches the failure mode it
//! covers: a gateway that resolves the real home writes sessions, memory and
//! credentials into a machine's actual state, CI cannot see it (a fresh
//! container has no real `~/.syscity` to damage), and locally the damage is
//! invisible until a value persisted by one run changes the behaviour of the
//! next — which is exactly how a provider credential stored by one run kept a
//! provider authorized in the following one.
//!
//! So the assertions are deliberately about the *shape* of the isolation, not
//! about this or that gateway: the layout root the harness hands out is under
//! the temp dir, it is the process default, and a gateway started through the
//! harness actually writes there.

use super::*;

#[tokio::test]
#[serial]
async fn the_harness_keeps_gateways_off_the_real_home() {
    let root = test_paths_root();

    // 1. The root the harness hands out is a temp dir, not `<home>/.syscity`.
    assert!(
        root.root().starts_with(std::env::temp_dir()),
        "the harness root is not under the temp dir: {}",
        root.root().display()
    );

    // 2. It is also the process default, so tools calling the `dirs::…` free
    //    functions agree with the gateways instead of resolving the real home.
    assert_eq!(
        syscity::dirs::syscity_dir(),
        root.root(),
        "the process default layout root is not the harness root"
    );

    // 3. And a gateway started through the harness really does use it: the
    //    layout directories are created under the root at startup. Without this
    //    the first two assertions would still hold if the harness stopped
    //    passing `paths` and the gateways quietly fell back to the real home.
    let port = free_port();
    start_test_gateway(port, false).await;

    let entries = std::fs::read_dir(root.root())
        .expect("the harness root should exist")
        .count();
    assert!(
        entries > 0,
        "no gateway wrote anything to the harness root — is it still being passed?"
    );
}
