//! Devin IDE `acp-messages` store integration tests (Phase B).
#![cfg(feature = "devin")]

#[path = "fixtures/devin_acp/mod.rs"]
mod devin_acp;

use tempfile::TempDir;

#[test]
fn fixtures_create_valid_main_subagent_and_empty_stores() {
    let dir = TempDir::new().expect("tempdir");
    let main_db = dir.path().join("11111111-1111-4111-8111-111111111111.db");
    let sub_db = dir.path().join("22222222-2222-4222-8222-222222222222.db");
    let empty_no_msg = dir.path().join("33333333-3333-4333-8333-333333333333.db");
    let empty_zero = dir.path().join("44444444-4444-4444-8444-444444444444.db");

    devin_acp::build_main_session_db(&main_db);
    devin_acp::build_subagent_session_db(&sub_db);
    devin_acp::build_empty_store_db(&empty_no_msg, true, false);
    devin_acp::build_empty_store_db(&empty_zero, false, false);

    assert_eq!(devin_acp::count_tables(&main_db), 2);
    assert_eq!(devin_acp::count_tables(&sub_db), 2);
    assert_eq!(devin_acp::count_tables(&empty_no_msg), 1);
    assert_eq!(std::fs::metadata(&empty_zero).expect("zero meta").len(), 0);
}
