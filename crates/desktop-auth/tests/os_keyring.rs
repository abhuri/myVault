use myvault_desktop_auth::{OsKeyringStore, SecretStore};
use secrecy::{ExposeSecret, SecretString};

#[test]
#[ignore = "mutates the current OS credential store with an ephemeral test entry"]
fn os_keyring_round_trip_and_logout() {
    let store = OsKeyringStore::new("com.abhuri.myvault.phase0-test");
    let account = "ephemeral-keyring-probe";
    let token = SecretString::from("not-a-real-token-phase0".to_owned());

    store.delete_refresh_token(account).unwrap();
    store.save_refresh_token(account, &token).unwrap();
    let loaded = store.load_refresh_token(account).unwrap().unwrap();
    assert_eq!(loaded.expose_secret(), token.expose_secret());
    store.delete_refresh_token(account).unwrap();
    assert!(store.load_refresh_token(account).unwrap().is_none());
}
