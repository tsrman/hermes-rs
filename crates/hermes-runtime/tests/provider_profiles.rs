use hermes_runtime::config::load_chat_settings;

fn home(config: &str, dotenv: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.yaml"), config).unwrap();
    std::fs::write(dir.path().join(".env"), dotenv).unwrap();
    dir
}

#[test]
fn named_providers_resolve_endpoint_mode_and_key() {
    let dir = home(
        "model: {default: m, provider: gmi_cloud}\n",
        "GMI_API_KEY=gmi-secret\n",
    );
    let settings = load_chat_settings(Some(dir.path())).unwrap();
    assert_eq!(settings.provider, "gmi");
    assert_eq!(
        settings.base_url.as_deref(),
        Some("https://api.gmi-serving.com/v1")
    );
    assert!(!settings.responses);
    let url = settings.base_url.clone().unwrap();
    assert_eq!(
        settings.vendor_key_for("gmi", &url).as_deref(),
        Some("gmi-secret")
    );
    // The key goes only to the provider's own host, never to another endpoint.
    assert_eq!(settings.vendor_key_for("gmi", "https://evil.test/v1"), None);
    assert_eq!(
        settings.vendor_key_for("gmi", "https://api.gmi-serving.com.evil.test/v1"),
        None
    );
}

#[test]
fn an_endpoint_override_in_the_profile_env_is_paired_with_the_key() {
    let dir = home(
        "model: {default: m, provider: hf}\n",
        "HF_TOKEN=hf-secret\n",
    );
    let settings = load_chat_settings(Some(dir.path())).unwrap();
    assert_eq!(settings.provider, "huggingface");
    assert_eq!(
        settings.base_url.as_deref(),
        Some("https://router.huggingface.co/v1")
    );

    let dir = home(
        "model: {default: m, provider: novita}\n",
        "NOVITA_API_KEY=n-secret\nNOVITA_BASE_URL=http://127.0.0.1:9000/v1\n",
    );
    let settings = load_chat_settings(Some(dir.path())).unwrap();
    assert_eq!(
        settings.base_url.as_deref(),
        Some("http://127.0.0.1:9000/v1")
    );
    assert_eq!(
        settings
            .vendor_key_for("novita", "http://127.0.0.1:9000/v1")
            .as_deref(),
        Some("n-secret")
    );
    // A configured base_url wins over the environment override.
    let dir = home(
        "model: {default: m, provider: novita, base_url: 'https://proxy.test/v1'}\n",
        "NOVITA_API_KEY=n-secret\nNOVITA_BASE_URL=http://127.0.0.1:9000/v1\n",
    );
    assert_eq!(
        load_chat_settings(Some(dir.path()))
            .unwrap()
            .base_url
            .as_deref(),
        Some("https://proxy.test/v1")
    );
}

#[test]
fn responses_providers_and_explicit_api_modes() {
    let dir = home("model: {default: m, provider: grok}\n", "XAI_API_KEY=x\n");
    let settings = load_chat_settings(Some(dir.path())).unwrap();
    assert_eq!((settings.provider, settings.responses), ("xai", true));
    let dir = home(
        "model: {default: m, provider: xai, api_mode: chat_completions}\n",
        "",
    );
    assert!(!load_chat_settings(Some(dir.path())).unwrap().responses);
}

#[test]
fn fallbacks_accept_named_providers_and_unknown_ones_are_rejected() {
    let dir = home(
        "model: {default: m, provider: custom, base_url: 'http://x/v1'}\nfallback_providers:\n  - {provider: stepfun, model: s}\n  - {provider: deepseek, model: d}\n",
        "STEPFUN_API_KEY=s-secret\n",
    );
    let settings = load_chat_settings(Some(dir.path())).unwrap();
    // DeepSeek needs request handling this runtime lacks, so only StepFun is kept.
    assert_eq!(settings.fallbacks.len(), 1);
    assert_eq!(settings.fallbacks[0].provider, "stepfun");
    assert_eq!(settings.fallbacks[0].api_key.as_deref(), Some("s-secret"));
    let dir = home("model: {default: m, provider: deepseek}\n", "");
    assert!(load_chat_settings(Some(dir.path())).is_err());
}
