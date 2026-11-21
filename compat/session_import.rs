//! Run as an integration test in a candidate's `hermes-agent` crate.
//! HERMES_COMPAT_PROFILE must identify a disposable Python fixture profile.

use hermes_agent::session_persistence::SessionPersistence;

#[test]
fn reads_python_session_from_existing_profile() {
    let profile = std::env::var_os("HERMES_COMPAT_PROFILE")
        .expect("HERMES_COMPAT_PROFILE must point to a disposable Python fixture");
    let store = SessionPersistence::new(profile);
    let messages = store
        .load_session("compat-session")
        .expect("candidate should read the Python fixture");
    let contents: Vec<_> = messages
        .iter()
        .map(|message| message.content.as_deref())
        .collect();
    assert_eq!(contents, [Some("Привет, Hermes"), Some("Здравствуйте!")]);
}
