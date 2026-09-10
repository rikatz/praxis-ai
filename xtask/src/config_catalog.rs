// SPDX-License-Identifier: Apache-2.0
//! Generate and lint the machine-readable Praxis AI catalog fragment.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};

use clap::Parser;
use praxis_config_catalog::SchemaId;
use praxis_config_catalog_generator::{
    self as catalog_generator, RequirementHint as GeneratorRequirementHint, SchemaPolicy, SourceField, SourceModel,
};

use super::filter_docs::*;

/// Generator-owned unions for factories that intentionally dispatch between
/// multiple serde shapes under one registered filter name. MCP's public
/// `mcp` factory selects broker mode when `servers` is present; this is a
/// source-level dispatch, not two registry entries. The emitted schema is a
/// typed `one_of`, preserving both configuration contracts without changing
/// runtime registration.
const MCP_FACTORY_UNIONS: &[(&str, &str, &str)] = &[("mcp", "McpConfig", "McpBrokerConfig")];

/// Arguments for catalog generation.
#[derive(Parser)]
pub(crate) struct GenerateArgs {}

/// Arguments for catalog linting.
#[derive(Parser)]
pub(crate) struct LintArgs {}

pub(crate) fn generate(_args: GenerateArgs) {
    let root = workspace_root();
    let path = root.join("docs/catalog/config-catalog.json");
    let bytes = render_catalog_impl(&root);
    fs::create_dir_all(path.parent().expect("catalog parent")).expect("create catalog directory");
    fs::write(&path, bytes).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    println!("wrote {}", path.display());
}

pub(crate) fn lint(_args: LintArgs) {
    let root = workspace_root();
    let _ = render_catalog_impl(&root);
    println!("configuration catalog generation is valid");
}

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask has workspace parent")
        .to_path_buf()
}

// -----------------------------------------------------------------------------
// Machine-readable catalog
// -----------------------------------------------------------------------------

