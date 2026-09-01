use super::*;

pub(super) fn load_source_graph(graph: ConfigSourceGraph) -> Result<LoadedConfig> {
    let (mut config, source_files, source_contents, processed) =
        decode::decode_source_graph(graph)?;
    validate_core::validate(&mut config)?;
    validate_runtime::validate(&mut config)?;
    validate_me::validate(&mut config)?;
    validate_server::validate(&mut config)?;
    // Which WEB transport the operator asked for is settled before either one
    // starts complaining about its own half-written section.
    config
        .fork
        .validate_selection(config.telemt_web_requested())?;
    validate_web::validate(&mut config)?;
    // Fork-only features validate themselves; the rest of the document is
    // stock telemt and was already checked above.
    config.fork.validate(config.telemt_web_requested())?;
    // The panel is a client of `[server.api]`, so it is checked after the
    // server section has settled and before the effective values are applied.
    config.panel.validate()?;
    validate_panel_requires_control_api(&config)?;
    effective::apply(&mut config)?;
    Ok(LoadedConfig {
        config,
        source_files: source_files.into_iter().collect(),
        source_contents,
        rendered_hash: hash_rendered_snapshot(&processed),
    })
}
