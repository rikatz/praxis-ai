// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Praxis Contributors

//! AI filter discovery and Markdown rendering.
//!
//! Rust parsing and serde metadata extraction live in the Core-owned catalog
//! generator. This module contains only AI layout and documentation policy.

use std::{
    collections::{BTreeMap, HashSet},
    fmt::Write as _,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use clap::Parser;
use praxis_config_catalog_generator::{
    RustEnum, RustEnumVariantShape, RustField, RustSourceModel, RustStruct,
    collect_rust_files as collect_shared_rust_files, parse_rust_files, parse_rust_source,
};
use quote::ToTokens as _;

pub(crate) type ModuleItems = RustSourceModel;
pub(crate) type RawField = RustField;
pub(crate) type ConfigStruct = RustStruct;
pub(crate) type EnumVariantShape = RustEnumVariantShape;

#[derive(Parser)]
pub(crate) struct GenerateArgs;
#[derive(Parser)]
pub(crate) struct LintArgs;

pub(crate) struct FilterEntry {
    crate_kind: String,
    pub(crate) category: String,
    pub(crate) filter: FilterInfo,
}
#[derive(Clone)]
pub(crate) struct FilterInfo {
    pub(crate) name: String,
    pub(crate) description: String,
    extra_descriptions: Vec<String>,
    config_notes: Vec<String>,
    fields: Vec<FieldInfo>,
    pub(crate) yaml_examples: Vec<String>,
    pub(crate) raw_fields: Vec<RawField>,
    pub(crate) config_type_name: Option<String>,
    pub(crate) variants: Vec<FilterVariant>,
    pub(crate) source_items: ModuleItems,
    pub(crate) source_path: Option<PathBuf>,
}
#[derive(Clone)]
pub(crate) struct FilterVariant {
    pub(crate) config_type_name: String,
    pub(crate) raw_fields: Vec<RawField>,
    pub(crate) source_items: ModuleItems,
}
#[derive(Clone)]
struct FieldInfo {
    name: String,
    type_str: String,
    doc: String,
    required: RequiredKind,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RequiredKind {
    Yes,
    No,
}
impl RequiredKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Yes => "yes",
            Self::No => "no",
        }
    }
}
struct FilterAnchor {
    file: PathBuf,
    name: String,
    config_type_name: Option<String>,
}

impl FilterInfo {
    fn merge(&mut self, other: Self) {
        if self.description.is_empty() {
            self.description = other.description;
        } else if !other.description.is_empty()
            && other.description != self.description
            && !self.extra_descriptions.contains(&other.description)
        {
            self.extra_descriptions.push(other.description);
        }
        append_unique(&mut self.extra_descriptions, other.extra_descriptions);
        append_unique(&mut self.config_notes, other.config_notes);
        append_unique_fields(&mut self.fields, other.fields);
        append_unique(&mut self.yaml_examples, other.yaml_examples);
        for variant in other.variants {
            if !self
                .variants
                .iter()
                .any(|item| item.config_type_name == variant.config_type_name)
            {
                self.variants.push(variant);
            }
        }
    }
}

pub(crate) fn generate(_args: GenerateArgs) {
    let root = workspace_root();
    let shared = parse_shared_config_items(&root);
    let entries = discover_all_filters(&root, &shared);
    let dir = root.join("docs/filters");
    create_dir_all_or_exit(&dir);
    for entry in &entries {
        let path = dir.join(format!("{}.md", entry.filter.name));
        write_or_exit(&path, &render_filter_doc(entry));
        print_relative(&root, &path, "wrote");
    }
    let index = dir.join("reference.md");
    write_or_exit(&index, &render_reference_index(&entries));
    print_relative(&root, &index, "wrote");
    remove_stale_docs(&root, &dir, &entries);
    println!("{} filter doc(s) generated", entries.len() + 1);
}
pub(crate) fn lint(_args: LintArgs) {
    let root = workspace_root();
    let shared = parse_shared_config_items(&root);
    let entries = discover_all_filters(&root, &shared);
    let dir = root.join("docs/filters");
    let stale = collect_stale_doc_paths(&root, &dir, &entries);
    if stale.is_empty() {
        println!("all filter doc files are up to date");
    } else {
        eprintln!("filter doc files are stale:");
        for path in stale {
            eprintln!("  {}", path.display());
        }
        std::process::exit(1);
    }
}