/// Render the AI-owned catalog fragment from the same parsed source metadata
/// used by the filter documentation generator. Keeping this seam here makes
/// the human and machine-readable descriptions fail together when a config
/// shape changes.
pub(crate) fn render_catalog_impl(root: &Path) -> Vec<u8> {
    use praxis_config_catalog::{
        CatalogCompatibility, CatalogFormatVersion, CatalogFragment, ConfigExample, ConfigSchema, FilterDescriptor,
        ProducerComponent, ProducerInfo, Protocol, SchemaNode, VersionRequirement,
    };

    let shared = parse_shared_config_items(root);
    validate_factory_variants(root, MCP_FACTORY_UNIONS).unwrap_or_else(|error| panic!("{error}"));
    let entries = discover_all_filters(root, &shared);
    let registry = praxis_ai_filters::build_ai_registry();
    validate_yaml_examples(&entries).unwrap_or_else(|error| panic!("{error}"));
    let mut feature_profile = ai_feature_profile(root).expect("valid filters Cargo.toml");
    feature_profile.available.extend(
        ["apis", "experimental", "opentelemetry", "praxis-main"]
            .into_iter()
            .map(str::to_owned),
    );
    for entry in &entries {
        feature_profile
            .available
            .extend(required_features_for(root, &entry.filter));
    }
    feature_profile.default_enabled.insert("apis".to_owned());
    validate_registry_parity(root, &entries, &registry, &active_catalog_features());
    let mut fragment = CatalogFragment {
        format_version: CatalogFormatVersion { major: 1, minor: 0 },
        producer: ProducerInfo {
            component: ProducerComponent::Ai,
            package: "praxis-ai".to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            source_revision: std::env::var("PRAXIS_SOURCE_REVISION")
                .ok()
                .filter(|revision| !revision.trim().is_empty()),
        },
        compatibility: CatalogCompatibility {
            requires_format_major: 1,
            requires_core: Some(VersionRequirement::from("^0.5.4")),
        },
        feature_profile,
        schemas: BTreeMap::new(),
        roots: Vec::new(),
        filters: Vec::new(),
        diagnostics: Vec::new(),
    };

    for entry in entries {
        let schema_id = SchemaId::from(format!("ai.filter.http.{}.{}", entry.category, entry.filter.name));
        let node = if entry.filter.name == "mcp" && entry.filter.variants.len() > 1 {
            let mut variants = Vec::new();
            for variant in &entry.filter.variants {
                let fields = build_ai_fields(
                    &mut fragment.schemas,
                    &mut fragment.diagnostics,
                    &variant.source_items,
                    root,
                    &entry.filter.name,
                    Some(&variant.config_type_name),
                    entry.filter.name == "mcp",
                    &variant.raw_fields,
                )
                .unwrap_or_else(|error| panic!("cannot represent AI filter {}: {error}", entry.filter.name));
                variants.push(SchemaNode::object(fields));
            }
            SchemaNode::one_of(variants)
        } else {
            let fields = build_ai_fields(
                &mut fragment.schemas,
                &mut fragment.diagnostics,
                &entry.filter.source_items,
                root,
                &entry.filter.name,
                entry.filter.config_type_name.as_deref(),
                false,
                &entry.filter.raw_fields,
            )
            .unwrap_or_else(|error| panic!("cannot represent AI filter {}: {error}", entry.filter.name));
            SchemaNode::object(fields)
        };
        fragment.schemas.insert(
            schema_id.clone(),
            ConfigSchema {
                id: schema_id.clone(),
                title: entry.filter.name.clone(),
                description: entry.filter.description.clone(),
                shared: false,
                node: SchemaNode {
                    kind: node.kind,
                    title: None,
                    description: String::new(),
                    default: None,
                    examples: Vec::new(),
                    rules: Vec::new(),
                    sensitive: false,
                },
                producer: Some(fragment.producer.clone()),
            },
        );
        let name = entry.filter.name.clone();
        let required_features = required_features_for(root, &entry.filter);
        let capabilities = filter_capabilities(&entry.filter, registry.is_security_filter(&name));
        let source = source_location(root, &entry.filter);
        fragment.filters.push(FilterDescriptor {
            name: name.clone(),
            protocol: Protocol::Http,
            category: entry.category,
            description: entry.filter.description,
            config_schema: schema_id,
            required_features,
            capabilities,
            examples: entry
                .filter
                .yaml_examples
                .into_iter()
                .map(|yaml| ConfigExample { yaml })
                .collect(),
            source,
            producer: Some(fragment.producer.clone()),
        });
    }
    praxis_config_catalog_generator::render(fragment)
        .unwrap_or_else(|error| panic!("invalid generated AI catalog: {error}"))
}

/// Build AI fields through the Core-owned normalized schema builder.
fn build_ai_fields(
    schemas: &mut BTreeMap<SchemaId, praxis_config_catalog::ConfigSchema>,
    diagnostics: &mut Vec<praxis_config_catalog::CatalogDiagnostic>,
    source: &ModuleItems,
    root: &Path,
    owner: &str,
    config_type_name: Option<&str>,
    allow_json_value: bool,
    fields: &[RawField],
) -> Result<Vec<praxis_config_catalog::ObjectField>, String> {
    if let Some(config) = config_type_name.and_then(|name| source.structs.get(name))
        && let Some(try_from) = config.try_from.as_deref()
    {
        return Err(format!(
            "catalog_error=unsupported_try_from target={owner} type={try_from}"
        ));
    }
    if allow_json_value {
        ensure_json_value_schema(schemas);
    }
    let model = ai_schema_model(source);
    let normalized = ModuleItems::schema_fields(fields);
    let policy = AiSchemaPolicy { root, allow_json_value };
    let mut built = catalog_generator::build_fields(&normalized, &model, schemas, diagnostics, owner, &policy)
        .map_err(|error| error.to_string())?;
    let provider_id = SchemaId::from("ai.type.ProviderConfig");
    for field in &mut built {
        if matches!(&field.schema.kind, praxis_config_catalog::SchemaKind::Reference { schema_id } if schema_id == &provider_id)
            && let Some(schema) = schemas.get(&provider_id)
        {
            field.schema = schema.node.clone();
        }
    }
    Ok(built)
}

