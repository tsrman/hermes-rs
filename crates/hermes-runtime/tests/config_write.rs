use std::fs;
use std::path::Path;
use std::sync::Barrier;

use hermes_runtime::config::set_config_value;
use serde_yaml::Value;

fn read_config(home: &Path) -> Value {
    serde_yaml::from_str(&fs::read_to_string(home.join("config.yaml")).unwrap()).unwrap()
}

#[test]
fn updates_literal_keys_and_list_items_without_changing_other_values() {
    let home = tempfile::tempdir().unwrap();
    fs::write(
        home.path().join("config.yaml"),
        "model: fixture\nplugins:\n  versions:\n    example.1: {active: false, keep: [a, b]}\n  routes: [{name: first}, {name: second}]\n  template: '${PLUGIN_TOKEN}'\n  empty: null\n",
    )
    .unwrap();
    set_config_value(home.path(), "plugins.versions.example.1.active", "yes").unwrap();
    set_config_value(
        home.path(),
        r"plugins.versions.example\.2",
        "{threshold: 2}",
    )
    .unwrap();
    set_config_value(home.path(), "plugins.routes.-1.name", "replacement").unwrap();
    set_config_value(home.path(), "model.base_url", "").unwrap();

    let expected: Value = serde_yaml::from_str(
        "model: {default: fixture, base_url: ''}\nplugins:\n  versions:\n    example.1: {active: true, keep: [a, b]}\n    example.2: {threshold: 2}\n  routes: [{name: first}, {name: replacement}]\n  template: '${PLUGIN_TOKEN}'\n  empty: null\n",
    )
    .unwrap();
    assert_eq!(read_config(home.path()), expected);
}

#[test]
fn leaves_the_original_bytes_on_an_invalid_document_or_update() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.yaml");
    for content in ["broken: [unterminated\n", "- scalar\n"] {
        fs::write(&path, content).unwrap();
        assert!(set_config_value(home.path(), "model.default", "fixture").is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }

    let content = "plugin:\n  flags: [on]\n  label: original\n";
    fs::write(&path, content).unwrap();
    for (key, value) in [
        ("HERMES_API_KEY", "fixture"),
        ("plugin..label", "changed"),
        ("plugin.flags.1", "missing"),
        ("plugin.flags", "[unterminated"),
        ("plugin.label.subkey", "changed"),
        ("plugin", "{label: changed}"),
    ] {
        assert!(set_config_value(home.path(), key, value).is_err(), "{key}");
        assert_eq!(fs::read_to_string(&path).unwrap(), content);
    }
}

#[test]
fn serializes_concurrent_updates_to_a_new_profile() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("new-profile");
    let workers = 8;
    let start = Barrier::new(workers);
    std::thread::scope(|scope| {
        for index in 0..workers {
            let home = &home;
            let start = &start;
            scope.spawn(move || {
                start.wait();
                set_config_value(home, &format!("plugin.item_{index}"), &index.to_string())
                    .unwrap();
            });
        }
    });

    let config = read_config(&home);
    assert_eq!(config["plugin"].as_mapping().unwrap().len(), workers);
    for index in 0..workers {
        assert_eq!(
            config["plugin"][format!("item_{index}")],
            Value::from(index)
        );
    }
}