const LOCAL_SHARED_CONFIG_FILES: &[&str] = &[
    "apis/src/web_search/config.rs",
    "apis/src/callout_policy.rs",
    "apis/src/store/postgres.rs",
    "apis/src/store/pool.rs",
];
pub(crate) fn parse_shared_config_items(root: &Path) -> ModuleItems {
    let mut paths = Vec::new();
    let core = root.join("../praxis");
    if core.is_dir() {
        paths.extend(
            collect_shared_rust_files(&core.join("filter/src/builtins/http/payload_processing")).unwrap_or_default(),
        );
    } else {
        paths.extend(
            resolve_praxis_source_dirs()
                .into_iter()
                .flat_map(|dir| collect_shared_rust_files(&dir).unwrap_or_default()),
        );
    }
    paths.extend(
        LOCAL_SHARED_CONFIG_FILES
            .iter()
            .map(|rel| root.join(rel))
            .filter(|path| path.is_file()),
    );
    parse_rust_files(paths).unwrap_or_default()
}

pub(crate) fn collect_rs_files(root: &Path) -> Vec<PathBuf> {
    collect_shared_rust_files(root).unwrap_or_default()
}
fn resolve_praxis_source_dirs() -> Vec<PathBuf> {
    let Ok(output) = Command::new("cargo")
        .args(["metadata", "--format-version", "1"])
        .output()
    else {
        return Vec::new();
    };
    let Ok(meta) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    meta.get("packages")
        .and_then(|value| value.as_array())
        .into_iter()
        .flatten()
        .find_map(|package| {
            (package.get("name").and_then(|value| value.as_str()) == Some("praxis-proxy-filter"))
                .then(|| {
                    let manifest = package.get("manifest_path")?.as_str()?;
                    Some(
                        Path::new(manifest)
                            .parent()?
                            .join("src/builtins/http/payload_processing"),
                    )
                })
                .flatten()
        })
        .into_iter()
        .collect()
}

pub(crate) fn discover_all_filters(root: &Path, shared: &ModuleItems) -> Vec<FilterEntry> {
    let mut entries = Vec::new();
    discover_crate_filters(&root.join("apis/src"), "apis", shared, &mut entries);
    discover_crate_filters(&root.join("filters/src"), "filters", shared, &mut entries);
    entries.sort_by(|a, b| a.filter.name.cmp(&b.filter.name));
    entries
}
fn discover_crate_filters(src: &Path, kind: &str, shared: &ModuleItems, out: &mut Vec<FilterEntry>) {
    let Ok(items) = fs::read_dir(src) else { return };
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for item in items.flatten() {
        let path = item.path();
        if path.is_dir() {
            if !matches!(dir_file_name(&path).as_str(), "store" | "classifier") {
                dirs.push(path);
            }
        } else if path.extension().is_some_and(|e| e == "rs")
            && path
                .file_name()
                .is_none_or(|n| !matches!(n.to_str(), Some("lib.rs" | "mod.rs" | "tests.rs")))
            && parse_anchor_file(&path).is_some()
        {
            files.push(path);
        }
    }
    dirs.sort();
    files.sort();
    for dir in dirs {
        let category = dir_file_name(&dir);
        for filter in extract_filters(&dir, shared) {
            out.push(FilterEntry {
                crate_kind: kind.to_owned(),
                category: category.clone(),
                filter,
            });
        }
    }
    for path in files {
        if let Some(anchor) = parse_anchor_file(&path) {
            let model = parse_model(shared, [path.clone()]);
            let mut filter = build_filter(&model, &anchor.name, anchor.config_type_name.as_deref());
            filter.source_path = Some(path.clone());
            out.push(FilterEntry {
                crate_kind: kind.to_owned(),
                category: infer_standalone_category(&anchor.name),
                filter,
            });
        }
    }
}
fn infer_standalone_category(name: &str) -> String {
    if name.contains("token") {
        "token_usage".to_owned()
    } else {
        "general".to_owned()
    }
}