fn ai_schema_model(source: &ModuleItems) -> SourceModel {
    let mut model = source.schema_model();
    if model.structs.contains_key("ProviderConfig") && model.structs.contains_key("NemoConfig") {
        let mut provider = model
            .structs
            .get("NemoConfig")
            .cloned()
            .ok_or_else(|| "missing NemoConfig".to_owned())?;
        provider.name = "ProviderConfig".to_owned();
        provider.fields.insert(
            0,
            SourceField {
                name: "type".to_owned(),
                aliases: Vec::new(),
                ty: syn::parse_str("AiProviderDiscriminator").expect("literal type parses"),
                doc: "Provider discriminator.".to_owned(),
                has_default: false,
                default_path: None,
                deserialize_with: None,
                flatten: false,
                requirement: GeneratorRequirementHint::Normal,
                sensitive: false,
            },
        );
        model.structs.insert("ProviderConfig".to_owned(), provider);
    }
    model
}

#[derive(Debug)]
struct AiSchemaPolicy<'a> {
    root: &'a Path,
    allow_json_value: bool,
}

impl SchemaPolicy for AiSchemaPolicy<'_> {
    fn field_schema(&self, field: &SourceField) -> Result<Option<praxis_config_catalog::SchemaNode>, String> {
        let Some(path) = field.deserialize_with.as_deref() else {
            return Ok(None);
        };
        match path.rsplit("::").next().unwrap_or(path) {
            "deserialize_duration" => Ok(Some(praxis_config_catalog::SchemaNode::simple(
                praxis_config_catalog::SchemaKind::String,
            ))),
            "nullable_vec" | "deserialize_metadata_object" => Ok(None),
            _ => Err(format!(
                "catalog_error=unsupported_deserializer field={} deserializer={path}",
                field.name
            )),
        }
    }

    fn named_type_schema(&self, name: &str) -> Result<Option<praxis_config_catalog::SchemaNode>, String> {
        use praxis_config_catalog::SchemaNode;
        match name {
            "AiProviderDiscriminator" => Ok(Some(SchemaNode::literal(serde_json::Value::String("nemo".to_owned())))),
            "OnInvalidBehavior" => Ok(Some(SchemaNode::enum_strings(vec![
                "continue".to_owned(),
                "reject".to_owned(),
                "error".to_owned(),
            ]))),
            "Value" if self.allow_json_value => Ok(Some(SchemaNode::reference("ai.type.json_value".to_owned()))),
            "Value" | "Condition" | "ResponseCondition" => Err(format!(
                "opaque serde value `{name}` has no portable catalog representation"
            )),
            _ => Ok(None),
        }
    }

    fn default_value(&self, path: &str) -> Option<serde_json::Value> {
        evaluate_default(self.root, path)
    }

    fn schema_prefix(&self) -> &str {
        "ai.type"
    }
}

fn ensure_json_value_schema(schemas: &mut BTreeMap<SchemaId, praxis_config_catalog::ConfigSchema>) {
    use praxis_config_catalog::{SchemaKind, SchemaNode};
    let id = SchemaId::from("ai.type.json_value");
    if schemas.contains_key(&id) {
        return;
    }
    let reference = || SchemaNode::reference(id.0.clone());
    let node = SchemaNode::one_of(vec![
        SchemaNode::simple(SchemaKind::Null),
        SchemaNode::simple(SchemaKind::Boolean),
        SchemaNode::simple(SchemaKind::Number),
        SchemaNode::simple(SchemaKind::String),
        SchemaNode::array(reference()),
        SchemaNode::map(reference()),
    ]);
    schemas.insert(
        id.clone(),
        praxis_config_catalog::ConfigSchema {
            id,
            title: "JSON value".to_owned(),
            description: "Any JSON value accepted by the MCP tool schema fields.".to_owned(),
            shared: false,
            node,
            producer: None,
        },
    );
}

/// Convert the small set of AI custom deserializers whose wire shape is known.
fn custom_deserializer_kind(field: &RawField) -> Result<Option<praxis_config_catalog::SchemaKind>, String> {
    let Some(path) = field.deserialize_with.as_deref() else {
        return Ok(None);
    };
    let name = path.rsplit("::").next().unwrap_or(path);
    match name {
        "deserialize_duration" => Ok(Some(praxis_config_catalog::SchemaKind::String)),
        "nullable_vec" | "deserialize_metadata_object" => Ok(None),
        _ => Err(format!(
            "catalog_error=unsupported_deserializer field={} deserializer={path}",
            field.name
        )),
    }
}