fn extract_filters(dir: &Path, shared: &ModuleItems) -> Vec<FilterInfo> {
    let anchors = discover_filter_anchors(dir);
    let mut filters = Vec::new();
    for anchor in &anchors {
        let files = scope_files_for_anchor(anchor, dir, &anchors);
        let model = parse_model(shared, files);
        let mut filter = build_filter(&model, &anchor.name, anchor.config_type_name.as_deref());
        filter.source_path = Some(anchor.file.clone());
        filters.push(filter);
    }
    let mut grouped = BTreeMap::<String, FilterInfo>::new();
    for filter in filters {
        grouped
            .entry(filter.name.clone())
            .and_modify(|current| current.merge(filter.clone()))
            .or_insert(filter);
    }
    grouped.into_values().collect()
}
fn parse_model(base: &ModuleItems, paths: impl IntoIterator<Item = PathBuf>) -> ModuleItems {
    let mut model = base.clone();
    for path in paths {
        if let Ok(source) = fs::read_to_string(&path) {
            let _ = parse_rust_source(&source, &mut model);
            model.files.push(path);
        }
    }
    model
}
fn discover_filter_anchors(dir: &Path) -> Vec<FilterAnchor> {
    let mut result = collect_shared_rust_files(dir)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|path| parse_anchor_file(&path))
        .collect::<Vec<_>>();
    result.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.file.cmp(&b.file)));
    result
}
fn scope_files_for_anchor(anchor: &FilterAnchor, category: &Path, all: &[FilterAnchor]) -> Vec<PathBuf> {
    let Some(parent) = anchor.file.parent() else {
        return vec![anchor.file.clone()];
    };
    let sibling = all
        .iter()
        .any(|item| item.file != anchor.file && item.file.parent() == Some(parent));
    if sibling {
        let mut files = vec![anchor.file.clone()];
        files.extend(direct_support_files(parent, all));
        files.sort();
        files.dedup();
        return files;
    }
    let excluded = all
        .iter()
        .filter(|item| item.file != anchor.file && item.file.starts_with(parent) && item.file.parent() != Some(parent))
        .filter_map(|item| item.file.parent())
        .collect::<HashSet<_>>();
    let mut files = Vec::new();
    collect_scope(parent, &excluded, &mut files);
    files.extend(direct_support_files(parent, all));
    let mut ancestor = parent.parent();
    while let Some(dir) = ancestor {
        if !dir.starts_with(category) || dir == category {
            break;
        }
        files.extend(direct_support_files(dir, all));
        ancestor = dir.parent();
    }
    files.extend(direct_support_files(category, all));
    files.sort();
    files.dedup();
    files
}
fn direct_support_files(dir: &Path, all: &[FilterAnchor]) -> Vec<PathBuf> {
    let Ok(items) = fs::read_dir(dir) else {
        return Vec::new();
    };
    items
        .flatten()
        .map(|item| item.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "rs"))
        .filter(|path| {
            path.file_name()
                .is_none_or(|name| name != "mod.rs" && name != "tests.rs")
        })
        .filter(|path| !all.iter().any(|anchor| anchor.file == *path))
        .collect()
}
fn collect_scope(dir: &Path, excluded: &HashSet<&Path>, out: &mut Vec<PathBuf>) {
    let Ok(items) = fs::read_dir(dir) else { return };
    for item in items.flatten() {
        let path = item.path();
        if path.is_dir() {
            if !excluded.contains(path.as_path()) {
                collect_scope(&path, excluded, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") && path.file_name().is_none_or(|n| n != "tests.rs") {
            out.push(path);
        }
    }
}

pub(crate) fn validate_factory_variants(root: &Path, allowed: &[(&str, &str, &str)]) -> Result<(), String> {
    let mut types = BTreeMap::new();
    for source in [root.join("apis/src"), root.join("filters/src")] {
        let Ok(items) = fs::read_dir(source) else { continue };
        for item in items.flatten().filter(|item| item.path().is_dir()) {
            for anchor in discover_filter_anchors(&item.path()) {
                record_factory_variant(&mut types, &anchor, allowed)?;
            }
        }
    }
    Ok(())
}
fn record_factory_variant(
    types: &mut BTreeMap<String, String>,
    anchor: &FilterAnchor,
    allowed: &[(&str, &str, &str)],
) -> Result<(), String> {
    let Some(config) = anchor.config_type_name.as_deref() else {
        return Ok(());
    };
    if let Some(previous) = types.insert(anchor.name.clone(), config.to_owned())
        && previous != config
    {
        let union = allowed.iter().any(|(filter, first, second)| {
            *filter == anchor.name
                && ((*first == previous && *second == config) || (*first == config && *second == previous))
        });
        if union {
            types.remove(&anchor.name);
            return Ok(());
        }
        return Err(format!(
            "catalog_error=incompatible_config_type filter={} first={} second={}",
            anchor.name, previous, config
        ));
    }
    Ok(())
}

fn parse_anchor_file(path: &Path) -> Option<FilterAnchor> {
    let source = fs::read_to_string(path).ok()?;
    let file = syn::parse_file(&source).ok()?;
    let mut name = None;
    let mut config = None;
    let mut factory = false;
    for item in &file.items {
        let syn::Item::Impl(imp) = item else { continue };
        if let Some(value) = extract_filter_name(imp) {
            name = Some(value)
        }
        if has_from_config_method(imp) {
            factory = true;
            config = config.or_else(|| extract_config_type_name(imp));
        }
    }
    factory.then_some(FilterAnchor {
        file: path.to_owned(),
        name: name?,
        config_type_name: config,
    })
}
fn extract_filter_name(imp: &syn::ItemImpl) -> Option<String> {
    imp.items.iter().find_map(|item| {
        let syn::ImplItem::Fn(method) = item else { return None };
        (method.sig.ident == "name")
            .then(|| {
                method.block.stmts.iter().find_map(|stmt| match stmt {
                    syn::Stmt::Expr(expr, _) => extract_str_literal(expr),
                    _ => None,
                })
            })
            .flatten()
    })
}
fn has_from_config_method(imp: &syn::ItemImpl) -> bool {
    imp.items
        .iter()
        .any(|item| matches!(item,syn::ImplItem::Fn(method)if method.sig.ident=="from_config"))
}
fn extract_config_type_name(imp: &syn::ItemImpl) -> Option<String> {
    let methods = imp
        .items
        .iter()
        .filter_map(|item| match item {
            syn::ImplItem::Fn(method) => Some(method),
            _ => None,
        })
        .collect::<Vec<_>>();
    let from = methods.iter().find(|m| m.sig.ident == "from_config")?;
    scan_config_type(from).or_else(|| {
        methods
            .iter()
            .filter(|m| m.sig.ident != "from_config")
            .find_map(|m| scan_config_type(m))
    })
}
fn scan_config_type(method: &syn::ImplItemFn) -> Option<String> {
    method.block.stmts.iter().find_map(|stmt| {
        let syn::Stmt::Local(local) = stmt else { return None };
        let init = local.init.as_ref()?;
        if !init.expr.to_token_stream().to_string().contains("parse_filter_config") {
            return None;
        };
        let syn::Pat::Type(pattern) = &local.pat else {
            return None;
        };
        Some(pattern.ty.to_token_stream().to_string())
    })
}
fn extract_str_literal(expr: &syn::Expr) -> Option<String> {
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(value),
            ..
        }) => Some(value.value()),
        _ => None,
    }
}

fn build_filter(items: &ModuleItems, name: &str, config_type: Option<&str>) -> FilterInfo {
    let docs = items
        .structs
        .values()
        .filter(|item| item.public || item.name.ends_with("Filter"))
        .map(|item| item.docs.clone())
        .chain(items.module_docs.iter().cloned())
        .find(|doc| !doc.is_empty())
        .unwrap_or_default();
    let config = config_type.and_then(|name| items.structs.get(name));
    let raw = config.map_or_else(Vec::new, |item| item.fields.clone());
    let variants = config_type
        .zip(config)
        .map(|(name, item)| {
            vec![FilterVariant {
                config_type_name: name.to_owned(),
                raw_fields: item.fields.clone(),
                source_items: items.clone(),
            }]
        })
        .unwrap_or_default();
    FilterInfo {
        name: name.to_owned(),
        description: first_paragraph(&docs),
        extra_descriptions: Vec::new(),
        config_notes: filter_notes(&docs),
        fields: config.map_or_else(Vec::new, |item| build_fields(item, items)),
        yaml_examples: collect_yaml_examples(items),
        raw_fields: raw,
        config_type_name: config_type.map(str::to_owned),
        variants,
        source_items: items.clone(),
        source_path: None,
    }
}
fn collect_yaml_examples(items: &ModuleItems) -> Vec<String> {
    let mut result = Vec::new();
    for doc in items
        .module_docs
        .iter()
        .chain(items.structs.values().map(|item| &item.docs))
    {
        append_unique(&mut result, extract_yaml_examples(doc));
    }
    result
}
fn build_fields(config: &ConfigStruct, items: &ModuleItems) -> Vec<FieldInfo> {
    let mut result = Vec::new();
    append_fields("", &config.fields, items, &mut Vec::new(), &mut result);
    result
}
fn append_fields(
    prefix: &str,
    fields: &[RawField],
    items: &ModuleItems,
    stack: &mut Vec<String>,
    out: &mut Vec<FieldInfo>,
) {
    for field in fields.iter().filter(|field| !field.skip) {
        let path = field_path(prefix, &field.name);
        if !field.flatten {
            out.push(FieldInfo {
                name: path.clone(),
                type_str: render_field_type(field, &items.enums),
                doc: field.docs.clone(),
                required: required_kind(field),
            });
        } else if let Some(tag) = flattened_tag_field(prefix, field, items) {
            out.push(tag)
        }
        let nested = if field.flatten {
            prefix.to_owned()
        } else if is_sequence_type(&field.ty) {
            format!("{path}[]")
        } else {
            path
        };
        append_nested(&nested, &field.ty, items, stack, out);
    }
}
fn append_nested(prefix: &str, ty: &syn::Type, items: &ModuleItems, stack: &mut Vec<String>, out: &mut Vec<FieldInfo>) {
    let Some(name) = nested_type_name(ty) else { return };
    if stack.contains(&name) {
        return;
    }
    if let Some(config) = items.structs.get(&name) {
        stack.push(name.clone());
        append_fields(prefix, &config.fields, items, stack, out);
        stack.pop();
    } else if let Some(info) = items.enums.get(&name) {
        if let Some(fields) = info.variants.iter().find_map(|variant| match &variant.shape {
            RustEnumVariantShape::Named(fields) => Some(fields.as_slice()),
            _ => None,
        }) {
            stack.push(name);
            append_fields(prefix, fields, items, stack, out);
            stack.pop();
        }
    }
}
fn flattened_tag_field(prefix: &str, field: &RawField, items: &ModuleItems) -> Option<FieldInfo> {
    let name = nested_type_name(&field.ty)?;
    let info = items.enums.get(&name)?;
    let tag = info.tag.as_ref()?;
    Some(FieldInfo {
        name: field_path(prefix, tag),
        type_str: render_enum_type(info, &items.enums),
        doc: field.docs.clone(),
        required: required_kind(field),
    })
}
fn required_kind(field: &RawField) -> RequiredKind {
    if !field.has_default && !is_option_type(&field.ty) {
        RequiredKind::Yes
    } else {
        RequiredKind::No
    }
}
fn field_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else if name.is_empty() {
        prefix.to_owned()
    } else {
        format!("{prefix}.{name}")
    }
}
fn is_option_type(ty: &syn::Type) -> bool {
    matches!(ty,syn::Type::Path(path)if path.path.segments.last().is_some_and(|segment|segment.ident=="Option"))
}
fn render_field_type(field: &RawField, enums: &BTreeMap<String, RustEnum>) -> String {
    if field.deserialize_with.as_deref() == Some("deserialize_redirect_status") {
        "301 \\| 302 \\| 307 \\| 308".to_owned()
    } else {
        render_type(&field.ty, enums)
    }
}
fn render_type(ty: &syn::Type, enums: &BTreeMap<String, RustEnum>) -> String {
    if let syn::Type::Path(path) = ty {
        render_type_path(path, enums)
    } else {
        quote::quote!(#ty).to_string()
    }
}
fn render_type_path(path: &syn::TypePath, enums: &BTreeMap<String, RustEnum>) -> String {
    let Some(segment) = path.path.segments.last() else {
        return "any".to_owned();
    };
    let name = segment.ident.to_string();
    let args = type_args(&segment.arguments);
    match name.as_str() {
        "Vec" => args
            .first()
            .map_or_else(|| "any[]".to_owned(), |ty| format!("{}[]", render_type(ty, enums))),
        "Option" | "Zeroizing" | "Arc" | "Box" => args
            .last()
            .map_or_else(|| "any".to_owned(), |ty| render_type(ty, enums)),
        "BTreeMap" | "HashMap" => "object".to_owned(),
        "String" | "SecretString" => "string".to_owned(),
        "Value" => "any".to_owned(),
        "bool" => "bool".to_owned(),
        "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" | "isize" => "integer".to_owned(),
        "f32" | "f64" => "number".to_owned(),
        _ => enums.get(&name).map_or(name, |info| render_enum_type(info, enums)),
    }
}
fn render_enum_type(info: &RustEnum, enums: &BTreeMap<String, RustEnum>) -> String {
    if info.untagged {
        render_union(info.variants.iter().map(|variant| {
            match &variant.shape {
                RustEnumVariantShape::Unit => format!("`{}`", variant.name),
                RustEnumVariantShape::Unnamed(types) => types
                    .first()
                    .map_or_else(|| "object".to_owned(), |ty| render_type(ty, enums)),
                RustEnumVariantShape::Named(_) => "object".to_owned(),
            }
        }))
    } else {
        render_union(info.variants.iter().map(|variant| format!("`{}`", variant.name)))
    }
}
fn render_union<I: IntoIterator<Item = String>>(items: I) -> String {
    let mut seen = HashSet::new();
    items
        .into_iter()
        .filter(|item| seen.insert(item.clone()))
        .collect::<Vec<_>>()
        .join(" \\| ")
}
fn type_args(arguments: &syn::PathArguments) -> Vec<&syn::Type> {
    let syn::PathArguments::AngleBracketed(arguments) = arguments else {
        return Vec::new();
    };
    arguments
        .args
        .iter()
        .filter_map(|argument| match argument {
            syn::GenericArgument::Type(ty) => Some(ty),
            _ => None,
        })
        .collect()
}
fn is_sequence_type(ty: &syn::Type) -> bool {
    let syn::Type::Path(path) = ty else { return false };
    let Some(segment) = path.path.segments.last() else {
        return false;
    };
    match segment.ident.to_string().as_str() {
        "Vec" => true,
        "Option" | "Arc" | "Box" => type_args(&segment.arguments)
            .first()
            .is_some_and(|ty| is_sequence_type(ty)),
        _ => false,
    }
}
fn nested_type_name(ty: &syn::Type) -> Option<String> {
    let syn::Type::Path(path) = ty else { return None };
    let segment = path.path.segments.last()?;
    let name = segment.ident.to_string();
    match name.as_str() {
        "Vec" | "Option" | "Arc" | "Box" => type_args(&segment.arguments)
            .first()
            .and_then(|ty| nested_type_name(ty)),
        "BTreeMap" | "HashMap" => type_args(&segment.arguments).get(1).and_then(|ty| nested_type_name(ty)),
        "String" | "SecretString" | "Value" | "str" | "bool" | "u8" | "u16" | "u32" | "u64" | "usize" | "i8"
        | "i16" | "i32" | "i64" | "isize" | "f32" | "f64" => None,
        _ => Some(name),
    }
}

fn extract_yaml_examples(doc: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut lines = doc.lines().peekable();
    while let Some(line) = lines.next() {
        if !is_yaml_heading(line) {
            continue;
        }
        while let Some(next) = lines.peek().copied() {
            if is_yaml_fence_start(next.trim()) {
                lines.next();
                let mut yaml = Vec::new();
                for line in lines.by_ref() {
                    if line.trim() == "```" {
                        break;
                    }
                    yaml.push(line)
                }
                if !yaml.is_empty() {
                    result.push(yaml.join("\n"))
                }
                break;
            }
            if is_markdown_heading(next.trim()) {
                break;
            }
            let _ = lines.next();
        }
    }
    result
}
fn is_yaml_heading(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('#')
        && trimmed
            .trim_start_matches('#')
            .split(|ch: char| !ch.is_ascii_alphanumeric())
            .any(|word| word.eq_ignore_ascii_case("yaml"))
}
fn is_markdown_heading(line: &str) -> bool {
    line.trim_start().starts_with('#')
}
fn is_yaml_fence_start(line: &str) -> bool {
    line.starts_with("```yaml") || line.starts_with("```yml")
}
fn first_paragraph(doc: &str) -> String {
    doc.lines()
        .take_while(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join(" ")
        .trim()
        .to_owned()
}
fn filter_notes(doc: &str) -> Vec<String> {
    doc.split("\n\n").skip(1).filter_map(normalize_doc_prose).collect()
}
fn normalize_doc_prose(doc: &str) -> Option<String> {
    let mut fence = false;
    let lines = doc
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("```") {
                fence = !fence;
                return None;
            }
            if fence
                || trimmed.is_empty()
                || trimmed.starts_with('#')
                || (trimmed.starts_with('[') && trimmed.contains("]:"))
            {
                None
            } else {
                Some(trimmed)
            }
        })
        .collect::<Vec<_>>();
    (!lines.is_empty()).then(|| lines.join(" "))
}
fn normalize_field_doc(doc: &str) -> String {
    normalize_doc_prose(doc).unwrap_or_default().replace('|', "\\|")
}