/// The recursive source-to-schema implementation lives in the Core generator crate.
/// Parse examples during generation so malformed documentation cannot enter
/// the machine-readable catalog as if it were usable configuration.
fn validate_yaml_examples(entries: &[FilterEntry]) -> Result<(), String> {
    for entry in entries {
        for (index, example) in entry.filter.yaml_examples.iter().enumerate() {
            serde_yaml::from_str::<serde_yaml::Value>(example).map_err(|error| {
                format!(
                    "catalog_error=invalid_example filter={} index={} error={error}",
                    entry.filter.name, index
                )
            })?;
        }
    }
    Ok(())
}

/// Safely evaluate only literal defaults. Runtime-dependent defaults produce a
/// warning diagnostic and remain without a literal value.
fn evaluate_default(root: &Path, function: &str) -> Option<serde_json::Value> {
    for directory in [root.join("apis/src"), root.join("filters/src")] {
        for path in collect_rs_files(&directory) {
            let Ok(source) = fs::read_to_string(path) else { continue };
            let marker = format!("fn {function}");
            let Some(start) = source.find(&marker) else { continue };
            let tail = &source[start..];
            let body = &tail[..tail.find('}').unwrap_or(tail.len())];
            if body.contains("true") {
                return Some(serde_json::Value::Bool(true));
            }
            if body.contains("false") {
                return Some(serde_json::Value::Bool(false));
            }
            if let Some(value) = body.split('"').nth(1) {
                return Some(serde_json::Value::String(value.to_owned()));
            }
            if let Some(number) = body
                .split(|character: char| !character.is_ascii_digit() && character != '-')
                .find(|value| !value.is_empty() && *value != "-")
                && let Ok(value) = number.parse::<i64>()
            {
                return Some(serde_json::Value::from(value));
            }
        }
    }
    None
}

fn ai_feature_profile(root: &Path) -> Result<praxis_config_catalog::FeatureProfile, String> {
    let path = root.join("filters/Cargo.toml");
    let Ok(source) = fs::read_to_string(&path) else {
        return Err(format!("missing {}", path.display()));
    };
    feature_profile_from_manifest(&source)
}

fn feature_profile_from_manifest(source: &str) -> Result<praxis_config_catalog::FeatureProfile, String> {
    let mut profile = praxis_config_catalog::FeatureProfile::default();
    let Ok(document) = toml::from_str::<toml::Table>(source) else {
        return Err("invalid TOML feature manifest".to_owned());
    };
    let Some(features) = document.get("features").and_then(toml::Value::as_table) else {
        return Err("missing [features] table".to_owned());
    };
    if let Some(values) = features.get("default").and_then(toml::Value::as_array) {
        profile
            .default_enabled
            .extend(values.iter().filter_map(toml::Value::as_str).map(str::to_owned));
    }
    for (name, _value) in features {
        if name != "default" {
            profile.available.insert(name.clone());
        }
    }
    Ok(profile)
}

fn required_features_for(root: &Path, filter: &FilterInfo) -> BTreeSet<String> {
    let registrations = fs::read_to_string(root.join("filters/src/register.rs")).unwrap_or_default();
    required_features_for_source(&registrations, &filter.name)
}

fn required_features_for_source(registrations: &str, filter_name: &str) -> BTreeSet<String> {
    let mut features = BTreeSet::new();
    let registration_lines: Vec<&str> = registrations.lines().collect();
    let mut function_feature = None;
    let mut depth = 0usize;
    for line in registration_lines {
        let trimmed = line.trim();
        if let Some(feature) = trimmed
            .strip_prefix("#[cfg(feature = \"")
            .and_then(|line| line.strip_suffix("\")]"))
        {
            function_feature = Some(feature.to_owned());
        }
        if trimmed.starts_with("fn ") {
            depth = 0;
        }
        if trimmed.contains(&format!("\"{filter_name}\""))
            && let Some(feature) = &function_feature
        {
            features.insert(feature.clone());
        }
        depth = depth.saturating_add(line.matches('{').count());
        depth = depth.saturating_sub(line.matches('}').count());
        if function_feature.is_some() && (trimmed == ")" || trimmed == ");") {
            function_feature = None;
        }
        if depth == 0 && trimmed.contains('}') {
            function_feature = None;
        }
    }
    features
}