fn render_filter_doc(entry: &FilterEntry) -> String {
    let mut out = String::new();
    writeln!(
        out,
        "<!-- Generated by: cargo xtask generate-filter-docs -->\n<!-- Do not edit manually -->\n\n# `{}`\n",
        entry.filter.name
    )
    .unwrap();
    if !entry.filter.description.is_empty() {
        writeln!(out, "{}", entry.filter.description).unwrap()
    }
    for description in &entry.filter.extra_descriptions {
        writeln!(out, "\n{description}").unwrap()
    }
    if !entry.filter.config_notes.is_empty() {
        writeln!(out, "\n## Configuration Notes").unwrap();
        for note in &entry.filter.config_notes {
            writeln!(out, "\n{note}").unwrap()
        }
    }
    if !entry.filter.fields.is_empty() {
        writeln!(
            out,
            "\n## Configuration\n\n| Field | Type | Required | Description |\n|-------|------|---------|-------------|"
        )
        .unwrap();
        for field in &entry.filter.fields {
            writeln!(
                out,
                "| `{}` | {} | {} | {} |",
                field.name,
                field.type_str,
                field.required.as_str(),
                normalize_field_doc(&field.doc)
            )
            .unwrap()
        }
    }
    if !entry.filter.yaml_examples.is_empty() {
        writeln!(
            out,
            "\n## {}",
            if entry.filter.yaml_examples.len() == 1 {
                "Example"
            } else {
                "Examples"
            }
        )
        .unwrap();
        for (index, yaml) in entry.filter.yaml_examples.iter().enumerate() {
            if entry.filter.yaml_examples.len() > 1 {
                writeln!(out, "\n### Example {}", index + 1).unwrap()
            }
            writeln!(out, "\n```yaml\n{yaml}\n```").unwrap()
        }
    }
    out
}
fn render_reference_index(entries: &[FilterEntry]) -> String {
    let mut out = String::from(
        "<!-- Generated by: cargo xtask generate-filter-docs -->\n<!-- Do not edit manually -->\n\n# Filter Reference\n\nAI filters provided by Praxis AI. For base proxy\nfilters (router, load balancer, headers, CORS, etc.),\nsee the [Praxis core filter reference][core-ref].\n\n[core-ref]: https://github.com/praxis-proxy/praxis/blob/main/docs/filters/reference.md\n",
    );
    let grouped = group_by_crate_category(entries);
    let mut current = "";
    for ((kind, category), filters) in grouped {
        if kind != current {
            current = kind;
            writeln!(out, "\n## {}", crate_heading(kind)).unwrap()
        }
        writeln!(
            out,
            "\n### {}\n\n| Filter | Description |\n|--------|-------------|",
            format_title(category)
        )
        .unwrap();
        for filter in filters {
            writeln!(
                out,
                "| [`{}`]({}.md) | {} |",
                filter.filter.name, filter.filter.name, filter.filter.description
            )
            .unwrap()
        }
    }
    out
}
fn group_by_crate_category(entries: &[FilterEntry]) -> BTreeMap<(&str, &str), Vec<&FilterEntry>> {
    let mut result = BTreeMap::new();
    for entry in entries {
        result
            .entry((entry.crate_kind.as_str(), entry.category.as_str()))
            .or_insert_with(Vec::new)
            .push(entry)
    }
    result
}
fn crate_heading(kind: &str) -> &str {
    match kind {
        "apis" => "Provider APIs (praxis-ai-apis)",
        "filters" => "Cross-Provider Filters (praxis-ai-filters)",
        other => other,
    }
}
fn format_title(value: &str) -> String {
    value
        .split('_')
        .map(|word| match word {
            "ai" => "AI",
            "tcp" => "TCP",
            "ip" => "IP",
            "http" => "HTTP",
            "openai" => "OpenAI",
            "mcp" => "MCP",
            "a2a" => "A2A",
            "aws" => "AWS",
            "gcp" => "GCP",
            other => other,
        })
        .map(capitalize)
        .collect::<Vec<_>>()
        .join(" ")
}
fn capitalize(value: &str) -> String {
    let mut chars = value.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().collect::<String>() + chars.as_str()
    })
}
fn build_expected_paths(dir: &Path, entries: &[FilterEntry]) -> HashSet<PathBuf> {
    let mut result = entries
        .iter()
        .map(|entry| dir.join(format!("{}.md", entry.filter.name)))
        .collect::<HashSet<_>>();
    result.insert(dir.join("reference.md"));
    result
}
fn is_generated_md(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "md")
        && fs::read_to_string(path)
            .is_ok_and(|text| text.starts_with("<!-- Generated by: cargo xtask generate-filter-docs -->"))
}
fn collect_stale_doc_paths(root: &Path, dir: &Path, entries: &[FilterEntry]) -> Vec<PathBuf> {
    let mut stale = Vec::new();
    for entry in entries {
        let path = dir.join(format!("{}.md", entry.filter.name));
        if !file_matches(&path, &render_filter_doc(entry)) {
            stale.push(relative_path(root, &path))
        }
    }
    let index = dir.join("reference.md");
    if !file_matches(&index, &render_reference_index(entries)) {
        stale.push(relative_path(root, &index))
    }
    let expected = build_expected_paths(dir, entries);
    if let Ok(files) = fs::read_dir(dir) {
        for file in files.flatten() {
            let path = file.path();
            if is_generated_md(&path) && !expected.contains(&path) {
                stale.push(relative_path(root, &path))
            }
        }
    }
    stale
}
fn remove_stale_docs(root: &Path, dir: &Path, entries: &[FilterEntry]) {
    let expected = build_expected_paths(dir, entries);
    if let Ok(files) = fs::read_dir(dir) {
        for file in files.flatten() {
            let path = file.path();
            if is_generated_md(&path) && !expected.contains(&path) && fs::remove_file(&path).is_ok() {
                print_relative(root, &path, "removed stale")
            }
        }
    }
}
fn append_unique(target: &mut Vec<String>, values: Vec<String>) {
    for value in values {
        if !target.contains(&value) {
            target.push(value)
        }
    }
}
fn append_unique_fields(target: &mut Vec<FieldInfo>, values: Vec<FieldInfo>) {
    for value in values {
        if !target.iter().any(|item| item.name == value.name) {
            target.push(value)
        }
    }
}
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask workspace parent")
        .to_path_buf()
}
fn dir_file_name(path: &Path) -> String {
    path.file_name().expect("directory name").to_string_lossy().into_owned()
}
fn create_dir_all_or_exit(path: &Path) {
    if let Err(error) = fs::create_dir_all(path) {
        eprintln!("failed to create {}: {error}", path.display());
        std::process::exit(1)
    }
}
fn write_or_exit(path: &Path, content: &str) {
    if let Err(error) = fs::write(path, content) {
        eprintln!("failed to write {}: {error}", path.display());
        std::process::exit(1)
    }
}
fn file_matches(path: &Path, expected: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|actual| actual == expected)
}
fn print_relative(root: &Path, path: &Path, action: &str) {
    println!("  {action} {}", path.strip_prefix(root).unwrap_or(path).display())
}
pub(crate) fn relative_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_owned()
}