fn source_location(root: &Path, filter: &FilterInfo) -> Option<praxis_config_catalog::SourceLocation> {
    let path = filter.source_path.as_ref()?;
    let source = fs::read_to_string(path).ok()?;
    let line = source.lines().position(|line| line.contains("HttpFilter for"));
    Some(praxis_config_catalog::SourceLocation {
        path: relative_path(root, path).to_string_lossy().replace('\\', "/"),
        line: line.map(|line| line as u32 + 1),
    })
}

fn filter_capabilities(filter: &FilterInfo, is_security: bool) -> praxis_config_catalog::FilterCapabilities {
    let source = filter
        .source_path
        .as_ref()
        .and_then(|path| fs::read_to_string(path).ok())
        .unwrap_or_default();
    capabilities_from_source(&source, is_security)
}

fn capabilities_from_source(source: &str, is_security: bool) -> praxis_config_catalog::FilterCapabilities {
    praxis_config_catalog::FilterCapabilities {
        security_class: if is_security {
            praxis_config_catalog::SecurityClass::Security
        } else {
            praxis_config_catalog::SecurityClass::Standard
        },
        request_headers: source.contains("fn on_request("),
        request_body: source.contains("fn on_request_body("),
        response_headers: source.contains("fn on_response("),
        response_body: source.contains("fn on_response_body("),
        terminal: source.contains("TerminalResponse") || source.contains("StreamingTerminalResponse"),
    }
}

fn active_catalog_features() -> BTreeSet<String> {
    let feature_names = [
        "apis",
        #[cfg(feature = "catalog-http-callout")]
        "http-callout-filter",
        #[cfg(feature = "catalog-azure-ad")]
        "azure-ad-filter",
        #[cfg(feature = "catalog-gcp-adc")]
        "gcp-adc-filter",
        #[cfg(feature = "catalog-token-rate-limit")]
        "token-rate-limit-filter",
    ];
    feature_names.into_iter().map(str::to_owned).collect()
}

fn validate_registry_parity(
    root: &Path,
    entries: &[FilterEntry],
    registry: &praxis_filter::FilterRegistry,
    active_features: &BTreeSet<String>,
) {
    let discovered: BTreeSet<String> = entries
        .iter()
        .filter(|entry| required_features_for(root, &entry.filter).is_subset(active_features))
        .map(|entry| entry.filter.name.clone())
        .collect();
    let core: BTreeSet<String> = praxis_filter::FilterRegistry::with_builtins()
        .available_filters()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let registered: BTreeSet<String> = registry
        .available_filters()
        .into_iter()
        .filter(|name| !core.contains(*name))
        .map(str::to_owned)
        .collect();
    assert_eq!(
        registered, discovered,
        "default AI registry and catalog discovery differ"
    );
    for entry in entries {
        for feature in required_features_for(root, &entry.filter) {
            assert!(
                active_features.contains(&feature)
                    || ai_feature_profile(root).is_ok_and(|profile| profile.available.contains(&feature)),
                "unknown required feature {feature}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use praxis_config_catalog::{
        CatalogCompatibility, CatalogFormatVersion, CatalogFragment, ConfigSchema, ProducerComponent, ProducerInfo,
        SchemaId, SchemaKind, SchemaNode,
    };

    use super::*;

    #[test]
    fn capabilities_follow_http_filter_callbacks() {
        let capabilities = capabilities_from_source(
            "impl HttpFilter for Example { fn on_request(&self) {} fn on_response_body(&self) {} FilterAction::TerminalResponse }",
            false,
        );
        assert!(capabilities.request_headers);
        assert!(capabilities.response_body);
        assert!(!capabilities.request_body);
        assert!(!capabilities.response_headers);
        assert!(capabilities.terminal);
    }

    #[test]
    fn custom_deserializers_are_explicitly_allowlisted() {
        let field = RawField {
            name: "timeout".to_owned(),
            ty: syn::parse_str("Duration").unwrap(),
            docs: String::new(),
            aliases: Vec::new(),
            has_default: false,
            default_path: None,
            deserialize_with: Some("deserialize_duration".to_owned()),
            flatten: false,
            skip: false,
        };
        assert!(matches!(
            custom_deserializer_kind(&field),
            Ok(Some(praxis_config_catalog::SchemaKind::String))
        ));
        let mut unsupported = field;
        unsupported.deserialize_with = Some("custom::unknown".to_owned());
        let error = custom_deserializer_kind(&unsupported).err().unwrap_or_default();
        assert!(error.contains("catalog_error=unsupported_deserializer"));
    }

    #[test]
    fn literal_defaults_are_evaluated_without_running_code() {
        let root = workspace_root();
        assert_eq!(
            evaluate_default(&root, "default_true"),
            Some(serde_json::Value::Bool(true))
        );
        assert_eq!(evaluate_default(&root, "does_not_exist"), None);
    }

    fn fragment(component: ProducerComponent, package: &str, version: &str) -> CatalogFragment {
        CatalogFragment {
            format_version: CatalogFormatVersion { major: 1, minor: 0 },
            producer: ProducerInfo {
                component,
                package: package.to_owned(),
                version: version.to_owned(),
                source_revision: None,
            },
            compatibility: CatalogCompatibility {
                requires_format_major: 1,
                requires_core: None,
            },
            feature_profile: Default::default(),
            schemas: BTreeMap::new(),
            roots: Vec::new(),
            filters: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn registration_features_associate_with_adjacent_functions() {
        let source = r#"
            #[cfg(feature = "gated-a")]
            fn register_a() { register!("a"); }
            fn register_b() { register!("b"); }
            #[cfg(feature = "gated-c")]
            fn register_c() { register!("c"); }
        "#;
        assert_eq!(
            required_features_for_source(source, "a"),
            ["gated-a".to_owned()].into_iter().collect()
        );
        assert!(required_features_for_source(source, "b").is_empty());
        assert_eq!(
            required_features_for_source(source, "c"),
            ["gated-c".to_owned()].into_iter().collect()
        );
    }

    #[test]
    fn non_string_map_keys_fail_closed() {
        let field = SourceField {
            name: "labels".to_owned(),
            aliases: Vec::new(),
            ty: syn::parse_str("BTreeMap<u32, String>").unwrap(),
            doc: String::new(),
            has_default: false,
            default_path: None,
            deserialize_with: None,
            flatten: false,
            requirement: GeneratorRequirementHint::Normal,
            sensitive: false,
        };
        let mut schemas = BTreeMap::new();
        let mut diagnostics = Vec::new();
        let policy = AiSchemaPolicy {
            root: Path::new("."),
            allow_json_value: false,
        };
        assert!(
            catalog_generator::build_fields(
                &[field],
                &SourceModel::default(),
                &mut schemas,
                &mut diagnostics,
                "test",
                &policy,
            )
            .is_err()
        );
    }

    #[test]
    fn generated_ai_catalog_has_nemo_schema_and_provenance() {
        let root = workspace_root();
        let rendered = render_catalog_impl(&root);
        let fragment: CatalogFragment = serde_json::from_slice(&rendered).expect("catalog JSON");
        fragment.validate().expect("catalog validates");
        let schema = fragment
            .schemas
            .get(&SchemaId::from("ai.filter.http.guardrails.ai_guardrails"))
            .expect("guardrails schema");
        let praxis_config_catalog::SchemaKind::Object { fields, .. } = &schema.node.kind else {
            panic!("guardrails object")
        };
        let names: BTreeSet<_> = fields.iter().map(|field| field.serialized_name.as_str()).collect();
        assert_eq!(names, ["provider", "phase"].into_iter().collect());
        let provider = &fields
            .iter()
            .find(|field| field.serialized_name == "provider")
            .unwrap()
            .schema;
        let praxis_config_catalog::SchemaKind::Object { fields, .. } = &provider.kind else {
            panic!("provider object")
        };
        let provider_names: BTreeSet<_> = fields.iter().map(|field| field.serialized_name.as_str()).collect();
        assert_eq!(
            provider_names,
            ["type", "endpoint", "allow_private_endpoint", "model", "timeout_ms"]
                .into_iter()
                .collect()
        );
        let discriminator = &fields
            .iter()
            .find(|field| field.serialized_name == "type")
            .unwrap()
            .schema
            .kind;
        assert!(matches!(discriminator, praxis_config_catalog::SchemaKind::Literal { value } if value == "nemo"));
        assert!(fragment.filters.iter().all(|filter| filter.producer.is_some()));
        assert!(fragment.schemas.values().all(|schema| schema.producer.is_some()));
        assert!(
            fragment
                .filters
                .iter()
                .all(|filter| filter.source.as_ref().is_some_and(|source| source.line.is_some()))
        );
    }

    #[test]
    fn mcp_factory_union_is_typed_and_provenanced() {
        let root = workspace_root();
        let bytes = render_catalog_impl(&root);
        let fragment: CatalogFragment = serde_json::from_slice(&bytes).expect("catalog JSON");
        let schema = fragment
            .schemas
            .get(&SchemaId::from("ai.filter.http.agentic.mcp"))
            .expect("MCP schema");
        let praxis_config_catalog::SchemaKind::OneOf { variants, .. } = &schema.node.kind else {
            panic!("MCP must expose its two factory shapes as one_of")
        };
        assert_eq!(variants.len(), 2);
        let names: BTreeSet<&str> = variants
            .iter()
            .filter_map(|variant| match &variant.kind {
                praxis_config_catalog::SchemaKind::Object { fields, .. } => {
                    fields.first().map(|field| field.serialized_name.as_str())
                },
                _ => None,
            })
            .collect();
        assert!(names.contains("header_validation"));
        assert!(names.contains("cache_scope"));
        assert_eq!(
            schema.producer.as_ref().map(|p| &p.component),
            Some(&ProducerComponent::Ai)
        );
        assert!(
            fragment
                .schemas
                .get(&SchemaId::from("ai.type.json_value"))
                .is_some_and(|schema| schema.producer.is_some())
        );
    }

    #[test]
    fn feature_profile_fixture_is_strict() {
        let profile = feature_profile_from_manifest("[features]\ndefault=[\"apis\"]\napis=[]\nexperimental=[]\n")
            .expect("valid TOML");
        assert!(profile.available.contains("apis"));
        assert!(profile.default_enabled.contains("apis"));
        assert!(feature_profile_from_manifest("[features\n").is_err());
    }

    #[test]
    fn core_fixture_and_ai_fragment_merge() {
        let root = workspace_root();
        let mut core = fragment(ProducerComponent::Core, "praxis-proxy-core", "0.5.4");
        let schema = SchemaId::from("praxis-proxy-core.schema");
        core.schemas.insert(
            schema.clone(),
            ConfigSchema {
                id: schema.clone(),
                title: "Core config".to_owned(),
                description: String::new(),
                node: SchemaNode::simple(SchemaKind::String),
                shared: false,
                producer: None,
            },
        );
        core.roots.push(praxis_config_catalog::RootConfigDescriptor {
            name: "Config".to_owned(),
            schema,
        });
        let ai: CatalogFragment = serde_json::from_slice(&render_catalog_impl(&root)).expect("AI JSON");
        let merged = praxis_config_catalog_generator::merge(core, vec![ai]).expect("Core and AI merge");
        assert!(!merged.fragment.filters.is_empty());
        assert!(merged.producers.len() >= 2);
        assert!(!merged.fragment.roots.is_empty(), "Core roots must survive AI merge");
        assert!(merged.fragment.filters.windows(2).all(|pair| {
            (&pair[0].protocol, &pair[0].category, &pair[0].name)
                <= (&pair[1].protocol, &pair[1].category, &pair[1].name)
        }));
        assert!(merged.fragment.filters.iter().all(|filter| filter.producer.is_some()));
        assert!(merged.fragment.schemas.values().all(|schema| schema.producer.is_some()));
    }

    #[test]
    fn incompatible_core_requirement_has_stable_error() {
        let mut ai = fragment(ProducerComponent::Ai, "praxis-ai", "0.3.0");
        ai.compatibility.requires_core = Some("^9.0.0".into());
        let error = praxis_config_catalog_generator::merge(
            fragment(ProducerComponent::Core, "praxis-proxy-core", "0.5.4"),
            vec![ai],
        )
        .expect_err("incompatible Core requirement must fail closed");
        assert!(matches!(
            error,
            praxis_config_catalog::merge::MergeError::CoreRequirement(_)
        ));
    }
}
