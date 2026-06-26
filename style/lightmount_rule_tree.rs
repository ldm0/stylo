//! Lightmount-facing stylesheet rule tree hooks.

use std::{
    borrow::Cow,
    sync::{atomic::AtomicBool, Once},
};

use cssparser::{
    CowRcStr, DeclarationParser, Parser, ParserInput, ParserState, RuleBodyItemParser,
    RuleBodyParser, SourceLocation, ToCss as CssParserToCss,
};
use servo_arc::Arc;
use style_traits::{
    CssStringWriter, CssWriter, ParseError, ParsingMode, StyleParseErrorKind, ToCss,
};

use crate::{
    context::QuirksMode,
    custom_properties::AttrTaint,
    font_face::{DescriptorId, FontFaceRule},
    media_queries::MediaList,
    parser::{NestingContext, Parse, ParserContext},
    properties::{
        parse_one_declaration_into, parse_property_declaration_list, Importance,
        PropertyDeclarationBlock, PropertyId, SourcePropertyDeclaration,
    },
    selector_parser::{SelectorImpl, SelectorParser},
    shared_lock::{SharedRwLock, ToCssWithGuard},
    stylesheets::{
        font_feature_values_rule::{FFVDeclaration, PairValues, SingleValue, VectorValues},
        import_rule::{ImportLayer, ImportRule, ImportSheet, ImportSupportsCondition},
        keyframes_rule::{Keyframe, KeyframeSelectors, KeyframesRule},
        parse_nested_rule_block, AllowImportRules, CssRule, CssRuleType, CssRuleTypes, CssRules,
        MarginRule, MarginRuleType, Namespaces, Origin, PageRule, PageSelectors, RulesMutateError,
        StyleRule, Stylesheet, StylesheetContents, StylesheetLoader, UrlExtraData,
    },
    values::CssUrl,
    Atom,
};
use selectors::parser::SelectorList;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssStylesheetRuleText {
    pub rule_type: CssRuleType,
    pub css_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssStylesheetRuleView {
    pub rule_type: CssRuleType,
    pub css_text: String,
    pub prelude_text: Option<String>,
    pub selector_text: Option<String>,
    pub declaration_text: Option<String>,
    pub child_rules: Vec<CssStylesheetRuleView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssCounterStyleRuleView {
    pub css_text: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssPropertyRuleView {
    pub css_text: String,
    pub name: String,
    pub syntax: String,
    pub inherits: bool,
    pub initial_value: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssFontFaceRuleView {
    pub css_text: String,
    pub style_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssFontFaceDescriptorEntryView {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssImportRuleView {
    pub css_text: String,
    pub href: String,
    pub condition_text: String,
    pub condition_prefix: String,
    pub media_text: String,
    pub layer_name: Option<String>,
    pub supports_text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssNamespaceRuleView {
    pub css_text: String,
    pub prefix: String,
    pub namespace_uri: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssConditionRuleView {
    pub rule_type: CssRuleType,
    pub css_text: String,
    pub condition_text: String,
    pub container_name: Option<String>,
    pub container_query: Option<String>,
    pub scope_start: Option<String>,
    pub scope_end: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssLayerRuleView {
    pub rule_type: CssRuleType,
    pub css_text: String,
    pub name: Option<String>,
    pub names: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssPageRuleView {
    pub css_text: String,
    pub selector_text: String,
    pub style_text: String,
    pub child_rules: Vec<CssMarginRuleView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssMarginRuleView {
    pub css_text: String,
    pub name: String,
    pub style_text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssPageDescriptorEntryView {
    pub name: String,
    pub value: String,
}

pub fn font_face_descriptor_names() -> &'static [&'static str] {
    DescriptorId::names()
}

pub fn parse_font_face_cssom_descriptor_entry(
    descriptor: &str,
    value: &str,
) -> Option<CssFontFaceDescriptorEntryView> {
    let descriptor_id = DescriptorId::from_ident(descriptor).ok()?;
    if value.trim().is_empty() {
        return None;
    }
    let value = with_font_face_descriptor_context(|context| {
        let mut rule = FontFaceRule::empty(SourceLocation { line: 0, column: 0 });
        let mut input = ParserInput::new(value);
        let mut input = Parser::new(&mut input);
        rule.set_cssom_descriptor(descriptor_id, context, &mut input, false)
            .map_err(|_| CssRuleInsertError::Syntax)?;
        let mut value = CssStringWriter::new();
        rule.descriptors
            .get(descriptor_id, &mut value)
            .map_err(|_| CssRuleInsertError::Syntax)?;
        Ok(value.trim().to_owned())
    })
    .ok()?;
    Some(CssFontFaceDescriptorEntryView {
        name: descriptor_id.name().to_owned(),
        value,
    })
}

pub fn page_descriptor_names() -> &'static [&'static str] {
    CSSOM_PAGE_DESCRIPTOR_NAMES
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssKeyframesRuleView {
    pub css_text: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssFontFeatureValueEntryView {
    pub name: String,
    pub values: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssFontFeatureValuesRuleView {
    pub css_text: String,
    pub font_family: String,
    pub annotation: Vec<CssFontFeatureValueEntryView>,
    pub ornaments: Vec<CssFontFeatureValueEntryView>,
    pub stylistic: Vec<CssFontFeatureValueEntryView>,
    pub styleset: Vec<CssFontFeatureValueEntryView>,
    pub character_variant: Vec<CssFontFeatureValueEntryView>,
    pub swash: Vec<CssFontFeatureValueEntryView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssStylesheetMutationResult {
    pub css_text: String,
    pub rules: Vec<CssStylesheetRuleView>,
    pub first_declaration_text: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CssNestedRuleMutationResult {
    pub stylesheet_css_text: String,
    pub parent_rule: CssStylesheetRuleView,
    pub rules: Vec<CssStylesheetRuleView>,
}

#[derive(Clone)]
pub struct CssStylesheetRuleTree {
    contents: Arc<StylesheetContents>,
    shared_lock: SharedRwLock,
    allow_import_rules: AllowImportRules,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CssRuleInsertError {
    Syntax,
    IndexSize,
    HierarchyRequest,
    InvalidState,
}

pub fn parse_stylesheet_rule_texts(css_text: &str) -> Vec<CssStylesheetRuleText> {
    parse_stylesheet_rule_texts_with_import_policy(css_text, AllowImportRules::Yes)
}

pub fn parse_constructed_stylesheet_rule_texts(css_text: &str) -> Vec<CssStylesheetRuleText> {
    parse_stylesheet_rule_texts_with_import_policy(css_text, AllowImportRules::No)
}

pub fn parse_stylesheet_rule_views(css_text: &str) -> Vec<CssStylesheetRuleView> {
    parse_stylesheet_rule_views_with_import_policy(css_text, AllowImportRules::Yes)
}

pub fn parse_constructed_stylesheet_rule_views(css_text: &str) -> Vec<CssStylesheetRuleView> {
    parse_stylesheet_rule_views_with_import_policy(css_text, AllowImportRules::No)
}

pub fn parse_counter_style_rule_view(css_text: &str) -> Option<CssCounterStyleRuleView> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [CssRule::CounterStyle(rule)] = rules.0.as_slice() else {
        return None;
    };
    let rule = rule.read_with(&guard);
    Some(CssCounterStyleRuleView {
        css_text: rule.to_css_string(&guard),
        name: rule.name().to_css_string(),
    })
}

pub fn parse_property_rule_view(css_text: &str) -> Option<CssPropertyRuleView> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [CssRule::Property(rule)] = rules.0.as_slice() else {
        return None;
    };
    let syntax = rule
        .descriptors
        .syntax
        .as_ref()
        .map(|syntax| {
            syntax
                .specified_string()
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| syntax.to_css_string())
        })
        .filter(|syntax| !syntax.is_empty())?;
    Some(CssPropertyRuleView {
        css_text: rule.to_css_string(&guard),
        name: rule.name.to_css_string(),
        syntax,
        inherits: rule.descriptors.inherits(),
        initial_value: rule
            .descriptors
            .initial_value
            .as_ref()
            .map(|value| value.css_text().trim().to_owned()),
    })
}

pub fn parse_font_face_rule_view(css_text: &str) -> Option<CssFontFaceRuleView> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [CssRule::FontFace(rule)] = rules.0.as_slice() else {
        return None;
    };
    let rule = rule.read_with(&guard);
    Some(CssFontFaceRuleView {
        css_text: rule.to_css_string(&guard),
        style_text: rule.style_css_text(),
    })
}

pub fn parse_font_face_cssom_descriptor_block(style_text: &str) -> Option<String> {
    parse_font_face_cssom_descriptor_rule(style_text)
        .ok()
        .map(|rule| rule.style_css_text())
}

pub fn parse_import_rule_view(css_text: &str) -> Option<CssImportRuleView> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::Yes);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [CssRule::Import(rule)] = rules.0.as_slice() else {
        return None;
    };
    Some(import_rule_view(&rule.read_with(&guard), &guard)?)
}

pub fn parse_namespace_rule_view(css_text: &str) -> Option<CssNamespaceRuleView> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [CssRule::Namespace(rule)] = rules.0.as_slice() else {
        return None;
    };
    Some(namespace_rule_view(rule.as_ref(), &guard))
}

pub fn parse_condition_rule_view(css_text: &str) -> Option<CssConditionRuleView> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [rule] = rules.0.as_slice() else {
        return None;
    };
    condition_rule_view(rule, &guard)
}

pub fn parse_layer_rule_view(css_text: &str) -> Option<CssLayerRuleView> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [rule] = rules.0.as_slice() else {
        return None;
    };
    layer_rule_view(rule, &guard)
}

pub fn parse_page_rule_view(css_text: &str) -> Option<CssPageRuleView> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [CssRule::Page(rule)] = rules.0.as_slice() else {
        return None;
    };
    Some(page_rule_view(&rule.read_with(&guard), &guard))
}

pub fn parse_page_margin_rule_view(css_text: &str) -> Option<CssMarginRuleView> {
    let page_view = parse_page_rule_view(&format!("@page {{ {css_text} }}"))?;
    let [view] = page_view.child_rules.as_slice() else {
        return None;
    };
    Some(view.clone())
}

pub fn parse_page_margin_descriptor_block(
    margin_name: &str,
    descriptor_text: &str,
) -> Option<String> {
    let rule_type = MarginRuleType::match_name(margin_name)?;
    let block =
        parse_page_margin_declaration_block_for_rule_type(rule_type, descriptor_text).ok()?;
    Some(declaration_block_css_text(&block))
}

pub fn parse_page_descriptor_entries(
    name: &str,
    value: &str,
) -> Option<Vec<CssPageDescriptorEntryView>> {
    let name = canonical_page_descriptor_name(name)?;
    if value.trim().is_empty() {
        return None;
    }
    let block = parse_page_descriptor_declaration(name, value)?;
    let entries = page_descriptor_entries_from_block(&block);
    (!entries.is_empty() && page_descriptor_entries_match_name(name, &entries)).then_some(entries)
}

pub fn parse_keyframes_rule_view(css_text: &str) -> Option<CssKeyframesRuleView> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [CssRule::Keyframes(rule)] = rules.0.as_slice() else {
        return None;
    };
    let rule = rule.read_with(&guard);
    Some(CssKeyframesRuleView {
        css_text: rule.to_css_string(&guard),
        name: rule.name.as_atom().to_string(),
    })
}

pub fn parse_font_feature_values_rule_view(css_text: &str) -> Option<CssFontFeatureValuesRuleView> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [CssRule::FontFeatureValues(rule)] = rules.0.as_slice() else {
        return None;
    };
    let rule = rule.as_ref();
    Some(CssFontFeatureValuesRuleView {
        css_text: rule.to_css_string(&guard),
        font_family: rule.family_names.to_css_string(),
        annotation: single_font_feature_entries(&rule.annotation),
        ornaments: single_font_feature_entries(&rule.ornaments),
        stylistic: single_font_feature_entries(&rule.stylistic),
        styleset: vector_font_feature_entries(&rule.styleset),
        character_variant: pair_font_feature_entries(&rule.character_variant),
        swash: single_font_feature_entries(&rule.swash),
    })
}

pub fn set_font_feature_values_rule_entry(
    css_text: &str,
    group: &str,
    name: &str,
    values: &[u32],
) -> Option<String> {
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No);
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let [CssRule::FontFeatureValues(rule)] = rules.0.as_slice() else {
        return None;
    };
    let mut rule = rule.as_ref().clone();
    let name = Atom::from(name);
    match group {
        "annotation" => update_font_feature_values_entry(
            &mut rule.annotation,
            name,
            single_font_feature_value(values)?,
        ),
        "ornaments" => update_font_feature_values_entry(
            &mut rule.ornaments,
            name,
            single_font_feature_value(values)?,
        ),
        "stylistic" => update_font_feature_values_entry(
            &mut rule.stylistic,
            name,
            single_font_feature_value(values)?,
        ),
        "swash" => update_font_feature_values_entry(
            &mut rule.swash,
            name,
            single_font_feature_value(values)?,
        ),
        "character-variant" => update_font_feature_values_entry(
            &mut rule.character_variant,
            name,
            pair_font_feature_value(values)?,
        ),
        "styleset" => update_font_feature_values_entry(
            &mut rule.styleset,
            name,
            vector_font_feature_value(values)?,
        ),
        _ => return None,
    }
    Some(rule.to_css_string(&guard))
}

fn update_font_feature_values_entry<T>(entries: &mut Vec<FFVDeclaration<T>>, name: Atom, value: T) {
    if let Some(index) = entries.iter().position(|entry| entry.name == name) {
        entries[index].value = value;
    } else {
        entries.push(FFVDeclaration { name, value });
    }
}

fn single_font_feature_value(values: &[u32]) -> Option<SingleValue> {
    let [value] = values else {
        return None;
    };
    Some(SingleValue(*value))
}

fn pair_font_feature_value(values: &[u32]) -> Option<PairValues> {
    match values {
        [first] => Some(PairValues(*first, None)),
        [first, second] => Some(PairValues(*first, Some(*second))),
        _ => None,
    }
}

fn vector_font_feature_value(values: &[u32]) -> Option<VectorValues> {
    if values.is_empty() {
        return None;
    }
    Some(VectorValues(values.to_vec()))
}

fn single_font_feature_entries(
    entries: &[FFVDeclaration<SingleValue>],
) -> Vec<CssFontFeatureValueEntryView> {
    entries
        .iter()
        .map(|entry| CssFontFeatureValueEntryView {
            name: entry.name.to_string(),
            values: vec![entry.value.0],
        })
        .collect()
}

fn pair_font_feature_entries(
    entries: &[FFVDeclaration<PairValues>],
) -> Vec<CssFontFeatureValueEntryView> {
    entries
        .iter()
        .map(|entry| {
            let mut values = vec![entry.value.0];
            if let Some(second) = entry.value.1 {
                values.push(second);
            }
            CssFontFeatureValueEntryView {
                name: entry.name.to_string(),
                values,
            }
        })
        .collect()
}

fn vector_font_feature_entries(
    entries: &[FFVDeclaration<VectorValues>],
) -> Vec<CssFontFeatureValueEntryView> {
    entries
        .iter()
        .map(|entry| CssFontFeatureValueEntryView {
            name: entry.name.to_string(),
            values: entry.value.0.clone(),
        })
        .collect()
}

pub fn parse_stylesheet_rule_tree(css_text: &str) -> CssStylesheetRuleTree {
    parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::Yes)
}

pub fn parse_constructed_stylesheet_rule_tree(css_text: &str) -> CssStylesheetRuleTree {
    parse_stylesheet_rule_tree_with_import_policy(css_text, AllowImportRules::No)
}

pub fn stylesheet_rule_tree_css_text(rule_tree: &CssStylesheetRuleTree) -> String {
    stylesheet_mutation_result(&rule_tree.contents, &rule_tree.shared_lock).css_text
}

pub fn stylesheet_rule_tree_rule_views(
    rule_tree: &CssStylesheetRuleTree,
) -> Vec<CssStylesheetRuleView> {
    stylesheet_mutation_result(&rule_tree.contents, &rule_tree.shared_lock).rules
}

pub fn stylesheet_rule_tree_page_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssPageRuleView> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Page(rule) => {
            let guard = rule_tree.shared_lock.read();
            Some(page_rule_view(rule.read_with(&guard), &guard))
        },
        _ => None,
    }
}

pub fn stylesheet_rule_tree_margin_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssMarginRuleView> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Margin(rule) => {
            let guard = rule_tree.shared_lock.read();
            Some(margin_rule_view(&rule, &guard))
        },
        _ => None,
    }
}

pub fn stylesheet_rule_tree_counter_style_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssCounterStyleRuleView> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::CounterStyle(rule) => {
            let guard = rule_tree.shared_lock.read();
            let rule = rule.read_with(&guard);
            Some(CssCounterStyleRuleView {
                css_text: rule.to_css_string(&guard),
                name: rule.name().to_css_string(),
            })
        },
        _ => None,
    }
}

pub fn stylesheet_rule_tree_font_face_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssFontFaceRuleView> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::FontFace(rule) => {
            let guard = rule_tree.shared_lock.read();
            let rule = rule.read_with(&guard);
            Some(CssFontFaceRuleView {
                css_text: rule.to_css_string(&guard),
                style_text: rule.style_css_text(),
            })
        },
        _ => None,
    }
}

pub fn stylesheet_rule_tree_keyframes_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssKeyframesRuleView> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Keyframes(rule) => {
            let guard = rule_tree.shared_lock.read();
            let rule = rule.read_with(&guard);
            Some(CssKeyframesRuleView {
                css_text: rule.to_css_string(&guard),
                name: rule.name.as_atom().to_string(),
            })
        },
        _ => None,
    }
}

pub fn stylesheet_rule_tree_import_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssImportRuleView> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Import(rule) => {
            let guard = rule_tree.shared_lock.read();
            import_rule_view(&rule.read_with(&guard), &guard)
        },
        _ => None,
    }
}

pub fn stylesheet_rule_tree_namespace_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssNamespaceRuleView> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Namespace(rule) => {
            let guard = rule_tree.shared_lock.read();
            Some(namespace_rule_view(rule.as_ref(), &guard))
        },
        _ => None,
    }
}

pub fn stylesheet_rule_tree_condition_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssConditionRuleView> {
    let rule = rule_at_path(rule_tree, rule_path)?;
    let guard = rule_tree.shared_lock.read();
    condition_rule_view(&rule, &guard)
}

pub fn stylesheet_rule_tree_layer_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssLayerRuleView> {
    let rule = rule_at_path(rule_tree, rule_path)?;
    let guard = rule_tree.shared_lock.read();
    layer_rule_view(&rule, &guard)
}

pub fn stylesheet_rule_tree_property_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssPropertyRuleView> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Property(rule) => {
            let syntax = rule
                .descriptors
                .syntax
                .as_ref()
                .map(|syntax| {
                    syntax
                        .specified_string()
                        .map(ToOwned::to_owned)
                        .unwrap_or_else(|| syntax.to_css_string())
                })
                .filter(|syntax| !syntax.is_empty())?;
            let guard = rule_tree.shared_lock.read();
            Some(CssPropertyRuleView {
                css_text: rule.to_css_string(&guard),
                name: rule.name.to_css_string(),
                syntax,
                inherits: rule.descriptors.inherits(),
                initial_value: rule
                    .descriptors
                    .initial_value
                    .as_ref()
                    .map(|value| value.css_text().trim().to_owned()),
            })
        },
        _ => None,
    }
}

pub fn stylesheet_rule_tree_font_feature_values_rule_view(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<CssFontFeatureValuesRuleView> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::FontFeatureValues(rule) => {
            let guard = rule_tree.shared_lock.read();
            let rule = rule.as_ref();
            Some(CssFontFeatureValuesRuleView {
                css_text: rule.to_css_string(&guard),
                font_family: rule.family_names.to_css_string(),
                annotation: single_font_feature_entries(&rule.annotation),
                ornaments: single_font_feature_entries(&rule.ornaments),
                stylistic: single_font_feature_entries(&rule.stylistic),
                styleset: vector_font_feature_entries(&rule.styleset),
                character_variant: pair_font_feature_entries(&rule.character_variant),
                swash: single_font_feature_entries(&rule.swash),
            })
        },
        _ => None,
    }
}

pub fn insert_rule_into_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_text: &str,
    index: usize,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match rule_tree.allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let parsed_rule = rules.parse_rule_for_insert(
        &rule_tree.shared_lock,
        rule_text,
        &rule_tree.contents,
        index,
        CssRuleTypes::default(),
        None,
        stylesheet_loader,
        rule_tree.allow_import_rules,
    );
    let rule = match parsed_rule {
        Ok(rule) => rule,
        Err(error) => {
            let error = CssRuleInsertError::from(error);
            if error == CssRuleInsertError::HierarchyRequest
                && rule_text_is_namespace_rule(rule_text)
                && rules
                    .0
                    .iter()
                    .any(|rule| !matches!(rule, CssRule::Import(..) | CssRule::Namespace(..)))
            {
                return Err(CssRuleInsertError::InvalidState);
            }
            return Err(error);
        },
    };
    drop(guard);
    let refresh_namespaces = matches!(&rule, CssRule::Namespace(..));
    {
        let mut guard = rule_tree.shared_lock.write();
        rule_tree
            .contents
            .rules
            .write_with(&mut guard)
            .0
            .insert(index, rule);
    }
    let result = stylesheet_mutation_result(&rule_tree.contents, &rule_tree.shared_lock);
    if refresh_namespaces {
        refresh_stylesheet_rule_tree_from_css_text(rule_tree, &result.css_text);
        return Ok(stylesheet_mutation_result(
            &rule_tree.contents,
            &rule_tree.shared_lock,
        ));
    }
    Ok(result)
}

pub fn delete_rule_from_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    index: usize,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let refresh_namespaces = {
        let guard = rule_tree.shared_lock.read();
        rule_tree
            .contents
            .rules
            .read_with(&guard)
            .0
            .get(index)
            .is_some_and(|rule| matches!(rule, CssRule::Namespace(..)))
    };
    {
        let mut guard = rule_tree.shared_lock.write();
        rule_tree
            .contents
            .rules
            .write_with(&mut guard)
            .remove_rule(index)
            .map_err(CssRuleInsertError::from)?;
    }
    let result = stylesheet_mutation_result(&rule_tree.contents, &rule_tree.shared_lock);
    if refresh_namespaces {
        refresh_stylesheet_rule_tree_from_css_text(rule_tree, &result.css_text);
        return Ok(stylesheet_mutation_result(
            &rule_tree.contents,
            &rule_tree.shared_lock,
        ));
    }
    Ok(result)
}

pub fn replace_rule_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_text: &str,
    index: usize,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match rule_tree.allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let Some(existing_rule) = rules.0.get(index) else {
        return Err(CssRuleInsertError::IndexSize);
    };
    let old_is_namespace = matches!(existing_rule, CssRule::Namespace(..));
    let parsed_rule = rules.parse_rule_for_insert(
        &rule_tree.shared_lock,
        rule_text,
        &rule_tree.contents,
        index,
        CssRuleTypes::default(),
        None,
        stylesheet_loader,
        rule_tree.allow_import_rules,
    );
    let rule = match parsed_rule {
        Ok(rule) => rule,
        Err(error) => {
            let error = CssRuleInsertError::from(error);
            if error == CssRuleInsertError::HierarchyRequest
                && rule_text_is_namespace_rule(rule_text)
                && rules
                    .0
                    .iter()
                    .any(|rule| !matches!(rule, CssRule::Import(..) | CssRule::Namespace(..)))
            {
                return Err(CssRuleInsertError::InvalidState);
            }
            return Err(error);
        },
    };
    drop(guard);
    let refresh_namespaces = old_is_namespace || matches!(&rule, CssRule::Namespace(..));
    {
        let mut guard = rule_tree.shared_lock.write();
        rule_tree.contents.rules.write_with(&mut guard).0[index] = rule;
    }
    let result = stylesheet_mutation_result(&rule_tree.contents, &rule_tree.shared_lock);
    if refresh_namespaces {
        refresh_stylesheet_rule_tree_from_css_text(rule_tree, &result.css_text);
        return Ok(stylesheet_mutation_result(
            &rule_tree.contents,
            &rule_tree.shared_lock,
        ));
    }
    Ok(result)
}

pub fn serialize_stylesheet(css_text: &str) -> String {
    parse_stylesheet_rule_texts(css_text)
        .into_iter()
        .map(|rule| rule.css_text)
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn parse_stylesheet_rule_for_insert(
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
    constructed: bool,
) -> Result<CssStylesheetRuleText, CssRuleInsertError> {
    let parsed =
        parse_stylesheet_rule_for_insert_rule(existing_rule_texts, rule_text, index, constructed)?;
    let guard = parsed.shared_lock.read();
    Ok(CssStylesheetRuleText {
        rule_type: parsed.rule.rule_type(),
        css_text: parsed.rule.to_css_string(&guard),
    })
}

pub fn parse_stylesheet_rule_view_for_insert(
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
    constructed: bool,
) -> Result<CssStylesheetRuleView, CssRuleInsertError> {
    let parsed =
        parse_stylesheet_rule_for_insert_rule(existing_rule_texts, rule_text, index, constructed)?;
    let guard = parsed.shared_lock.read();
    Ok(stylesheet_rule_view(&parsed.rule, &guard))
}

pub fn insert_stylesheet_rule(
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
    constructed: bool,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed =
        parse_stylesheet_rule_for_insert_rule(existing_rule_texts, rule_text, index, constructed)?;
    {
        let mut guard = parsed.shared_lock.write();
        parsed
            .contents
            .rules
            .write_with(&mut guard)
            .0
            .insert(index, parsed.rule);
    }
    Ok(stylesheet_mutation_result(
        &parsed.contents,
        &parsed.shared_lock,
    ))
}

pub fn delete_stylesheet_rule(
    existing_rule_texts: &[String],
    index: usize,
    constructed: bool,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed = parse_stylesheet_for_mutation(existing_rule_texts, constructed)?;
    {
        let mut guard = parsed.shared_lock.write();
        parsed
            .contents
            .rules
            .write_with(&mut guard)
            .remove_rule(index)
            .map_err(CssRuleInsertError::from)?;
    }
    Ok(stylesheet_mutation_result(
        &parsed.contents,
        &parsed.shared_lock,
    ))
}

pub fn insert_nested_rule(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed = parse_nested_rules_for_mutation(
        parent_stylesheet_rule_texts,
        existing_rule_texts,
        containing_rule_type_bits,
        parse_relative_rule_type,
    )?;
    if index > parsed.rules.0.len() {
        return Err(CssRuleInsertError::IndexSize);
    }
    let rule = parsed.parse_rule_for_insert(rule_text, index)?;
    let mut rules = parsed.rules;
    rules.0.insert(index, rule);
    Ok(css_rules_mutation_result(&rules, &parsed.shared_lock))
}

pub fn delete_nested_rule(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    index: usize,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed = parse_nested_rules_for_mutation(
        parent_stylesheet_rule_texts,
        existing_rule_texts,
        containing_rule_type_bits,
        parse_relative_rule_type,
    )?;
    let mut rules = parsed.rules;
    rules.remove_rule(index).map_err(CssRuleInsertError::from)?;
    Ok(css_rules_mutation_result(&rules, &parsed.shared_lock))
}

pub fn parse_nested_rule_block_views(
    parent_stylesheet_rule_texts: &[String],
    block_text: &str,
    rule_type: CssRuleType,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
    wants_first_declaration_block: bool,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed = parse_nested_rules_for_mutation(
        parent_stylesheet_rule_texts,
        &[],
        containing_rule_type_bits,
        parse_relative_rule_type,
    )?;
    let nested = parse_nested_rule_block(
        block_text,
        rule_type,
        &parsed.contents,
        &parsed.shared_lock,
        parsed.containing_rule_types,
        parsed.parse_relative_rule_type,
        wants_first_declaration_block,
    );
    let first_declaration_text = wants_first_declaration_block
        .then(|| declaration_block_css_text(&nested.first_declaration_block));
    Ok(css_rules_mutation_result_with_first_declaration_text(
        &CssRules(nested.rules),
        &parsed.shared_lock,
        first_declaration_text,
    ))
}

pub fn insert_nested_rule_into_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    rule_text: &str,
    index: usize,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let child_rules = mutable_child_rules_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let containing_rule_types = CssRuleTypes::from_bits(containing_rule_type_bits);
    let rule = {
        let guard = rule_tree.shared_lock.read();
        let rules = child_rules.read_with(&guard);
        rules
            .parse_rule_for_insert(
                &rule_tree.shared_lock,
                rule_text,
                &rule_tree.contents,
                index,
                containing_rule_types,
                parse_relative_rule_type,
                None,
                AllowImportRules::No,
            )
            .map_err(CssRuleInsertError::from)?
    };
    {
        let mut guard = rule_tree.shared_lock.write();
        child_rules.write_with(&mut guard).0.insert(index, rule);
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn delete_nested_rule_from_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let child_rules = mutable_child_rules_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        child_rules
            .write_with(&mut guard)
            .remove_rule(index)
            .map_err(CssRuleInsertError::from)?;
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn replace_nested_rule_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    rule_text: &str,
    index: usize,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let child_rules = mutable_child_rules_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let containing_rule_types = CssRuleTypes::from_bits(containing_rule_type_bits);
    let rule = {
        let guard = rule_tree.shared_lock.read();
        let rules = child_rules.read_with(&guard);
        if index >= rules.0.len() {
            return Err(CssRuleInsertError::IndexSize);
        }
        rules
            .parse_rule_for_insert(
                &rule_tree.shared_lock,
                rule_text,
                &rule_tree.contents,
                index,
                containing_rule_types,
                parse_relative_rule_type,
                None,
                AllowImportRules::No,
            )
            .map_err(CssRuleInsertError::from)?
    };
    {
        let mut guard = rule_tree.shared_lock.write();
        child_rules.write_with(&mut guard).0[index] = rule;
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn insert_keyframe_rule(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed =
        parse_keyframe_rules_for_mutation(parent_stylesheet_rule_texts, existing_rule_texts)?;
    if index > parsed.keyframes.len() {
        return Err(CssRuleInsertError::IndexSize);
    }
    let rule = Keyframe::parse(rule_text, &parsed.contents, &parsed.shared_lock)
        .map_err(|_| CssRuleInsertError::Syntax)?;
    let mut keyframes = parsed.keyframes;
    keyframes.insert(index, rule);
    Ok(keyframe_rules_mutation_result(
        &keyframes,
        &parsed.shared_lock,
    ))
}

pub fn delete_keyframe_rule(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    index: usize,
) -> Result<CssStylesheetMutationResult, CssRuleInsertError> {
    let parsed =
        parse_keyframe_rules_for_mutation(parent_stylesheet_rule_texts, existing_rule_texts)?;
    if index >= parsed.keyframes.len() {
        return Err(CssRuleInsertError::IndexSize);
    }
    let mut keyframes = parsed.keyframes;
    keyframes.remove(index);
    Ok(keyframe_rules_mutation_result(
        &keyframes,
        &parsed.shared_lock,
    ))
}

pub fn insert_keyframe_rule_into_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    rule_text: &str,
    index: usize,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let keyframes_rule = mutable_keyframes_rule_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let rule = Keyframe::parse(rule_text, &rule_tree.contents, &rule_tree.shared_lock)
        .map_err(|_| CssRuleInsertError::Syntax)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        let keyframes_rule = keyframes_rule.write_with(&mut guard);
        if index > keyframes_rule.keyframes.len() {
            return Err(CssRuleInsertError::IndexSize);
        }
        keyframes_rule.keyframes.insert(index, rule);
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn delete_keyframe_rule_from_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let keyframes_rule = mutable_keyframes_rule_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        let keyframes_rule = keyframes_rule.write_with(&mut guard);
        if index >= keyframes_rule.keyframes.len() {
            return Err(CssRuleInsertError::IndexSize);
        }
        keyframes_rule.keyframes.remove(index);
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn replace_keyframe_rule_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    rule_text: &str,
    index: usize,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let keyframes_rule = mutable_keyframes_rule_for_rule_path(rule_tree, parent_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    {
        let guard = rule_tree.shared_lock.read();
        if index >= keyframes_rule.read_with(&guard).keyframes.len() {
            return Err(CssRuleInsertError::IndexSize);
        }
    }
    let rule = Keyframe::parse(rule_text, &rule_tree.contents, &rule_tree.shared_lock)
        .map_err(|_| CssRuleInsertError::Syntax)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        keyframes_rule.write_with(&mut guard).keyframes[index] = rule;
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn set_media_rule_media_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    media_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let media_queries = mutable_media_rule_media_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let parsed = parse_media_list_for_rule(media_text)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        *media_queries.write_with(&mut guard) = parsed;
    }
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

pub fn set_style_rule_declarations_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    declaration_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let block = mutable_style_rule_declaration_block_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let parsed = parse_declaration_block_for_rule(declaration_text, CssRuleType::Style)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        *block.write_with(&mut guard) = parsed;
    }
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

pub fn set_nested_declarations_rule_declarations_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    declaration_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let block = mutable_nested_declarations_rule_block_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let parsed = parse_declaration_block_for_rule(declaration_text, CssRuleType::Style)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        *block.write_with(&mut guard) = parsed;
    }
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

pub fn set_keyframe_rule_declarations_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
    declaration_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let block =
        mutable_keyframe_rule_declaration_block_for_rule_path(rule_tree, parent_path, index)
            .ok_or(CssRuleInsertError::IndexSize)?;
    let parsed = parse_declaration_block_for_rule(declaration_text, CssRuleType::Keyframe)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        *block.write_with(&mut guard) = parsed;
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn set_font_face_rule_descriptors_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    descriptor_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let font_face_rule = mutable_font_face_rule_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let parsed = parse_font_face_cssom_descriptor_rule(descriptor_text)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        let rule = font_face_rule.write_with(&mut guard);
        rule.descriptors = parsed.descriptors;
        rule.descriptor_importance = parsed.descriptor_importance;
    }
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

pub fn set_page_rule_descriptors_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    descriptor_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let block = mutable_page_rule_declaration_block_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let parsed = parse_declaration_block_for_rule(descriptor_text, CssRuleType::Page)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        *block.write_with(&mut guard) = parsed;
    }
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

pub fn set_page_margin_rule_descriptors_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    descriptor_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let (rule_type, block) = mutable_page_margin_rule_context_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let parsed = parse_page_margin_declaration_block_for_rule_type(rule_type, descriptor_text)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        *block.write_with(&mut guard) = parsed;
    }
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

pub fn set_style_rule_selector_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    selector_text: &str,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let style_rule = mutable_style_rule_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let selectors = parse_style_rule_selectors(
        selector_text,
        &rule_tree.contents,
        containing_rule_type_bits,
        parse_relative_rule_type,
    )?;
    {
        let mut guard = rule_tree.shared_lock.write();
        style_rule.write_with(&mut guard).selectors = selectors;
    }
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

pub fn set_font_face_rule_descriptor_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    rule_path: &[usize],
    descriptor: &str,
    value: &str,
    important: bool,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let font_face_rule = mutable_font_face_rule_for_rule_path(rule_tree, rule_path)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let descriptor_id =
        DescriptorId::from_ident(descriptor).map_err(|_| CssRuleInsertError::Syntax)?;
    if value.trim().is_empty() {
        {
            let mut guard = rule_tree.shared_lock.write();
            font_face_rule
                .write_with(&mut guard)
                .remove_cssom_descriptor(descriptor_id);
        }
        return nested_rule_tree_mutation_result(rule_tree, rule_path);
    }
    with_font_face_descriptor_context(|context| {
        let mut input = ParserInput::new(value);
        let mut input = Parser::new(&mut input);
        let mut guard = rule_tree.shared_lock.write();
        font_face_rule
            .write_with(&mut guard)
            .set_cssom_descriptor(descriptor_id, context, &mut input, important)
            .map_err(|_| CssRuleInsertError::Syntax)?;
        Ok(())
    })?;
    nested_rule_tree_mutation_result(rule_tree, rule_path)
}

fn parse_style_rule_selectors(
    selector_text: &str,
    parent_stylesheet_contents: &StylesheetContents,
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<SelectorList<SelectorImpl>, CssRuleInsertError> {
    let mut context = ParserContext::new(
        parent_stylesheet_contents.origin,
        &parent_stylesheet_contents.url_data,
        None,
        ParsingMode::DEFAULT,
        parent_stylesheet_contents.quirks_mode,
        Cow::Borrowed(&parent_stylesheet_contents.namespaces),
        None,
        None,
        /* attr_taint */ Default::default(),
    );
    context.nesting_context = NestingContext::new(
        CssRuleTypes::from_bits(containing_rule_type_bits),
        parse_relative_rule_type,
    );
    let selector_parser = SelectorParser {
        stylesheet_origin: context.stylesheet_origin,
        namespaces: &context.namespaces,
        url_data: context.url_data,
        for_supports_rule: false,
    };
    let mut input = ParserInput::new(selector_text);
    let mut input = Parser::new(&mut input);
    input
        .parse_entirely(|input| {
            SelectorList::parse(
                &selector_parser,
                input,
                context.nesting_context.parse_relative,
            )
        })
        .map_err(|_| CssRuleInsertError::Syntax)
}

pub fn set_keyframe_rule_selector_in_stylesheet_rule_tree(
    rule_tree: &mut CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
    selector_text: &str,
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let keyframe = mutable_keyframe_for_rule_path(rule_tree, parent_path, index)
        .ok_or(CssRuleInsertError::IndexSize)?;
    let selector = parse_keyframe_selectors(selector_text).ok_or(CssRuleInsertError::Syntax)?;
    {
        let mut guard = rule_tree.shared_lock.write();
        keyframe.write_with(&mut guard).selector = selector;
    }
    nested_rule_tree_mutation_result(rule_tree, parent_path)
}

pub fn normalize_keyframe_selector_text(selector_text: &str) -> Option<String> {
    parse_keyframe_selectors(selector_text).map(|selectors| selectors.to_css_string())
}

pub fn normalize_page_selector_text(selector_text: &str) -> Option<String> {
    let selector_text = selector_text.trim();
    if selector_text.is_empty() {
        return Some(String::new());
    }
    let Some(url_data) = about_blank_url_data() else {
        return None;
    };
    let context = ParserContext::new(
        Origin::Author,
        &url_data,
        Some(CssRuleType::Page),
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Cow::Owned(Namespaces::default()),
        None,
        None,
        AttrTaint::default(),
    );
    let mut input = ParserInput::new(selector_text);
    let mut input = Parser::new(&mut input);
    let selectors = input
        .parse_entirely(|input| PageSelectors::parse(&context, input))
        .ok()?;
    (!selectors.is_empty()).then(|| selectors.to_css_string())
}

pub fn keyframe_selector_texts_match(existing_selector_text: &str, selector_text: &str) -> bool {
    let Some(existing) = parse_keyframe_selectors(existing_selector_text) else {
        return false;
    };
    let Some(selector) = parse_keyframe_selectors(selector_text) else {
        return false;
    };
    existing == selector
}

struct ParsedStylesheetForMutation {
    contents: Arc<StylesheetContents>,
    shared_lock: SharedRwLock,
    allow_import_rules: AllowImportRules,
}

struct ParsedRuleForInsert {
    contents: Arc<StylesheetContents>,
    shared_lock: SharedRwLock,
    rule: CssRule,
}

struct ParsedNestedRulesForMutation {
    contents: Arc<StylesheetContents>,
    shared_lock: SharedRwLock,
    rules: CssRules,
    containing_rule_types: CssRuleTypes,
    parse_relative_rule_type: Option<CssRuleType>,
}

struct ParsedKeyframeRulesForMutation {
    contents: Arc<StylesheetContents>,
    shared_lock: SharedRwLock,
    keyframes: Vec<Arc<crate::shared_lock::Locked<Keyframe>>>,
}

impl ParsedNestedRulesForMutation {
    fn parse_rule_for_insert(
        &self,
        rule_text: &str,
        index: usize,
    ) -> Result<CssRule, CssRuleInsertError> {
        self.rules
            .parse_rule_for_insert(
                &self.shared_lock,
                rule_text,
                &self.contents,
                index,
                self.containing_rule_types,
                self.parse_relative_rule_type,
                None,
                AllowImportRules::No,
            )
            .map_err(CssRuleInsertError::from)
    }
}

fn parse_stylesheet_rule_for_insert_rule(
    existing_rule_texts: &[String],
    rule_text: &str,
    index: usize,
    constructed: bool,
) -> Result<ParsedRuleForInsert, CssRuleInsertError> {
    let parsed = parse_stylesheet_for_mutation(existing_rule_texts, constructed)?;
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match parsed.allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let guard = parsed.shared_lock.read();
    let rules = parsed.contents.rules.read_with(&guard);
    let parsed_rule = rules.parse_rule_for_insert(
        &parsed.shared_lock,
        rule_text,
        &parsed.contents,
        index,
        CssRuleTypes::default(),
        None,
        stylesheet_loader,
        parsed.allow_import_rules,
    );
    let rule = match parsed_rule {
        Ok(rule) => rule,
        Err(error) => {
            let error = CssRuleInsertError::from(error);
            if error == CssRuleInsertError::HierarchyRequest
                && rule_text_is_namespace_rule(rule_text)
                && rules
                    .0
                    .iter()
                    .any(|rule| !matches!(rule, CssRule::Import(..) | CssRule::Namespace(..)))
            {
                return Err(CssRuleInsertError::InvalidState);
            }
            return Err(error);
        },
    };
    drop(guard);
    Ok(ParsedRuleForInsert {
        contents: parsed.contents,
        shared_lock: parsed.shared_lock,
        rule,
    })
}

fn parse_stylesheet_for_mutation(
    existing_rule_texts: &[String],
    constructed: bool,
) -> Result<ParsedStylesheetForMutation, CssRuleInsertError> {
    ensure_lightmount_rule_tree_prefs();
    let allow_import_rules = if constructed {
        AllowImportRules::No
    } else {
        AllowImportRules::Yes
    };
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let shared_lock = SharedRwLock::new();
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let existing_css = existing_rule_texts.join(" ");
    let contents = StylesheetContents::from_str(
        &existing_css,
        url_data,
        Origin::Author,
        &shared_lock,
        stylesheet_loader,
        None,
        QuirksMode::NoQuirks,
        allow_import_rules,
        None,
    );
    Ok(ParsedStylesheetForMutation {
        contents,
        shared_lock,
        allow_import_rules,
    })
}

fn parse_stylesheet_rule_tree_with_import_policy(
    css_text: &str,
    allow_import_rules: AllowImportRules,
) -> CssStylesheetRuleTree {
    ensure_lightmount_rule_tree_prefs();
    let shared_lock = SharedRwLock::new();
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let contents = StylesheetContents::from_str(
        css_text,
        about_blank_url_data().expect("static about:blank URL should parse"),
        Origin::Author,
        &shared_lock,
        stylesheet_loader,
        None,
        QuirksMode::NoQuirks,
        allow_import_rules,
        None,
    );
    CssStylesheetRuleTree {
        contents,
        shared_lock,
        allow_import_rules,
    }
}

fn refresh_stylesheet_rule_tree_from_css_text(
    rule_tree: &mut CssStylesheetRuleTree,
    css_text: &str,
) {
    let allow_import_rules = rule_tree.allow_import_rules;
    *rule_tree = parse_stylesheet_rule_tree_with_import_policy(css_text, allow_import_rules);
}

fn mutable_child_rules_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    parent_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<CssRules>>> {
    let rule = rule_at_path(rule_tree, parent_path)?;
    match rule {
        CssRule::Style(ref style_rule) => {
            {
                let guard = rule_tree.shared_lock.read();
                if let Some(rules) = style_rule.read_with(&guard).rules.clone() {
                    return Some(rules);
                }
            }
            let mut guard = rule_tree.shared_lock.write();
            let style_rule = style_rule.write_with(&mut guard);
            if style_rule.rules.is_none() {
                style_rule.rules = Some(CssRules::new(Vec::new(), &rule_tree.shared_lock));
            }
            style_rule.rules.clone()
        },
        _ => {
            let guard = rule_tree.shared_lock.read();
            existing_child_rules_for_rule(&rule, &guard)
        },
    }
}

fn mutable_keyframes_rule_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    parent_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<KeyframesRule>>> {
    match rule_at_path(rule_tree, parent_path)? {
        CssRule::Keyframes(rule) => Some(rule),
        _ => None,
    }
}

fn mutable_media_rule_media_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<MediaList>>> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Media(rule) => Some(rule.media_queries.clone()),
        _ => None,
    }
}

fn mutable_font_face_rule_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<FontFaceRule>>> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::FontFace(rule) => Some(rule),
        _ => None,
    }
}

fn mutable_style_rule_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<StyleRule>>> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Style(rule) => Some(rule),
        _ => None,
    }
}

fn mutable_page_rule_declaration_block_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<PropertyDeclarationBlock>>> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Page(rule) => {
            let guard = rule_tree.shared_lock.read();
            Some(rule.read_with(&guard).block.clone())
        },
        _ => None,
    }
}

fn mutable_page_margin_rule_context_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<(
    MarginRuleType,
    Arc<crate::shared_lock::Locked<PropertyDeclarationBlock>>,
)> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Margin(rule) => Some((rule.rule_type, rule.block.clone())),
        _ => None,
    }
}

fn mutable_style_rule_declaration_block_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<PropertyDeclarationBlock>>> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::Style(rule) => {
            let guard = rule_tree.shared_lock.read();
            Some(rule.read_with(&guard).block.clone())
        },
        _ => None,
    }
}

fn mutable_nested_declarations_rule_block_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    rule_path: &[usize],
) -> Option<Arc<crate::shared_lock::Locked<PropertyDeclarationBlock>>> {
    match rule_at_path(rule_tree, rule_path)? {
        CssRule::NestedDeclarations(rule) => {
            let guard = rule_tree.shared_lock.read();
            Some(rule.read_with(&guard).block.clone())
        },
        _ => None,
    }
}

fn mutable_keyframe_rule_declaration_block_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
) -> Option<Arc<crate::shared_lock::Locked<PropertyDeclarationBlock>>> {
    let keyframe = mutable_keyframe_for_rule_path(rule_tree, parent_path, index)?;
    let guard = rule_tree.shared_lock.read();
    Some(keyframe.read_with(&guard).block.clone())
}

fn mutable_keyframe_for_rule_path(
    rule_tree: &CssStylesheetRuleTree,
    parent_path: &[usize],
    index: usize,
) -> Option<Arc<crate::shared_lock::Locked<Keyframe>>> {
    let keyframes_rule = mutable_keyframes_rule_for_rule_path(rule_tree, parent_path)?;
    let guard = rule_tree.shared_lock.read();
    let keyframes_rule = keyframes_rule.read_with(&guard);
    keyframes_rule.keyframes.get(index).cloned()
}

fn parse_media_list_for_rule(media_text: &str) -> Result<MediaList, CssRuleInsertError> {
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let context = ParserContext::new(
        Origin::Author,
        &url_data,
        Some(CssRuleType::Media),
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Cow::Owned(Namespaces::default()),
        None,
        None,
        AttrTaint::default(),
    );
    let mut input = ParserInput::new(media_text);
    let mut input = Parser::new(&mut input);
    Ok(MediaList::parse(&context, &mut input))
}

fn parse_declaration_block_for_rule(
    declaration_text: &str,
    rule_type: CssRuleType,
) -> Result<PropertyDeclarationBlock, CssRuleInsertError> {
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let context = ParserContext::new(
        Origin::Author,
        &url_data,
        Some(rule_type),
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Cow::Owned(Namespaces::default()),
        None,
        None,
        AttrTaint::default(),
    );
    let mut input = ParserInput::new(declaration_text);
    let mut input = Parser::new(&mut input);
    Ok(parse_property_declaration_list(&context, &mut input, &[]))
}

fn parse_page_margin_declaration_block_for_rule_type(
    rule_type: MarginRuleType,
    declaration_text: &str,
) -> Result<PropertyDeclarationBlock, CssRuleInsertError> {
    let rule_name = format!("{rule_type:?}");
    let rule_tree = parse_stylesheet_rule_tree_with_import_policy(
        &format!("@page {{ @{rule_name} {{ {declaration_text} }} }}"),
        AllowImportRules::No,
    );
    let guard = rule_tree.shared_lock.read();
    let rules = rule_tree.contents.rules.read_with(&guard);
    let Some(CssRule::Page(page_rule)) = rules.0.first() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let page_rule = page_rule.read_with(&guard);
    let margin_rules = page_rule.rules.read_with(&guard);
    let Some(CssRule::Margin(margin_rule)) = margin_rules.0.first() else {
        return Err(CssRuleInsertError::Syntax);
    };
    Ok(margin_rule.block.read_with(&guard).clone())
}

fn parse_font_face_cssom_descriptor_rule(
    descriptor_text: &str,
) -> Result<FontFaceRule, CssRuleInsertError> {
    with_font_face_descriptor_context(|context| {
        let mut rule = FontFaceRule::empty(SourceLocation { line: 0, column: 0 });
        let mut input = ParserInput::new(descriptor_text);
        let mut input = Parser::new(&mut input);
        {
            let mut parser = CssomFontFaceDescriptorParser {
                context,
                rule: &mut rule,
            };
            let iter = RuleBodyParser::new(&mut input, &mut parser);
            for declaration in iter {
                declaration.map_err(|_| CssRuleInsertError::Syntax)?;
            }
        }
        Ok(rule)
    })
}

fn with_font_face_descriptor_context<R>(
    f: impl FnOnce(&ParserContext) -> Result<R, CssRuleInsertError>,
) -> Result<R, CssRuleInsertError> {
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let context = ParserContext::new(
        Origin::Author,
        &url_data,
        Some(CssRuleType::FontFace),
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        Cow::Owned(Namespaces::default()),
        None,
        None,
        AttrTaint::default(),
    );
    f(&context)
}

struct CssomFontFaceDescriptorParser<'a, 'b: 'a> {
    context: &'a ParserContext<'b>,
    rule: &'a mut FontFaceRule,
}

impl<'a, 'b, 'i> cssparser::AtRuleParser<'i> for CssomFontFaceDescriptorParser<'a, 'b> {
    type Prelude = ();
    type AtRule = ();
    type Error = StyleParseErrorKind<'i>;
}

impl<'a, 'b, 'i> cssparser::QualifiedRuleParser<'i> for CssomFontFaceDescriptorParser<'a, 'b> {
    type Prelude = ();
    type QualifiedRule = ();
    type Error = StyleParseErrorKind<'i>;
}

impl<'a, 'b, 'i> RuleBodyItemParser<'i, (), StyleParseErrorKind<'i>>
    for CssomFontFaceDescriptorParser<'a, 'b>
{
    fn parse_qualified(&self) -> bool {
        false
    }

    fn parse_declarations(&self) -> bool {
        true
    }
}

impl<'a, 'b, 'i> DeclarationParser<'i> for CssomFontFaceDescriptorParser<'a, 'b> {
    type Declaration = ();
    type Error = StyleParseErrorKind<'i>;

    fn parse_value<'t>(
        &mut self,
        name: CowRcStr<'i>,
        input: &mut Parser<'i, 't>,
        _declaration_start: &ParserState,
    ) -> Result<(), ParseError<'i>> {
        let Ok(id) = DescriptorId::from_ident(name.as_ref()) else {
            return Err(input.new_custom_error(StyleParseErrorKind::UnexpectedIdent(name.clone())));
        };
        self.rule
            .set_cssom_descriptor_declaration(id, self.context, input)?;
        Ok(())
    }
}

fn rule_at_path(rule_tree: &CssStylesheetRuleTree, path: &[usize]) -> Option<CssRule> {
    let guard = rule_tree.shared_lock.read();
    let (first, rest) = path.split_first()?;
    let rules = rule_tree.contents.rules.read_with(&guard);
    let mut rule = rules.0.get(*first)?.clone();
    for index in rest {
        let child_rules = existing_child_rules_for_rule(&rule, &guard)?;
        rule = child_rules.read_with(&guard).0.get(*index)?.clone();
    }
    Some(rule)
}

fn existing_child_rules_for_rule(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Option<Arc<crate::shared_lock::Locked<CssRules>>> {
    match rule {
        CssRule::Style(rule) => rule.read_with(guard).rules.clone(),
        CssRule::Media(rule) => Some(rule.rules.clone()),
        CssRule::Container(rule) => Some(rule.rules.clone()),
        CssRule::Supports(rule) => Some(rule.rules.clone()),
        CssRule::Page(rule) => Some(rule.read_with(guard).rules.clone()),
        CssRule::Document(rule) => Some(rule.rules.clone()),
        CssRule::LayerBlock(rule) => Some(rule.rules.clone()),
        CssRule::Scope(rule) => Some(rule.rules.clone()),
        CssRule::StartingStyle(rule) => Some(rule.rules.clone()),
        CssRule::AppearanceBase(rule) => Some(rule.rules.clone()),
        _ => None,
    }
}

fn parse_nested_rules_for_mutation(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
    containing_rule_type_bits: u32,
    parse_relative_rule_type: Option<CssRuleType>,
) -> Result<ParsedNestedRulesForMutation, CssRuleInsertError> {
    ensure_lightmount_rule_tree_prefs();
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let shared_lock = SharedRwLock::new();
    let import_loader = LightmountImportLoader;
    let parent_css = parent_stylesheet_rule_texts.join(" ");
    let contents = StylesheetContents::from_str(
        &parent_css,
        url_data,
        Origin::Author,
        &shared_lock,
        Some(&import_loader as &dyn StylesheetLoader),
        None,
        QuirksMode::NoQuirks,
        AllowImportRules::Yes,
        None,
    );
    let containing_rule_types = CssRuleTypes::from_bits(containing_rule_type_bits);
    let mut rules = CssRules(Vec::new());
    for rule_text in existing_rule_texts {
        let rule = rules
            .parse_rule_for_insert(
                &shared_lock,
                rule_text,
                &contents,
                rules.0.len(),
                containing_rule_types,
                parse_relative_rule_type,
                None,
                AllowImportRules::No,
            )
            .map_err(CssRuleInsertError::from)?;
        rules.0.push(rule);
    }
    Ok(ParsedNestedRulesForMutation {
        contents,
        shared_lock,
        rules,
        containing_rule_types,
        parse_relative_rule_type,
    })
}

fn parse_keyframe_rules_for_mutation(
    parent_stylesheet_rule_texts: &[String],
    existing_rule_texts: &[String],
) -> Result<ParsedKeyframeRulesForMutation, CssRuleInsertError> {
    ensure_lightmount_rule_tree_prefs();
    let Some(url_data) = about_blank_url_data() else {
        return Err(CssRuleInsertError::Syntax);
    };
    let shared_lock = SharedRwLock::new();
    let import_loader = LightmountImportLoader;
    let parent_css = parent_stylesheet_rule_texts.join(" ");
    let contents = StylesheetContents::from_str(
        &parent_css,
        url_data,
        Origin::Author,
        &shared_lock,
        Some(&import_loader as &dyn StylesheetLoader),
        None,
        QuirksMode::NoQuirks,
        AllowImportRules::Yes,
        None,
    );
    let mut keyframes = Vec::new();
    for rule_text in existing_rule_texts {
        let rule = Keyframe::parse(rule_text, &contents, &shared_lock)
            .map_err(|_| CssRuleInsertError::Syntax)?;
        keyframes.push(rule);
    }
    Ok(ParsedKeyframeRulesForMutation {
        contents,
        shared_lock,
        keyframes,
    })
}

fn parse_keyframe_selectors(selector_text: &str) -> Option<KeyframeSelectors> {
    let mut input = ParserInput::new(selector_text);
    let mut input = Parser::new(&mut input);
    input.parse_entirely(KeyframeSelectors::parse).ok()
}

fn stylesheet_mutation_result(
    contents: &StylesheetContents,
    shared_lock: &SharedRwLock,
) -> CssStylesheetMutationResult {
    let guard = shared_lock.read();
    let rules = contents.rules.read_with(&guard);
    css_rules_mutation_result(rules, shared_lock)
}

fn css_rules_mutation_result(
    rules: &CssRules,
    shared_lock: &SharedRwLock,
) -> CssStylesheetMutationResult {
    let guard = shared_lock.read();
    let rules = rules
        .0
        .iter()
        .map(|rule| stylesheet_rule_view(rule, &guard))
        .collect::<Vec<_>>();
    let css_text = rules
        .iter()
        .map(|rule| rule.css_text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    CssStylesheetMutationResult {
        css_text,
        rules,
        first_declaration_text: None,
    }
}

fn nested_rule_tree_mutation_result(
    rule_tree: &CssStylesheetRuleTree,
    parent_path: &[usize],
) -> Result<CssNestedRuleMutationResult, CssRuleInsertError> {
    let guard = rule_tree.shared_lock.read();
    let top_rules = rule_tree.contents.rules.read_with(&guard);
    let parent_rule = rule_view_at_path(top_rules.0.as_slice(), parent_path, &guard)
        .ok_or(CssRuleInsertError::HierarchyRequest)?;
    let rules = top_rules
        .0
        .iter()
        .map(|rule| stylesheet_rule_view(rule, &guard))
        .collect::<Vec<_>>();
    Ok(CssNestedRuleMutationResult {
        stylesheet_css_text: css_rule_views_css_text(&rules),
        rules: parent_rule.child_rules.clone(),
        parent_rule,
    })
}

fn rule_view_at_path(
    rules: &[CssRule],
    path: &[usize],
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Option<CssStylesheetRuleView> {
    let (first, rest) = path.split_first()?;
    let rule = rules.get(*first)?;
    if rest.is_empty() {
        return Some(stylesheet_rule_view(rule, guard));
    }
    rule_view_at_path(rule.children(guard), rest, guard)
}

fn css_rule_views_css_text(rules: &[CssStylesheetRuleView]) -> String {
    rules
        .iter()
        .map(|rule| rule.css_text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

fn keyframe_rules_mutation_result(
    keyframes: &[Arc<crate::shared_lock::Locked<Keyframe>>],
    shared_lock: &SharedRwLock,
) -> CssStylesheetMutationResult {
    let guard = shared_lock.read();
    let rules = keyframes
        .iter()
        .map(|rule| keyframe_rule_view(rule, &guard))
        .collect::<Vec<_>>();
    let css_text = rules
        .iter()
        .map(|rule| rule.css_text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    CssStylesheetMutationResult {
        css_text,
        rules,
        first_declaration_text: None,
    }
}

fn css_rules_mutation_result_with_first_declaration_text(
    rules: &CssRules,
    shared_lock: &SharedRwLock,
    first_declaration_text: Option<String>,
) -> CssStylesheetMutationResult {
    let mut result = css_rules_mutation_result(rules, shared_lock);
    result.first_declaration_text = first_declaration_text;
    result
}

fn parse_stylesheet_rule_texts_with_import_policy(
    css_text: &str,
    allow_import_rules: AllowImportRules,
) -> Vec<CssStylesheetRuleText> {
    parse_stylesheet_rule_views_with_import_policy(css_text, allow_import_rules)
        .into_iter()
        .map(|rule| CssStylesheetRuleText {
            rule_type: rule.rule_type,
            css_text: rule.css_text,
        })
        .collect()
}

fn parse_stylesheet_rule_views_with_import_policy(
    css_text: &str,
    allow_import_rules: AllowImportRules,
) -> Vec<CssStylesheetRuleView> {
    ensure_lightmount_rule_tree_prefs();
    let Some(url_data) = about_blank_url_data() else {
        return Vec::new();
    };
    let shared_lock = SharedRwLock::new();
    let import_loader = LightmountImportLoader;
    let stylesheet_loader = match allow_import_rules {
        AllowImportRules::Yes => Some(&import_loader as &dyn StylesheetLoader),
        AllowImportRules::No => None,
    };
    let contents = StylesheetContents::from_str(
        css_text,
        url_data,
        Origin::Author,
        &shared_lock,
        stylesheet_loader,
        None,
        QuirksMode::NoQuirks,
        allow_import_rules,
        None,
    );
    let guard = shared_lock.read();
    contents
        .rules(&guard)
        .iter()
        .map(|rule| stylesheet_rule_view(rule, &guard))
        .collect()
}

fn ensure_lightmount_rule_tree_prefs() {
    static ENABLE: Once = Once::new();
    ENABLE.call_once(|| {
        static_prefs::set_pref!("layout.css.margin-rules.enabled", true);
        static_prefs::set_pref!("layout.css.at-scope.enabled", true);
    });
}

fn stylesheet_rule_view(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> CssStylesheetRuleView {
    CssStylesheetRuleView {
        rule_type: rule.rule_type(),
        css_text: rule.to_css_string(guard),
        prelude_text: stylesheet_rule_prelude_text(rule, guard),
        selector_text: stylesheet_rule_selector_text(rule, guard),
        declaration_text: stylesheet_rule_declaration_text(rule, guard),
        child_rules: stylesheet_rule_child_views(rule, guard),
    }
}

fn stylesheet_rule_prelude_text(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Option<String> {
    match rule {
        CssRule::Media(rule) => Some(rule.media_queries.read_with(guard).to_css_string()),
        CssRule::Supports(rule) => Some(rule.condition.to_css_string()),
        CssRule::Container(rule) => Some(rule.conditions.to_css_string()),
        CssRule::Scope(rule) => Some(scope_rule_condition_text(rule)),
        CssRule::LayerBlock(rule) => Some(
            rule.name
                .as_ref()
                .map(ToCss::to_css_string)
                .unwrap_or_default(),
        ),
        CssRule::LayerStatement(rule) => Some(
            rule.names
                .iter()
                .map(ToCss::to_css_string)
                .collect::<Vec<_>>()
                .join(", "),
        ),
        CssRule::Keyframes(rule) => Some(rule.read_with(guard).name.as_atom().to_string()),
        CssRule::Page(rule) => Some(rule.read_with(guard).selectors.to_css_string()),
        _ => None,
    }
}

fn stylesheet_rule_selector_text(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Option<String> {
    match rule {
        CssRule::Style(rule) => {
            let rule = rule.read_with(guard);
            Some(rule.selectors.to_css_string())
        },
        CssRule::Page(rule) => {
            let rule = rule.read_with(guard);
            Some(rule.selectors.to_css_string())
        },
        _ => None,
    }
}

fn stylesheet_rule_declaration_text(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Option<String> {
    match rule {
        CssRule::Style(rule) => {
            let rule = rule.read_with(guard);
            Some(declaration_block_css_text(&rule.block.read_with(guard)))
        },
        CssRule::NestedDeclarations(rule) => {
            let rule = rule.read_with(guard);
            Some(declaration_block_css_text(&rule.block.read_with(guard)))
        },
        CssRule::FontFace(rule) => {
            let rule = rule.read_with(guard);
            Some(rule.style_css_text())
        },
        CssRule::Page(rule) => {
            let rule = rule.read_with(guard);
            Some(declaration_block_css_text(rule.block.read_with(guard)))
        },
        CssRule::Margin(rule) => Some(declaration_block_css_text(rule.block.read_with(guard))),
        _ => None,
    }
}

fn stylesheet_rule_child_views(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Vec<CssStylesheetRuleView> {
    match rule {
        CssRule::Keyframes(rule) => rule
            .read_with(guard)
            .keyframes
            .iter()
            .map(|rule| keyframe_rule_view(rule, guard))
            .collect(),
        _ => rule
            .children(guard)
            .iter()
            .map(|rule| stylesheet_rule_view(rule, guard))
            .collect(),
    }
}

fn import_rule_view(
    rule: &ImportRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Option<CssImportRuleView> {
    let condition_prefix = import_rule_condition_prefix(rule);
    let media_text = import_rule_media_text(rule, guard);
    let condition_text = if condition_prefix.is_empty() {
        media_text.clone()
    } else if media_text.is_empty() {
        condition_prefix.clone()
    } else {
        format!("{condition_prefix} {media_text}")
    };
    Some(CssImportRuleView {
        css_text: rule.to_css_string(guard),
        href: css_url_href(&rule.url)?,
        condition_text,
        condition_prefix,
        media_text,
        layer_name: import_rule_layer_name(rule),
        supports_text: rule
            .supports
            .as_ref()
            .map(|supports| supports.condition.to_css_string()),
    })
}

fn css_url_href(url: &CssUrl) -> Option<String> {
    let css_text = url.to_css_string();
    let mut input = ParserInput::new(&css_text);
    let mut input = Parser::new(&mut input);
    let href = input.expect_url_or_string().ok()?.as_ref().to_owned();
    input.is_exhausted().then_some(href)
}

fn import_rule_layer_name(rule: &ImportRule) -> Option<String> {
    match &rule.layer {
        ImportLayer::None => None,
        ImportLayer::Anonymous => Some(String::new()),
        ImportLayer::Named(name) => Some(name.to_css_string()),
    }
}

fn import_rule_condition_prefix(rule: &ImportRule) -> String {
    let mut components = Vec::new();
    if !matches!(rule.layer, ImportLayer::None) {
        components.push(rule.layer.to_css_string());
    }
    if let Some(supports) = &rule.supports {
        components.push(format!("supports({})", supports.condition.to_css_string()));
    }
    components.join(" ")
}

fn import_rule_media_text(
    rule: &ImportRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> String {
    rule.stylesheet
        .media(guard)
        .filter(|media| !media.is_empty())
        .map(ToCss::to_css_string)
        .unwrap_or_default()
}

fn namespace_rule_view(
    rule: &crate::stylesheets::NamespaceRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> CssNamespaceRuleView {
    CssNamespaceRuleView {
        css_text: rule.to_css_string(guard),
        prefix: rule
            .prefix
            .as_ref()
            .map(|prefix| prefix.0.to_string())
            .unwrap_or_default(),
        namespace_uri: rule.url.to_string(),
    }
}

fn condition_rule_view(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Option<CssConditionRuleView> {
    match rule {
        CssRule::Media(rule) => Some(CssConditionRuleView {
            rule_type: CssRuleType::Media,
            css_text: rule.to_css_string(guard),
            condition_text: rule.media_queries.read_with(guard).to_css_string(),
            container_name: None,
            container_query: None,
            scope_start: None,
            scope_end: None,
        }),
        CssRule::Supports(rule) => Some(CssConditionRuleView {
            rule_type: CssRuleType::Supports,
            css_text: rule.to_css_string(guard),
            condition_text: rule.condition.to_css_string(),
            container_name: None,
            container_query: None,
            scope_start: None,
            scope_end: None,
        }),
        CssRule::Container(rule) => {
            let (container_name, container_query) =
                container_rule_cssom_name_and_query(&rule.conditions);
            Some(CssConditionRuleView {
                rule_type: CssRuleType::Container,
                css_text: rule.to_css_string(guard),
                condition_text: rule.conditions.to_css_string(),
                container_name,
                container_query,
                scope_start: None,
                scope_end: None,
            })
        },
        CssRule::Scope(rule) => Some(CssConditionRuleView {
            rule_type: CssRuleType::Scope,
            css_text: rule.to_css_string(guard),
            condition_text: scope_rule_condition_text(rule),
            container_name: None,
            container_query: None,
            scope_start: rule
                .bounds
                .start
                .as_ref()
                .map(cssparser::ToCss::to_css_string),
            scope_end: rule
                .bounds
                .end
                .as_ref()
                .map(cssparser::ToCss::to_css_string),
        }),
        _ => None,
    }
}

fn container_rule_cssom_name_and_query(
    conditions: &crate::stylesheets::container_rule::ContainerConditions,
) -> (Option<String>, Option<String>) {
    let Some(first) = conditions.0.iter().next() else {
        return (None, None);
    };
    let name = (!first.name().is_none()).then(|| first.name().to_css_string());
    let mut query_parts = Vec::new();
    if name.is_some() {
        if let Some(condition) = first.query_condition() {
            query_parts.push(condition.to_css_string());
        }
    } else {
        query_parts.push(first.to_css_string());
    }
    query_parts.extend(conditions.0.iter().skip(1).map(ToCss::to_css_string));
    let query = (!query_parts.is_empty()).then(|| query_parts.join(", "));
    (name, query)
}

fn scope_rule_condition_text(rule: &crate::stylesheets::ScopeRule) -> String {
    let mut components = Vec::new();
    if let Some(start) = rule.bounds.start.as_ref() {
        components.push(format!("({})", cssparser::ToCss::to_css_string(start)));
    }
    if let Some(end) = rule.bounds.end.as_ref() {
        components.push(format!("to ({})", cssparser::ToCss::to_css_string(end)));
    }
    components.join(" ")
}

fn layer_rule_view(
    rule: &CssRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> Option<CssLayerRuleView> {
    match rule {
        CssRule::LayerBlock(rule) => Some(CssLayerRuleView {
            rule_type: CssRuleType::LayerBlock,
            css_text: rule.to_css_string(guard),
            name: rule.name.as_ref().map(ToCss::to_css_string),
            names: Vec::new(),
        }),
        CssRule::LayerStatement(rule) => Some(CssLayerRuleView {
            rule_type: CssRuleType::LayerStatement,
            css_text: rule.to_css_string(guard),
            name: None,
            names: rule.names.iter().map(ToCss::to_css_string).collect(),
        }),
        _ => None,
    }
}

fn page_rule_view(
    rule: &PageRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> CssPageRuleView {
    let child_rules = rule
        .rules
        .read_with(guard)
        .0
        .iter()
        .filter_map(|rule| {
            let CssRule::Margin(rule) = rule else {
                return None;
            };
            Some(margin_rule_view(rule, guard))
        })
        .collect();
    CssPageRuleView {
        css_text: rule.to_css_string(guard),
        selector_text: rule.selectors.to_css_string(),
        style_text: declaration_block_css_text(rule.block.read_with(guard)),
        child_rules,
    }
}

fn margin_rule_view(
    rule: &MarginRule,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> CssMarginRuleView {
    CssMarginRuleView {
        css_text: rule.to_css_string(guard),
        name: rule.name().to_owned(),
        style_text: declaration_block_css_text(rule.block.read_with(guard)),
    }
}

const CSSOM_PAGE_DESCRIPTOR_NAMES: &[&str] = &[
    "margin",
    "margin-left",
    "margin-right",
    "margin-top",
    "margin-bottom",
    "page-orientation",
    "size",
    "marks",
    "bleed",
];

fn canonical_page_descriptor_name(name: &str) -> Option<&'static str> {
    match name {
        "size" => Some("size"),
        "page-orientation" => Some("page-orientation"),
        "margin" => Some("margin"),
        "margin-top" => Some("margin-top"),
        "margin-right" => Some("margin-right"),
        "margin-bottom" => Some("margin-bottom"),
        "margin-left" => Some("margin-left"),
        _ => None,
    }
}

fn parse_page_descriptor_declaration(name: &str, value: &str) -> Option<PropertyDeclarationBlock> {
    let property = PropertyId::parse_enabled_for_all_content(name).ok()?;
    let url_data = about_blank_url_data()?;
    let mut declarations = SourcePropertyDeclaration::default();
    parse_one_declaration_into(
        &mut declarations,
        property,
        value,
        Origin::Author,
        &url_data,
        None,
        ParsingMode::DEFAULT,
        QuirksMode::NoQuirks,
        CssRuleType::Page,
    )
    .ok()?;
    let mut block = PropertyDeclarationBlock::default();
    block.extend(declarations.drain(), Importance::Normal);
    Some(block)
}

fn page_descriptor_entries_from_block(
    block: &PropertyDeclarationBlock,
) -> Vec<CssPageDescriptorEntryView> {
    block
        .declaration_importance_iter()
        .filter_map(|(declaration, _importance)| {
            let mut name = String::new();
            declaration
                .id()
                .to_css(&mut CssWriter::new(&mut name))
                .ok()?;
            let mut value = CssStringWriter::new();
            declaration.to_css(&mut value).ok()?;
            Some(CssPageDescriptorEntryView { name, value })
        })
        .collect()
}

fn page_descriptor_entries_match_name(name: &str, entries: &[CssPageDescriptorEntryView]) -> bool {
    match name {
        "margin" => entries
            .iter()
            .all(|entry| page_margin_longhand_name(&entry.name)),
        "margin-top" | "margin-right" | "margin-bottom" | "margin-left" => {
            entries.len() == 1 && entries[0].name == name
        },
        "size" | "page-orientation" => entries.len() == 1 && entries[0].name == name,
        _ => false,
    }
}

fn page_margin_longhand_name(name: &str) -> bool {
    matches!(
        name,
        "margin-top" | "margin-right" | "margin-bottom" | "margin-left"
    )
}

fn declaration_block_css_text(block: &PropertyDeclarationBlock) -> String {
    let mut css_text = CssStringWriter::new();
    block
        .to_css(&mut css_text)
        .expect("serializing a declaration block to string should not fail");
    css_text.trim_end().to_owned()
}

fn keyframe_rule_view(
    rule: &Arc<crate::shared_lock::Locked<Keyframe>>,
    guard: &crate::shared_lock::SharedRwLockReadGuard,
) -> CssStylesheetRuleView {
    let rule = rule.read_with(guard);
    CssStylesheetRuleView {
        rule_type: CssRuleType::Keyframe,
        css_text: rule.to_css_string(guard),
        prelude_text: None,
        selector_text: Some(rule.selector.to_css_string()),
        declaration_text: Some(declaration_block_css_text(&rule.block.read_with(guard))),
        child_rules: Vec::new(),
    }
}

fn about_blank_url_data() -> Option<UrlExtraData> {
    Some(UrlExtraData::from(url::Url::parse("about:blank").ok()?))
}

fn rule_text_is_namespace_rule(rule_text: &str) -> bool {
    parse_stylesheet_rule_texts_with_import_policy(rule_text, AllowImportRules::Yes)
        .as_slice()
        .is_single_rule_type(CssRuleType::Namespace)
}

trait SingleRuleType {
    fn is_single_rule_type(&self, rule_type: CssRuleType) -> bool;
}

impl SingleRuleType for [CssStylesheetRuleText] {
    fn is_single_rule_type(&self, rule_type: CssRuleType) -> bool {
        matches!(self, [rule] if rule.rule_type == rule_type)
    }
}

impl From<RulesMutateError> for CssRuleInsertError {
    fn from(value: RulesMutateError) -> Self {
        match value {
            RulesMutateError::Syntax => Self::Syntax,
            RulesMutateError::IndexSize => Self::IndexSize,
            RulesMutateError::HierarchyRequest => Self::HierarchyRequest,
            RulesMutateError::InvalidState => Self::InvalidState,
        }
    }
}

struct LightmountImportLoader;

impl StylesheetLoader for LightmountImportLoader {
    fn request_stylesheet(
        &self,
        url: CssUrl,
        location: SourceLocation,
        lock: &SharedRwLock,
        media: Arc<crate::shared_lock::Locked<MediaList>>,
        supports: Option<ImportSupportsCondition>,
        layer: ImportLayer,
    ) -> Arc<crate::shared_lock::Locked<ImportRule>> {
        let contents = StylesheetContents::from_str(
            "",
            about_blank_url_data().expect("static about:blank URL should parse"),
            Origin::Author,
            lock,
            None,
            None,
            QuirksMode::NoQuirks,
            AllowImportRules::No,
            None,
        );
        let stylesheet = Arc::new(Stylesheet {
            contents: lock.wrap(contents),
            shared_lock: lock.clone(),
            media,
            disabled: AtomicBool::new(false),
        });
        Arc::new(lock.wrap(ImportRule {
            url,
            stylesheet: ImportSheet::new(stylesheet),
            supports,
            layer,
            source_location: location,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        delete_keyframe_rule, delete_keyframe_rule_from_stylesheet_rule_tree, delete_nested_rule,
        delete_nested_rule_from_stylesheet_rule_tree, delete_rule_from_stylesheet_rule_tree,
        delete_stylesheet_rule, font_face_descriptor_names, insert_keyframe_rule,
        insert_keyframe_rule_into_stylesheet_rule_tree, insert_nested_rule,
        insert_nested_rule_into_stylesheet_rule_tree, insert_rule_into_stylesheet_rule_tree,
        insert_stylesheet_rule, keyframe_selector_texts_match, normalize_keyframe_selector_text,
        normalize_page_selector_text, page_descriptor_names, parse_condition_rule_view,
        parse_constructed_stylesheet_rule_texts, parse_constructed_stylesheet_rule_tree,
        parse_counter_style_rule_view, parse_font_face_cssom_descriptor_block,
        parse_font_face_cssom_descriptor_entry, parse_font_face_rule_view,
        parse_font_feature_values_rule_view, parse_import_rule_view, parse_keyframes_rule_view,
        parse_layer_rule_view, parse_namespace_rule_view, parse_nested_rule_block_views,
        parse_page_descriptor_entries, parse_page_margin_descriptor_block,
        parse_page_margin_rule_view, parse_page_rule_view, parse_property_rule_view,
        parse_stylesheet_rule_for_insert, parse_stylesheet_rule_texts, parse_stylesheet_rule_tree,
        parse_stylesheet_rule_view_for_insert, parse_stylesheet_rule_views,
        replace_keyframe_rule_in_stylesheet_rule_tree, replace_nested_rule_in_stylesheet_rule_tree,
        replace_rule_in_stylesheet_rule_tree, serialize_stylesheet,
        set_font_face_rule_descriptor_in_stylesheet_rule_tree,
        set_font_face_rule_descriptors_in_stylesheet_rule_tree, set_font_feature_values_rule_entry,
        set_keyframe_rule_declarations_in_stylesheet_rule_tree,
        set_keyframe_rule_selector_in_stylesheet_rule_tree,
        set_media_rule_media_in_stylesheet_rule_tree,
        set_nested_declarations_rule_declarations_in_stylesheet_rule_tree,
        set_page_margin_rule_descriptors_in_stylesheet_rule_tree,
        set_page_rule_descriptors_in_stylesheet_rule_tree,
        set_style_rule_declarations_in_stylesheet_rule_tree,
        set_style_rule_selector_in_stylesheet_rule_tree, stylesheet_rule_tree_condition_rule_view,
        stylesheet_rule_tree_counter_style_rule_view, stylesheet_rule_tree_css_text,
        stylesheet_rule_tree_font_face_rule_view,
        stylesheet_rule_tree_font_feature_values_rule_view, stylesheet_rule_tree_import_rule_view,
        stylesheet_rule_tree_keyframes_rule_view, stylesheet_rule_tree_layer_rule_view,
        stylesheet_rule_tree_margin_rule_view, stylesheet_rule_tree_namespace_rule_view,
        stylesheet_rule_tree_page_rule_view, stylesheet_rule_tree_property_rule_view,
        stylesheet_rule_tree_rule_views, CssRuleInsertError,
    };
    use crate::stylesheets::CssRuleType;

    #[test]
    fn stylesheet_rule_texts_use_stylo_rule_parser_and_serializer() {
        let rules = parse_stylesheet_rule_texts(
            "  @import url(\"a.css\"); .one { color: red; } @media screen { .two { margin: 0; } }",
        );

        assert_eq!(rules.len(), 3);
        assert_eq!(rules[0].rule_type, CssRuleType::Import);
        assert_eq!(rules[0].css_text, "@import url(\"a.css\");");
        assert_eq!(rules[1].rule_type, CssRuleType::Style);
        assert_eq!(rules[1].css_text, ".one { color: red; }");
        assert_eq!(rules[2].rule_type, CssRuleType::Media);
        assert_eq!(
            rules[2].css_text,
            "@media screen {\n  .two { margin: 0px; }\n}"
        );
    }

    #[test]
    fn parse_import_rule_view_exposes_import_conditions() {
        let view = parse_import_rule_view(
            r#"@import url("support/c.css") layer(A.B) supports((display: flex) or (foo: bar)) print and (WiDtH);"#,
        )
        .expect("valid @import should produce a CSSOM view");

        assert_eq!(view.href, "support/c.css");
        assert_eq!(view.layer_name.as_deref(), Some("A.B"));
        assert_eq!(
            view.supports_text.as_deref(),
            Some("(display: flex) or (foo: bar)")
        );
        assert_eq!(view.media_text, "print and (width)");
        assert_eq!(
            view.condition_prefix,
            "layer(A.B) supports((display: flex) or (foo: bar))"
        );
        assert_eq!(
            view.condition_text,
            "layer(A.B) supports((display: flex) or (foo: bar)) print and (width)"
        );
        assert_eq!(
            view.css_text,
            r#"@import url("support/c.css") layer(A.B) supports((display: flex) or (foo: bar)) print and (width);"#
        );

        let anonymous =
            parse_import_rule_view(r#"@import "theme.css" layer;"#).expect("anonymous layer");
        assert_eq!(anonymous.href, "theme.css");
        assert_eq!(anonymous.layer_name.as_deref(), Some(""));
        assert_eq!(anonymous.condition_prefix, "layer");
        assert_eq!(anonymous.media_text, "");
        assert!(parse_import_rule_view(r#"@import url("theme.css") supports();"#).is_none());
        assert!(parse_import_rule_view(".not-import { color: red; }").is_none());
    }

    #[test]
    fn parse_namespace_rule_view_exposes_cssom_fields() {
        let view = parse_namespace_rule_view(r#"@namespace svg url(http://servo);"#)
            .expect("valid @namespace should produce a CSSOM view");

        assert_eq!(view.prefix, "svg");
        assert_eq!(view.namespace_uri, "http://servo");
        assert_eq!(view.css_text, r#"@namespace svg url("http://servo");"#);

        let default = parse_namespace_rule_view(r#"@namespace "http://www.w3.org/1999/xhtml";"#)
            .expect("default namespace should produce a CSSOM view");
        assert_eq!(default.prefix, "");
        assert_eq!(default.namespace_uri, "http://www.w3.org/1999/xhtml");
        assert_eq!(
            default.css_text,
            r#"@namespace url("http://www.w3.org/1999/xhtml");"#
        );
        assert!(parse_namespace_rule_view("@namespace svg;").is_none());
        assert!(parse_namespace_rule_view(".not-namespace { color: red; }").is_none());
    }

    #[test]
    fn parse_condition_rule_view_exposes_cssom_fields() {
        let media = parse_condition_rule_view("@media SCREEN and (WiDtH) {}")
            .expect("media rule should parse");
        assert_eq!(media.rule_type, CssRuleType::Media);
        assert_eq!(media.condition_text, "screen and (width)");
        assert_eq!(media.css_text, "@media screen and (width) {\n}");

        let supports = parse_condition_rule_view("@supports ((display: grid) or (foo: bar)) {}")
            .expect("supports rule should parse");
        assert_eq!(supports.rule_type, CssRuleType::Supports);
        assert_eq!(supports.condition_text, "((display: grid) or (foo: bar))");

        let container = parse_condition_rule_view("@container card (inline-size > 10px) {}")
            .expect("container rule should parse");
        assert_eq!(container.rule_type, CssRuleType::Container);
        assert_eq!(container.condition_text, "card (inline-size > 10px)");
        assert_eq!(container.container_name.as_deref(), Some("card"));
        assert_eq!(
            container.container_query.as_deref(),
            Some("(inline-size > 10px)")
        );

        let anonymous_container = parse_condition_rule_view("@container (width > 10px) {}")
            .expect("anonymous container rule should parse");
        assert_eq!(anonymous_container.container_name, None);
        assert_eq!(
            anonymous_container.container_query.as_deref(),
            Some("(width > 10px)")
        );

        let scope =
            parse_condition_rule_view("@scope (.a) to (> .b) {}").expect("scope rule should parse");
        assert_eq!(scope.rule_type, CssRuleType::Scope);
        assert_eq!(scope.condition_text, "(.a) to (> .b)");
        assert_eq!(scope.scope_start.as_deref(), Some(".a"));
        assert_eq!(scope.scope_end.as_deref(), Some("> .b"));

        assert!(parse_condition_rule_view("@layer a {}").is_none());
    }

    #[test]
    fn parse_layer_rule_view_exposes_cssom_fields() {
        let block =
            parse_layer_rule_view(r"@layer abc\;oops\! {}").expect("layer block should parse");
        assert_eq!(block.rule_type, CssRuleType::LayerBlock);
        assert_eq!(block.name.as_deref(), Some(r"abc\;oops\!"));
        assert!(block.names.is_empty());
        assert_eq!(block.css_text, "@layer abc\\;oops\\! {\n}");

        let statement =
            parse_layer_rule_view("@layer A, B.C.D;").expect("layer statement should parse");
        assert_eq!(statement.rule_type, CssRuleType::LayerStatement);
        assert_eq!(statement.name, None);
        assert_eq!(statement.names, vec!["A", "B.C.D"]);
        assert_eq!(statement.css_text, "@layer A, B.C.D;");

        assert!(parse_layer_rule_view("@media screen {}").is_none());
    }

    #[test]
    fn stylesheet_rule_views_include_stylo_nested_children() {
        let rules = parse_stylesheet_rule_views(
            ".one { color: red; } @media screen { .two { margin: 0; } @supports (display: grid) { .three { display: grid; } } }",
        );

        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].rule_type, CssRuleType::Style);
        assert_eq!(rules[0].selector_text.as_deref(), Some(".one"));
        assert_eq!(rules[0].declaration_text.as_deref(), Some("color: red;"));
        assert!(rules[0].child_rules.is_empty());
        assert_eq!(rules[1].rule_type, CssRuleType::Media);
        assert_eq!(
            rules[1].css_text,
            "@media screen {\n  .two { margin: 0px; }\n  @supports (display: grid) {\n  .three { display: grid; }\n}\n}"
        );
        assert_eq!(rules[1].child_rules.len(), 2);
        assert_eq!(rules[1].child_rules[0].rule_type, CssRuleType::Style);
        assert_eq!(rules[1].child_rules[0].css_text, ".two { margin: 0px; }");
        assert_eq!(
            rules[1].child_rules[0].selector_text.as_deref(),
            Some(".two")
        );
        assert_eq!(
            rules[1].child_rules[0].declaration_text.as_deref(),
            Some("margin: 0px;")
        );
        assert_eq!(rules[1].child_rules[1].rule_type, CssRuleType::Supports);
        assert_eq!(rules[1].child_rules[1].child_rules.len(), 1);
        assert_eq!(
            rules[1].child_rules[1].child_rules[0].css_text,
            ".three { display: grid; }"
        );
    }

    #[test]
    fn parse_nested_rule_block_views_uses_stylo_nested_parser() {
        let parent = vec![String::from(
            r#"@namespace svg url("http://www.w3.org/2000/svg"); .host { color: red; }"#,
        )];
        let parsed = parse_nested_rule_block_views(
            &parent,
            "color: red; & svg|path { color: blue; } --after: 1;",
            CssRuleType::Style,
            CssRuleType::Style.bit(),
            Some(CssRuleType::Style),
            true,
        )
        .expect("style nested block should parse");

        assert_eq!(
            parsed.first_declaration_text.as_deref(),
            Some("color: red;")
        );
        assert_eq!(parsed.rules.len(), 2);
        assert_eq!(parsed.rules[0].rule_type, CssRuleType::Style);
        assert_eq!(parsed.rules[0].selector_text.as_deref(), Some("& svg|path"));
        assert_eq!(
            parsed.rules[0].declaration_text.as_deref(),
            Some("color: blue;")
        );
        assert_eq!(parsed.rules[1].rule_type, CssRuleType::NestedDeclarations);
        assert_eq!(
            parsed.rules[1].declaration_text.as_deref(),
            Some("--after: 1;")
        );
    }

    #[test]
    fn parse_nested_rule_block_views_preserves_grouping_direct_declarations() {
        let parent = vec![String::from(".host { @media screen { color: red; } }")];
        let parsed = parse_nested_rule_block_views(
            &parent,
            "color: red; & .child { color: blue; }",
            CssRuleType::Media,
            CssRuleType::Style.bit() | CssRuleType::Media.bit(),
            Some(CssRuleType::Style),
            false,
        )
        .expect("nested grouping block should parse");

        assert_eq!(parsed.first_declaration_text, None);
        assert_eq!(parsed.rules.len(), 2);
        assert_eq!(parsed.rules[0].rule_type, CssRuleType::NestedDeclarations);
        assert_eq!(
            parsed.rules[0].declaration_text.as_deref(),
            Some("color: red;")
        );
        assert_eq!(parsed.rules[1].rule_type, CssRuleType::Style);
        assert_eq!(parsed.rules[1].selector_text.as_deref(), Some("& .child"));
    }

    #[test]
    fn stylesheet_rule_views_include_keyframes_children() {
        let rules = parse_stylesheet_rule_views(
            "@keyframes slide { from { opacity: 0; } to { opacity: 1; } }",
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::Keyframes);
        assert_eq!(rules[0].prelude_text.as_deref(), Some("slide"));
        assert_eq!(rules[0].child_rules.len(), 2);
        assert_eq!(rules[0].child_rules[0].rule_type, CssRuleType::Keyframe);
        assert_eq!(rules[0].child_rules[0].css_text, "0% { opacity: 0; }");
        assert_eq!(rules[0].child_rules[0].selector_text.as_deref(), Some("0%"));
        assert_eq!(
            rules[0].child_rules[0].declaration_text.as_deref(),
            Some("opacity: 0;")
        );
        assert_eq!(rules[0].child_rules[1].css_text, "100% { opacity: 1; }");
        assert_eq!(
            rules[0].child_rules[1].selector_text.as_deref(),
            Some("100%")
        );

        let view = parse_keyframes_rule_view(
            r#"@keyframes "slide show" { from { opacity: 0; } to { opacity: 1; } }"#,
        )
        .expect("valid @keyframes should produce a CSSOM view");
        assert_eq!(view.name, "slide show");
        assert_eq!(
            view.css_text,
            "@keyframes slide\\ show {\n0% { opacity: 0; }\n100% { opacity: 1; }\n}"
        );
        assert!(
            parse_keyframes_rule_view("@keyframes none { from { opacity: 0; } }").is_none(),
            "Stylo rejects invalid keyframes names"
        );
    }

    #[test]
    fn stylesheet_rule_views_include_counter_style_rules() {
        let rules = parse_stylesheet_rule_views(
            r#"@counter-style thumbs { system: cyclic; symbols: "*"; suffix: " "; }"#,
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::CounterStyle);
        assert_eq!(
            rules[0].css_text,
            r#"@counter-style thumbs { system: cyclic; suffix: " "; symbols: "*"; }"#
        );
        assert!(rules[0].child_rules.is_empty());

        let view = parse_counter_style_rule_view(
            r#"@counter-style thumbs { system: cyclic; symbols: "*"; suffix: " "; }"#,
        )
        .expect("valid @counter-style should produce a CSSOM view");
        assert_eq!(view.name, "thumbs");
        assert_eq!(view.css_text, rules[0].css_text);
        assert!(
            parse_counter_style_rule_view(
                r#"@counter-style thumbs { system: cyclic; suffix: " "; }"#
            )
            .is_none(),
            "Stylo rejects counter styles whose system requires symbols"
        );
    }

    #[test]
    fn parse_property_rule_view_exposes_property_registration() {
        let view = parse_property_rule_view(
            r#"@property --accent { syntax: "<color>"; inherits: false; initial-value: red; }"#,
        )
        .expect("property rule should parse");

        assert_eq!(
            view.css_text,
            r#"@property --accent { syntax: "<color>"; inherits: false; initial-value: red; }"#
        );
        assert_eq!(view.name, "--accent");
        assert_eq!(view.syntax, "<color>");
        assert!(!view.inherits);
        assert_eq!(view.initial_value.as_deref(), Some("red"));
        assert!(
            parse_property_rule_view(
                r#"@property --accent { syntax: "<color>"; inherits: false; initial-value: 10px; }"#
            )
            .is_none(),
            "Stylo rejects initial values that do not match the syntax descriptor"
        );
    }

    #[test]
    fn stylesheet_rule_views_include_font_face_rules() {
        let rules = parse_stylesheet_rule_views(
            r#"@font-face { src: url(http://foo/bar/font.ttf); font-family: Foo; font-weight: bold; }"#,
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::FontFace);
        assert_eq!(
            rules[0].css_text,
            r#"@font-face { font-family: Foo; src: url("http://foo/bar/font.ttf"); font-weight: bold; }"#
        );
        assert_eq!(
            rules[0].declaration_text.as_deref(),
            Some(r#"font-family: Foo; src: url("http://foo/bar/font.ttf"); font-weight: bold;"#)
        );
        assert!(rules[0].child_rules.is_empty());

        let view = parse_font_face_rule_view(
            r#"@font-face { src: url(http://foo/bar/font.ttf); font-family: Foo; font-weight: bold; }"#,
        )
        .expect("valid @font-face should produce a CSSOM view");
        assert_eq!(view.css_text, rules[0].css_text);
        assert_eq!(
            view.style_text,
            r#"font-family: Foo; src: url("http://foo/bar/font.ttf"); font-weight: bold;"#
        );
        assert!(parse_font_face_rule_view(".not-a-font-face { font-family: Foo; }").is_none());
    }

    #[test]
    fn font_face_source_parse_rejects_descriptor_important() {
        let view = parse_font_face_rule_view(
            "@font-face { font-family: Foo !important; src: local(Foo); }",
        )
        .expect("font-face rule should still parse after dropping invalid descriptor");

        assert_eq!(view.style_text, "src: local(Foo);");
        assert!(!view.css_text.contains("font-family"));
        assert!(!view.css_text.contains("!important"));
    }

    #[test]
    fn font_face_cssom_descriptor_block_preserves_priority() {
        assert_eq!(
            parse_font_face_cssom_descriptor_block(
                "font-family: Bar !important; src: local(Bar); font-display: swap !important;"
            )
            .as_deref(),
            Some("font-family: Bar !important; src: local(Bar); font-display: swap !important;")
        );
        assert!(
            parse_font_face_cssom_descriptor_block("font-family: Bar !important extra;").is_none()
        );
    }

    #[test]
    fn font_face_cssom_descriptor_entry_parses_value_fragments() {
        let entry = parse_font_face_cssom_descriptor_entry("src", "local(Bar")
            .expect("CSSOM descriptor entry should parse value fragments at EOF");
        assert_eq!(entry.name, "src");
        assert_eq!(entry.value, "local(Bar)");

        let family = parse_font_face_cssom_descriptor_entry("font-family", "Bar")
            .expect("font-family descriptor entry should parse");
        assert_eq!(family.name, "font-family");
        assert_eq!(family.value, "Bar");

        assert!(
            parse_font_face_cssom_descriptor_entry(
                "src",
                r#"url("a.woff2"); font-family: injected"#
            )
            .is_none(),
            "CSSOM descriptor value fragments must not parse declaration injection"
        );
        assert!(
            parse_font_face_cssom_descriptor_entry("font-weight", "400 !important").is_none(),
            "single descriptor entry keeps priority outside the value"
        );
        assert!(
            parse_font_face_cssom_descriptor_entry("font", "16px serif").is_none(),
            "ordinary properties must not enter the font-face descriptor entry API"
        );
    }

    #[test]
    fn font_face_rule_tree_descriptor_mutations_preserve_priority() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@media screen { @font-face { font-family: Foo; src: local(Foo); } }",
        );

        let mutation = set_font_face_rule_descriptor_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0, 0],
            "font-family",
            "Bar",
            true,
        )
        .expect("font-family descriptor mutation should succeed");
        assert_eq!(
            mutation.parent_rule.css_text,
            "@font-face { font-family: Bar !important; src: local(Foo); }"
        );
        assert_eq!(
            mutation.parent_rule.declaration_text.as_deref(),
            Some("font-family: Bar !important; src: local(Foo);")
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@media screen {\n  @font-face { font-family: Bar !important; src: local(Foo); }\n}"
        );

        let mutation = set_font_face_rule_descriptors_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0, 0],
            "font-family: Baz; src: local(Baz) !important;",
        )
        .expect("font-face descriptor block mutation should succeed");
        assert_eq!(
            mutation.parent_rule.css_text,
            "@font-face { font-family: Baz; src: local(Baz) !important; }"
        );
        assert_eq!(
            mutation.parent_rule.declaration_text.as_deref(),
            Some("font-family: Baz; src: local(Baz) !important;")
        );

        let mutation = set_font_face_rule_descriptor_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0, 0],
            "src",
            "",
            true,
        )
        .expect("empty value should remove descriptor");
        assert_eq!(
            mutation.parent_rule.css_text,
            "@font-face { font-family: Baz; }"
        );
        assert!(
            set_font_face_rule_descriptor_in_stylesheet_rule_tree(
                &mut rule_tree,
                &[0, 0],
                "font-weight",
                "400 !important",
                true,
            )
            .is_err(),
            "single descriptor mutation keeps priority separate from value"
        );
    }

    #[test]
    fn stylesheet_rule_views_include_font_feature_values_rules() {
        let rules = parse_stylesheet_rule_views(
            "@font-feature-values test_family { @annotation { the_first: 6; } @styleset { yo: 7; di: 10 9 4 5; } }",
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::FontFeatureValues);
        assert_eq!(
            rules[0].css_text,
            "@font-feature-values test_family {\n@annotation {\nthe_first: 6;\n}\n@styleset {\nyo: 7;\ndi: 10 9 4 5;\n}\n}"
        );
        assert!(rules[0].child_rules.is_empty());

        let view = parse_font_feature_values_rule_view(
            "@font-feature-values test_family { @annotation { the_first: 6; } @character-variant { cv: 2 3; } @styleset { yo: 7; di: 10 9 4 5; } }",
        )
        .expect("valid @font-feature-values should produce a CSSOM view");
        assert_eq!(view.font_family, "test_family");
        assert_eq!(
            view.css_text,
            "@font-feature-values test_family {\n@annotation {\nthe_first: 6;\n}\n@character-variant {\ncv: 2 3;\n}\n@styleset {\nyo: 7;\ndi: 10 9 4 5;\n}\n}"
        );
        assert_eq!(view.annotation.len(), 1);
        assert_eq!(view.annotation[0].name, "the_first");
        assert_eq!(view.annotation[0].values, vec![6]);
        assert_eq!(view.character_variant.len(), 1);
        assert_eq!(view.character_variant[0].name, "cv");
        assert_eq!(view.character_variant[0].values, vec![2, 3]);
        assert_eq!(view.styleset.len(), 2);
        assert_eq!(view.styleset[0].name, "yo");
        assert_eq!(view.styleset[0].values, vec![7]);
        assert_eq!(view.styleset[1].name, "di");
        assert_eq!(view.styleset[1].values, vec![10, 9, 4, 5]);
        assert!(
            parse_font_feature_values_rule_view(
                "@font-feature-values serif { @annotation { the_first: 6; } }"
            )
            .is_none(),
            "Stylo rejects generic family names"
        );
    }

    #[test]
    fn font_feature_values_rule_entry_mutation_uses_stylo_rule_storage() {
        let css_text =
            "@font-feature-values test_family { @annotation { the_first: 6; } @styleset { yo: 7; } }";

        let css_text =
            set_font_feature_values_rule_entry(css_text, "annotation", "the_first", &[9])
                .expect("annotation entry update should serialize");
        let view = parse_font_feature_values_rule_view(&css_text)
            .expect("updated font-feature-values rule should parse");
        assert_eq!(css_text, view.css_text);
        assert_eq!(view.annotation[0].name, "the_first");
        assert_eq!(view.annotation[0].values, vec![9]);
        assert_eq!(view.styleset[0].name, "yo");
        assert_eq!(view.styleset[0].values, vec![7]);

        let css_text = set_font_feature_values_rule_entry(&css_text, "styleset", "wide", &[1, 2])
            .expect("styleset entry append should serialize");
        let view = parse_font_feature_values_rule_view(&css_text)
            .expect("appended font-feature-values rule should parse");
        assert_eq!(
            view.styleset
                .iter()
                .find(|entry| entry.name == "wide")
                .map(|entry| entry.values.as_slice()),
            Some(&[1, 2][..])
        );

        assert!(
            set_font_feature_values_rule_entry(&css_text, "annotation", "bad", &[1, 2]).is_none(),
            "single-value groups reject multiple values"
        );
        assert!(
            set_font_feature_values_rule_entry(&css_text, "styleset", "empty", &[]).is_none(),
            "vector groups reject empty values"
        );
    }

    #[test]
    fn stylesheet_rule_tree_exposes_typed_rule_views_by_path() {
        let css_text = concat!(
            r#"@import url("support/c.css") layer(A.B) print;"#,
            r#"@namespace svg url(http://servo);"#,
            r#"@counter-style thumbs { system: cyclic; symbols: "*"; suffix: " "; }"#,
            r#"@font-face { font-family: Foo; src: local(Foo); }"#,
            r#"@property --accent { syntax: "<color>"; inherits: false; initial-value: red; }"#,
            r#"@font-feature-values test_family { @annotation { the_first: 6; } }"#,
            "@keyframes slide { from { opacity: 0; } to { opacity: 1; } }",
            "@media screen {}",
            "@layer A.B {}",
        );
        let rule_tree = parse_stylesheet_rule_tree(css_text);

        assert_eq!(
            stylesheet_rule_tree_import_rule_view(&rule_tree, &[0]),
            parse_import_rule_view(r#"@import url("support/c.css") layer(A.B) print;"#)
        );
        assert_eq!(
            stylesheet_rule_tree_namespace_rule_view(&rule_tree, &[1]),
            parse_namespace_rule_view(r#"@namespace svg url(http://servo);"#)
        );
        assert_eq!(
            stylesheet_rule_tree_counter_style_rule_view(&rule_tree, &[2]),
            parse_counter_style_rule_view(
                r#"@counter-style thumbs { system: cyclic; symbols: "*"; suffix: " "; }"#
            )
        );
        assert_eq!(
            stylesheet_rule_tree_font_face_rule_view(&rule_tree, &[3]),
            parse_font_face_rule_view(r#"@font-face { font-family: Foo; src: local(Foo); }"#)
        );
        assert_eq!(
            stylesheet_rule_tree_property_rule_view(&rule_tree, &[4]),
            parse_property_rule_view(
                r#"@property --accent { syntax: "<color>"; inherits: false; initial-value: red; }"#
            )
        );
        assert_eq!(
            stylesheet_rule_tree_font_feature_values_rule_view(&rule_tree, &[5]),
            parse_font_feature_values_rule_view(
                r#"@font-feature-values test_family { @annotation { the_first: 6; } }"#
            )
        );
        assert_eq!(
            stylesheet_rule_tree_keyframes_rule_view(&rule_tree, &[6]),
            parse_keyframes_rule_view(
                "@keyframes slide { from { opacity: 0; } to { opacity: 1; } }"
            )
        );
        assert_eq!(
            stylesheet_rule_tree_condition_rule_view(&rule_tree, &[7])
                .map(|view| view.condition_text),
            Some("screen".to_owned())
        );
        assert_eq!(
            stylesheet_rule_tree_rule_views(&rule_tree)[7]
                .prelude_text
                .as_deref(),
            Some("screen")
        );
        assert_eq!(
            stylesheet_rule_tree_layer_rule_view(&rule_tree, &[8]).and_then(|view| view.name),
            Some("A.B".to_owned())
        );
        assert_eq!(
            stylesheet_rule_tree_rule_views(&rule_tree)[8]
                .prelude_text
                .as_deref(),
            Some("A.B")
        );
    }

    #[test]
    fn stylesheet_rule_views_include_page_rules() {
        let css_text =
            r#"@page :first { margin-top: 1px; @top-left { content: "x"; color: red; } }"#;
        let rules = parse_stylesheet_rule_views(css_text);

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::Page);
        assert_eq!(
            rules[0].css_text,
            "@page :first {\n  margin-top: 1px;\n  @top-left { content: \"x\"; color: red; }\n}"
        );
        assert_eq!(
            rules[0].declaration_text.as_deref(),
            Some("margin-top: 1px;")
        );
        assert_eq!(rules[0].child_rules.len(), 1);
        assert_eq!(rules[0].child_rules[0].rule_type, CssRuleType::Margin);
        assert_eq!(
            rules[0].child_rules[0].css_text,
            "@top-left { content: \"x\"; color: red; }"
        );
        assert_eq!(
            rules[0].child_rules[0].declaration_text.as_deref(),
            Some("content: \"x\"; color: red;")
        );

        let rule_tree = parse_stylesheet_rule_tree(css_text);
        let page_view = stylesheet_rule_tree_page_rule_view(&rule_tree, &[0])
            .expect("page view should be available by rule path");
        assert_eq!(page_view.selector_text, ":first");
        assert_eq!(page_view.style_text, "margin-top: 1px;");
        let margin_view = stylesheet_rule_tree_margin_rule_view(&rule_tree, &[0, 0])
            .expect("margin view should be available by rule path");
        assert_eq!(margin_view.name, "top-left");
        assert_eq!(margin_view.style_text, "content: \"x\"; color: red;");
    }

    #[test]
    fn parse_page_rule_view_exposes_descriptors_and_margin_children() {
        let view = parse_page_rule_view(
            r#"@page :first { margin-top: 1px; @top-left { content: "x"; color: red; } }"#,
        )
        .expect("page rule should parse");

        assert_eq!(
            view.css_text,
            "@page :first {\n  margin-top: 1px;\n  @top-left { content: \"x\"; color: red; }\n}"
        );
        assert_eq!(view.selector_text, ":first");
        assert_eq!(view.style_text, "margin-top: 1px;");
        assert_eq!(view.child_rules.len(), 1);
        assert_eq!(view.child_rules[0].name, "top-left");
        assert_eq!(
            view.child_rules[0].style_text,
            "content: \"x\"; color: red;"
        );
        assert_eq!(
            view.child_rules[0].css_text,
            "@top-left { content: \"x\"; color: red; }"
        );
    }

    #[test]
    fn page_descriptor_entries_use_page_rule_declaration_block() {
        let entries = parse_page_descriptor_entries("margin", "1px 2px 3px 4px")
            .expect("margin shorthand should parse in page context");
        assert_eq!(
            entries
                .iter()
                .map(|entry| (entry.name.as_str(), entry.value.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("margin-top", "1px"),
                ("margin-right", "2px"),
                ("margin-bottom", "3px"),
                ("margin-left", "4px"),
            ]
        );

        assert!(parse_page_descriptor_entries("margin-top", "5px").is_some());
        assert_eq!(
            parse_page_descriptor_entries("size", "portrait")
                .expect("size descriptor should parse in page context")
                .iter()
                .map(|entry| (entry.name.as_str(), entry.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("size", "portrait")]
        );
        assert_eq!(
            parse_page_descriptor_entries("page-orientation", "rotate-left")
                .expect("page-orientation descriptor should parse in page context")
                .iter()
                .map(|entry| (entry.name.as_str(), entry.value.as_str()))
                .collect::<Vec<_>>(),
            vec![("page-orientation", "rotate-left")]
        );
        assert!(parse_page_descriptor_entries("marks", "crop").is_none());
        assert!(parse_page_descriptor_entries("margin-top", "1px; margin-bottom: 2px").is_none());
        assert!(parse_page_descriptor_entries("margin-top", "1px !important").is_none());
        assert!(parse_page_descriptor_entries("margin-top", "1px } @page { size: a4").is_none());
    }

    #[test]
    fn descriptor_name_metadata_exposes_cssom_accessor_surface() {
        let font_face = font_face_descriptor_names();
        assert!(font_face.contains(&"font-display"));
        assert!(font_face.contains(&"ascent-override"));
        assert!(font_face.contains(&"size-adjust"));

        let page = page_descriptor_names();
        assert!(page.contains(&"margin-top"));
        assert!(page.contains(&"page-orientation"));
        assert!(page.contains(&"marks"));
        assert!(page.contains(&"bleed"));
        assert!(parse_page_descriptor_entries("marks", "crop").is_none());
        assert!(parse_page_descriptor_entries("bleed", "1mm").is_none());
    }

    #[test]
    fn page_rule_tree_descriptor_mutation_preserves_margin_children() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            r#"@page :first { margin-top: 1px; @top-left { content: "x"; color: red; } }"#,
        );
        let mutation = set_page_rule_descriptors_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "size: A4; margin: 2px 3px; bad-descriptor: 1;",
        )
        .expect("page descriptor block mutation should succeed");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Page);
        let view = parse_page_rule_view(&mutation.parent_rule.css_text)
            .expect("mutated page rule should stay parseable");
        assert_eq!(view.selector_text, ":first");
        assert_eq!(view.child_rules.len(), 1);
        assert_eq!(
            view.child_rules[0].css_text,
            r#"@top-left { content: "x"; color: red; }"#
        );
        assert!(view.style_text.contains("size:"));
        assert!(view.style_text.contains("margin"));
        assert!(view.style_text.contains("2px"));
        assert!(!view.style_text.contains("bad-descriptor"));
        assert!(mutation.stylesheet_css_text.contains("@top-left"));
    }

    #[test]
    fn page_margin_rule_tree_descriptor_mutation_preserves_parent_page() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            r#"@page :first { margin-top: 1px; @bottom-right { content: "x"; color: red; } }"#,
        );
        let mutation = set_page_margin_rule_descriptors_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0, 0],
            r#"content: "y"; color: blue; margin-top: 4px; bad-descriptor: 1;"#,
        )
        .expect("page margin descriptor block mutation should succeed");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Margin);
        let margin_view = parse_page_margin_rule_view(&mutation.parent_rule.css_text)
            .expect("mutated margin rule should stay parseable");
        assert_eq!(margin_view.name, "bottom-right");
        assert_eq!(
            margin_view.style_text,
            r#"content: "y"; color: blue; margin-top: 4px;"#
        );
        assert!(!margin_view.style_text.contains("bad-descriptor"));

        let rules = stylesheet_rule_tree_rule_views(&rule_tree);
        let page_view = parse_page_rule_view(&rules[0].css_text)
            .expect("parent page rule should stay parseable");
        assert_eq!(page_view.selector_text, ":first");
        assert_eq!(page_view.child_rules.len(), 1);
        assert_eq!(page_view.child_rules[0], margin_view);
        assert!(page_view.css_text.contains("margin-top: 1px"));
    }

    #[test]
    fn parse_page_margin_descriptor_block_uses_requested_rule_type() {
        let block = parse_page_margin_descriptor_block(
            "bottom-right",
            r#"content: "x"; color: red; margin-top: 4px; bad-descriptor: 1;"#,
        )
        .expect("page margin descriptor block should parse in requested margin context");

        assert_eq!(block, r#"content: "x"; color: red; margin-top: 4px;"#);
        assert!(parse_page_margin_descriptor_block("not-a-margin", r#"content: "x";"#).is_none());
    }

    #[test]
    fn parse_page_margin_rule_view_uses_page_context() {
        let view = parse_page_margin_rule_view(r#"@top-left { content: "x"; color: red; }"#)
            .expect("margin rule should parse in page context");

        assert_eq!(view.name, "top-left");
        assert_eq!(view.style_text, "content: \"x\"; color: red;");
        assert_eq!(view.css_text, "@top-left { content: \"x\"; color: red; }");
        assert!(parse_page_margin_rule_view("@media screen { }").is_none());
    }

    #[test]
    fn constructed_stylesheet_rule_texts_drop_import_rules() {
        let rules = parse_constructed_stylesheet_rule_texts(
            "@import url(\"ignored.css\"); .target { color: blue; }",
        );

        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].rule_type, CssRuleType::Style);
        assert_eq!(rules[0].css_text, ".target { color: blue; }");
    }

    #[test]
    fn stylesheet_serialization_joins_stylo_serialized_rule_texts() {
        assert_eq!(
            serialize_stylesheet(".one { padding: 0 1px; } .two { display: block; }"),
            ".one { padding: 0px 1px; } .two { display: block; }"
        );
    }

    #[test]
    fn insert_rule_parser_uses_stylo_ordering_and_serialization() {
        let existing = vec![
            String::from("@import url(\"a.css\");"),
            String::from("@namespace svg url(\"http://www.w3.org/2000/svg\");"),
            String::from(".one { color: red; }"),
        ];

        let inserted = parse_stylesheet_rule_for_insert(&existing, ".two { margin: 0; }", 3, false)
            .expect("style rule should insert");
        assert_eq!(inserted.rule_type, CssRuleType::Style);
        assert_eq!(inserted.css_text, ".two { margin: 0px; }");

        assert_eq!(
            parse_stylesheet_rule_for_insert(&existing, "@import url(\"late.css\");", 3, false),
            Err(CssRuleInsertError::HierarchyRequest)
        );
        assert_eq!(
            parse_stylesheet_rule_for_insert(
                &existing,
                "@namespace html url(\"http://www.w3.org/1999/xhtml\");",
                3,
                false,
            ),
            Err(CssRuleInsertError::InvalidState)
        );
        assert_eq!(
            parse_stylesheet_rule_for_insert(&existing, ".too-far {}", 4, false),
            Err(CssRuleInsertError::IndexSize)
        );
    }

    #[test]
    fn insert_rule_view_parser_uses_parent_namespace_context() {
        let existing = vec![String::from("@namespace ns\\:odd url(\"ns\");")];
        let inserted = parse_stylesheet_rule_view_for_insert(
            &existing,
            r#"[ns\:odd|odd\:name] { color: red; }"#,
            1,
            false,
        )
        .expect("style rule should parse with parent namespace context");

        assert_eq!(inserted.rule_type, CssRuleType::Style);
        assert_eq!(
            inserted.selector_text.as_deref(),
            Some(r#"[ns\:odd|odd\:name]"#)
        );
        assert_eq!(inserted.declaration_text.as_deref(), Some("color: red;"));
        assert_eq!(inserted.css_text, r#"[ns\:odd|odd\:name] { color: red; }"#);
    }

    #[test]
    fn constructed_insert_rule_parser_rejects_import_rules() {
        assert_eq!(
            parse_stylesheet_rule_for_insert(&[], "@import url(\"ignored.css\");", 0, true),
            Err(CssRuleInsertError::Syntax)
        );
    }

    #[test]
    fn insert_stylesheet_rule_returns_mutated_rule_tree_view() {
        let existing = vec![
            String::from("@import url(\"a.css\");"),
            String::from(".one { color: red; }"),
        ];

        let mutation = insert_stylesheet_rule(
            &existing,
            "@media screen { .two { padding: 0 1px; } }",
            2,
            false,
        )
        .expect("media rule should insert");

        assert_eq!(mutation.rules.len(), 3);
        assert_eq!(
            mutation.css_text,
            "@import url(\"a.css\"); .one { color: red; } @media screen {\n  .two { padding: 0px 1px; }\n}"
        );
        assert_eq!(mutation.rules[2].rule_type, CssRuleType::Media);
        assert_eq!(mutation.rules[2].child_rules.len(), 1);
        assert_eq!(
            mutation.rules[2].child_rules[0].css_text,
            ".two { padding: 0px 1px; }"
        );
    }

    #[test]
    fn delete_stylesheet_rule_uses_stylo_remove_semantics() {
        let existing = vec![
            String::from("@namespace svg url(\"http://www.w3.org/2000/svg\");"),
            String::from(".one { color: red; }"),
            String::from(".two { color: blue; }"),
        ];

        assert_eq!(
            delete_stylesheet_rule(&existing, 0, false),
            Err(CssRuleInsertError::InvalidState)
        );

        let mutation =
            delete_stylesheet_rule(&existing, 1, false).expect("style rule should be removable");
        assert_eq!(mutation.rules.len(), 2);
        assert_eq!(
            mutation.css_text,
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); .two { color: blue; }"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_without_reparsing_rule_texts() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); .one { color: red; }",
        );

        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); .one { color: red; }"
        );
        assert_eq!(
            insert_rule_into_stylesheet_rule_tree(
                &mut rule_tree,
                "@namespace html url(\"http://www.w3.org/1999/xhtml\");",
                2,
            ),
            Err(CssRuleInsertError::InvalidState)
        );

        let inserted = insert_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            "@media screen { svg|path { color: blue; } }",
            2,
        )
        .expect("media rule should insert into existing persistent rule tree");
        assert_eq!(inserted.rules.len(), 3);
        assert_eq!(inserted.rules[2].rule_type, CssRuleType::Media);
        assert_eq!(
            inserted.rules[2].child_rules[0].css_text,
            "svg|path { color: blue; }"
        );
        assert_eq!(
            stylesheet_rule_tree_rule_views(&rule_tree)[2].css_text,
            "@media screen {\n  svg|path { color: blue; }\n}"
        );

        let deleted = delete_rule_from_stylesheet_rule_tree(&mut rule_tree, 1)
            .expect("style rule should delete from same persistent tree");
        assert_eq!(deleted.rules.len(), 2);
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); @media screen {\n  svg|path { color: blue; }\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_refreshes_namespaces_after_insert() {
        let mut rule_tree = parse_stylesheet_rule_tree("");

        insert_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            "@namespace svg url(\"http://www.w3.org/2000/svg\");",
            0,
        )
        .expect("namespace rule should insert");
        let inserted =
            insert_rule_into_stylesheet_rule_tree(&mut rule_tree, "svg|a { color: white; }", 1)
                .expect("style rule should see namespace inserted into persistent tree");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(inserted.rules[1].css_text, "svg|a { color: white; }");
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); svg|a { color: white; }"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_refreshes_namespaces_after_delete() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); svg|a { color: white; }",
        );

        delete_rule_from_stylesheet_rule_tree(&mut rule_tree, 1)
            .expect("style rule should delete before namespace");
        delete_rule_from_stylesheet_rule_tree(&mut rule_tree, 0)
            .expect("namespace rule should delete once no style rules remain");

        assert_eq!(stylesheet_rule_tree_css_text(&rule_tree), "");
        assert_eq!(
            insert_rule_into_stylesheet_rule_tree(&mut rule_tree, "svg|a { color: blue; }", 0),
            Err(CssRuleInsertError::Syntax)
        );
    }

    #[test]
    fn persistent_constructed_stylesheet_rule_tree_rejects_import_insert() {
        let mut rule_tree = parse_constructed_stylesheet_rule_tree(".one { color: red; }");

        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            ".one { color: red; }"
        );
        assert_eq!(
            insert_rule_into_stylesheet_rule_tree(
                &mut rule_tree,
                "@import url(\"ignored.css\");",
                0,
            ),
            Err(CssRuleInsertError::Syntax)
        );
    }

    #[test]
    fn nested_rule_mutation_returns_media_rule_tree_views() {
        let existing = vec![String::from(".one { color: red; }")];
        let inserted = insert_nested_rule(
            &[],
            &existing,
            "@supports (display: grid) { .two { display: grid; } }",
            1,
            CssRuleType::Media.bit(),
            None,
        )
        .expect("supports rule should insert into media rule");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(
            inserted.css_text,
            ".one { color: red; } @supports (display: grid) {\n  .two { display: grid; }\n}"
        );
        assert_eq!(inserted.rules[1].rule_type, CssRuleType::Supports);
        assert_eq!(inserted.rules[1].child_rules.len(), 1);
        assert_eq!(
            inserted.rules[1].child_rules[0].css_text,
            ".two { display: grid; }"
        );

        let deleted = delete_nested_rule(
            &[],
            &inserted
                .rules
                .iter()
                .map(|rule| rule.css_text.clone())
                .collect::<Vec<_>>(),
            0,
            CssRuleType::Media.bit(),
            None,
        )
        .expect("nested style rule should delete");
        assert_eq!(deleted.rules.len(), 1);
        assert_eq!(
            deleted.css_text,
            "@supports (display: grid) {\n  .two { display: grid; }\n}"
        );
    }

    #[test]
    fn nested_style_rule_mutation_parses_relative_selectors_and_declarations() {
        let parent_context = vec![String::from(
            "@namespace svg url(\"http://www.w3.org/2000/svg\");",
        )];
        let existing = vec![String::from("& .one { color: red; }")];

        let inserted = insert_nested_rule(
            &parent_context,
            &existing,
            "> svg|path { color: blue; }",
            1,
            CssRuleType::Style.bit(),
            Some(CssRuleType::Style),
        )
        .expect("relative style rule should insert into style rule");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(inserted.rules[0].css_text, "& .one { color: red; }");
        assert_eq!(inserted.rules[1].css_text, "& > svg|path { color: blue; }");

        let declarations = insert_nested_rule(
            &parent_context,
            &inserted
                .rules
                .iter()
                .map(|rule| rule.css_text.clone())
                .collect::<Vec<_>>(),
            "margin: 0; padding: 1px;",
            0,
            CssRuleType::Style.bit(),
            Some(CssRuleType::Style),
        )
        .expect("declarations should insert as nested declarations");

        assert_eq!(declarations.rules.len(), 3);
        assert_eq!(
            declarations.rules[0].rule_type,
            CssRuleType::NestedDeclarations
        );
        assert_eq!(declarations.rules[0].css_text, "margin: 0px; padding: 1px;");
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_nested_grouping_rules() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); @media screen { .one { color: red; } }",
        );

        let inserted = insert_nested_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            &[1],
            "svg|path { color: blue; }",
            1,
            CssRuleType::Media.bit(),
            None,
        )
        .expect("nested rule should insert into persistent media rule");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(inserted.rules[1].css_text, "svg|path { color: blue; }");
        assert_eq!(
            inserted.parent_rule.css_text,
            "@media screen {\n  .one { color: red; }\n  svg|path { color: blue; }\n}"
        );
        assert_eq!(
            inserted.stylesheet_css_text,
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); @media screen {\n  .one { color: red; }\n  svg|path { color: blue; }\n}"
        );

        let deleted = delete_nested_rule_from_stylesheet_rule_tree(&mut rule_tree, &[1], 0)
            .expect("nested rule should delete from persistent media rule");
        assert_eq!(deleted.rules.len(), 1);
        assert_eq!(deleted.rules[0].css_text, "svg|path { color: blue; }");
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); @media screen {\n  svg|path { color: blue; }\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_creates_style_rule_children() {
        let mut rule_tree = parse_stylesheet_rule_tree(".host { color: red; }");

        let inserted = insert_nested_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "> .child { color: blue; }",
            0,
            CssRuleType::Style.bit(),
            Some(CssRuleType::Style),
        )
        .expect("style rule should get a persistent nested rule list");

        assert_eq!(inserted.rules.len(), 1);
        assert_eq!(inserted.rules[0].css_text, "& > .child { color: blue; }");
        assert_eq!(
            inserted.parent_rule.css_text,
            ".host {\n  color: red;\n  & > .child { color: blue; }\n}"
        );

        let deleted = delete_nested_rule_from_stylesheet_rule_tree(&mut rule_tree, &[0], 0)
            .expect("style rule child should delete");
        assert!(deleted.rules.is_empty());
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            ".host { color: red; }"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_replaces_top_level_style_rules() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); .one { color: red; } .after { color: black; }",
        );

        let mutation = replace_rule_in_stylesheet_rule_tree(
            &mut rule_tree,
            "svg|path { color: blue; & > .icon { opacity: .5; } }",
            1,
        )
        .expect("style rule should replace in persistent tree");

        assert_eq!(mutation.rules.len(), 3);
        assert_eq!(mutation.rules[1].rule_type, CssRuleType::Style);
        assert_eq!(mutation.rules[1].child_rules.len(), 1);
        assert_eq!(
            mutation.rules[1].child_rules[0].css_text,
            "& > .icon { opacity: 0.5; }"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@namespace svg url(\"http://www.w3.org/2000/svg\"); svg|path {\n  color: blue;\n  & > .icon { opacity: 0.5; }\n} .after { color: black; }"
        );
        assert_eq!(
            replace_rule_in_stylesheet_rule_tree(&mut rule_tree, ".missing { color: red; }", 9),
            Err(CssRuleInsertError::IndexSize)
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_replaces_nested_style_rules() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            ".host { color: red; & .old { color: blue; } font-size: 12px; }",
        );

        let mutation = replace_nested_rule_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "> .new { color: green; }",
            0,
            CssRuleType::Style.bit(),
            Some(CssRuleType::Style),
        )
        .expect("nested style rule should replace in persistent tree");

        assert_eq!(mutation.rules.len(), 2);
        assert_eq!(mutation.rules[0].css_text, "& > .new { color: green; }");
        assert_eq!(mutation.rules[1].css_text, "font-size: 12px;");
        assert_eq!(
            mutation.parent_rule.css_text,
            ".host {\n  color: red;\n  & > .new { color: green; }\n  font-size: 12px;\n}"
        );
        assert_eq!(
            replace_nested_rule_in_stylesheet_rule_tree(
                &mut rule_tree,
                &[0],
                ".missing { color: red; }",
                9,
                CssRuleType::Style.bit(),
                Some(CssRuleType::Style),
            ),
            Err(CssRuleInsertError::IndexSize)
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_deep_nested_rule_path() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@media screen { @supports (display: grid) { .one { display: grid; } } }",
        );

        let inserted = insert_nested_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            &[0, 0],
            ".two { display: contents; }",
            1,
            CssRuleType::Media.bit() | CssRuleType::Supports.bit(),
            None,
        )
        .expect("deep supports rule should mutate in persistent tree");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(inserted.rules[1].css_text, ".two { display: contents; }");
        assert_eq!(
            inserted.parent_rule.css_text,
            "@supports (display: grid) {\n  .one { display: grid; }\n  .two { display: contents; }\n}"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@media screen {\n  @supports (display: grid) {\n  .one { display: grid; }\n  .two { display: contents; }\n}\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_media_rule_media_list() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@media screen { @supports (display: grid) { .one { display: grid; } } }",
        );

        let mutation = set_media_rule_media_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "print and (min-width: 10px)",
        )
        .expect("media rule should update in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Media);
        assert_eq!(
            mutation.parent_rule.css_text,
            "@media print and (min-width: 10px) {\n  @supports (display: grid) {\n  .one { display: grid; }\n}\n}"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@media print and (min-width: 10px) {\n  @supports (display: grid) {\n  .one { display: grid; }\n}\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_style_rule_declarations() {
        let mut rule_tree =
            parse_stylesheet_rule_tree(".host { color: red; & > .child { color: blue; } }");

        let mutation = set_style_rule_declarations_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "margin: 1px 2px; color: green;",
        )
        .expect("style rule declarations should update in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Style);
        assert_eq!(mutation.parent_rule.child_rules.len(), 1);
        assert_eq!(
            mutation.parent_rule.child_rules[0].css_text,
            "& > .child { color: blue; }"
        );
        assert_eq!(
            mutation.parent_rule.css_text,
            ".host {\n  margin: 1px 2px; color: green;\n  & > .child { color: blue; }\n}"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            ".host {\n  margin: 1px 2px; color: green;\n  & > .child { color: blue; }\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_nested_declarations() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            ".host { & .child { color: blue; } color: red; margin: 0; }",
        );

        let mutation = set_nested_declarations_rule_declarations_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0, 1],
            "padding: 1px 2px;",
        )
        .expect("nested declarations should update in persistent tree");

        assert_eq!(
            mutation.parent_rule.rule_type,
            CssRuleType::NestedDeclarations
        );
        assert_eq!(mutation.parent_rule.css_text, "padding: 1px 2px;");
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            ".host {\n  & .child { color: blue; }\n  padding: 1px 2px;\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_keyframe_declarations() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@keyframes fade { from { opacity: 0; } to { opacity: 1; } }",
        );

        let mutation = set_keyframe_rule_declarations_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            1,
            "opacity: .5; transform: translateX(10px);",
        )
        .expect("keyframe declarations should update in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Keyframes);
        assert_eq!(mutation.rules.len(), 2);
        assert_eq!(mutation.rules[0].css_text, "0% { opacity: 0; }");
        assert_eq!(
            mutation.rules[1].css_text,
            "100% { opacity: 0.5; transform: translateX(10px); }"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@keyframes fade {\n0% { opacity: 0; }\n100% { opacity: 0.5; transform: translateX(10px); }\n}"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_style_rule_selectors() {
        let mut rule_tree =
            parse_stylesheet_rule_tree(".one { color: red; } .host { & .child { color: blue; } }");

        let mutation = set_style_rule_selector_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            ".renamed, main > .item",
            0,
            None,
        )
        .expect("top-level style rule selector should update in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Style);
        assert_eq!(
            mutation.parent_rule.css_text,
            ".renamed, main > .item { color: red; }"
        );
        assert_eq!(
            mutation.parent_rule.selector_text.as_deref(),
            Some(".renamed, main > .item")
        );

        let nested = set_style_rule_selector_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[1, 0],
            "> .next",
            CssRuleType::Style.bit(),
            Some(CssRuleType::Style),
        )
        .expect("nested style rule selector should parse relative selector context");

        assert_eq!(nested.parent_rule.rule_type, CssRuleType::Style);
        assert_eq!(
            nested.parent_rule.selector_text.as_deref(),
            Some("& > .next")
        );
        assert_eq!(nested.parent_rule.css_text, "& > .next { color: blue; }");
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            ".renamed, main > .item { color: red; } .host {\n  & > .next { color: blue; }\n}"
        );
        assert_eq!(
            set_style_rule_selector_in_stylesheet_rule_tree(&mut rule_tree, &[0], "@bad", 0, None),
            Err(CssRuleInsertError::Syntax)
        );
        assert_eq!(
            set_style_rule_selector_in_stylesheet_rule_tree(
                &mut rule_tree,
                &[9],
                ".missing",
                0,
                None
            ),
            Err(CssRuleInsertError::HierarchyRequest)
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_keyframe_selectors() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@keyframes fade { from { opacity: 0; } to { opacity: 1; } }",
        );

        let mutation =
            set_keyframe_rule_selector_in_stylesheet_rule_tree(&mut rule_tree, &[0], 1, "75%, to")
                .expect("keyframe selector should update in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Keyframes);
        assert_eq!(mutation.rules.len(), 2);
        assert_eq!(mutation.rules[0].css_text, "0% { opacity: 0; }");
        assert_eq!(mutation.rules[1].css_text, "75%, 100% { opacity: 1; }");
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@keyframes fade {\n0% { opacity: 0; }\n75%, 100% { opacity: 1; }\n}"
        );
        assert_eq!(
            set_keyframe_rule_selector_in_stylesheet_rule_tree(&mut rule_tree, &[0], 9, "50%"),
            Err(CssRuleInsertError::IndexSize)
        );
        assert_eq!(
            set_keyframe_rule_selector_in_stylesheet_rule_tree(&mut rule_tree, &[0], 0, "body"),
            Err(CssRuleInsertError::Syntax)
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_replaces_keyframe_rules() {
        let mut rule_tree = parse_stylesheet_rule_tree(
            "@keyframes fade { from { opacity: 0; } to { opacity: 1; } }",
        );

        let mutation = replace_keyframe_rule_in_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "80% { opacity: .8; transform: scale(1); }",
            1,
        )
        .expect("keyframe child should replace in persistent tree");

        assert_eq!(mutation.parent_rule.rule_type, CssRuleType::Keyframes);
        assert_eq!(mutation.rules.len(), 2);
        assert_eq!(mutation.rules[0].css_text, "0% { opacity: 0; }");
        assert_eq!(
            mutation.rules[1].css_text,
            "80% { opacity: 0.8; transform: scale(1); }"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@keyframes fade {\n0% { opacity: 0; }\n80% { opacity: 0.8; transform: scale(1); }\n}"
        );
        assert_eq!(
            replace_keyframe_rule_in_stylesheet_rule_tree(
                &mut rule_tree,
                &[0],
                "body { opacity: 0; }",
                1,
            ),
            Err(CssRuleInsertError::Syntax)
        );
    }

    #[test]
    fn keyframe_rule_mutation_returns_keyframe_views() {
        let existing = vec![String::from("0% { opacity: 0; }")];
        let inserted = insert_keyframe_rule(
            &[],
            &existing,
            "to { opacity: 1; transform: translateX(10px); }",
            1,
        )
        .expect("keyframe rule should insert into keyframes rule");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(
            inserted.css_text,
            "0% { opacity: 0; } 100% { opacity: 1; transform: translateX(10px); }"
        );
        assert_eq!(inserted.rules[1].rule_type, CssRuleType::Keyframe);
        assert_eq!(
            inserted.rules[1].css_text,
            "100% { opacity: 1; transform: translateX(10px); }"
        );

        let deleted = delete_keyframe_rule(
            &[],
            &inserted
                .rules
                .iter()
                .map(|rule| rule.css_text.clone())
                .collect::<Vec<_>>(),
            0,
        )
        .expect("keyframe rule should delete");
        assert_eq!(deleted.rules.len(), 1);
        assert_eq!(
            deleted.css_text,
            "100% { opacity: 1; transform: translateX(10px); }"
        );
    }

    #[test]
    fn persistent_stylesheet_rule_tree_mutates_keyframes_rules() {
        let mut rule_tree = parse_stylesheet_rule_tree("@keyframes fade { from { opacity: 0; } }");

        let inserted = insert_keyframe_rule_into_stylesheet_rule_tree(
            &mut rule_tree,
            &[0],
            "to { opacity: 1; transform: translateX(10px); }",
            1,
        )
        .expect("keyframe rule should insert into persistent keyframes rule");

        assert_eq!(inserted.rules.len(), 2);
        assert_eq!(inserted.rules[0].css_text, "0% { opacity: 0; }");
        assert_eq!(
            inserted.rules[1].css_text,
            "100% { opacity: 1; transform: translateX(10px); }"
        );
        assert_eq!(
            inserted.parent_rule.css_text,
            "@keyframes fade {\n0% { opacity: 0; }\n100% { opacity: 1; transform: translateX(10px); }\n}"
        );
        assert_eq!(inserted.stylesheet_css_text, inserted.parent_rule.css_text);

        let deleted = delete_keyframe_rule_from_stylesheet_rule_tree(&mut rule_tree, &[0], 0)
            .expect("keyframe rule should delete from persistent keyframes rule");
        assert_eq!(deleted.rules.len(), 1);
        assert_eq!(
            deleted.rules[0].css_text,
            "100% { opacity: 1; transform: translateX(10px); }"
        );
        assert_eq!(
            stylesheet_rule_tree_css_text(&rule_tree),
            "@keyframes fade {\n100% { opacity: 1; transform: translateX(10px); }\n}"
        );
    }

    #[test]
    fn keyframe_rule_mutation_rejects_invalid_keyframe_rules() {
        assert_eq!(
            insert_keyframe_rule(&[], &[], ".not-a-keyframe { opacity: 1; }", 0),
            Err(CssRuleInsertError::Syntax)
        );
        assert_eq!(
            delete_keyframe_rule(&[], &[], 0),
            Err(CssRuleInsertError::IndexSize)
        );
    }

    #[test]
    fn keyframe_selector_helpers_use_stylo_parser_and_serialization() {
        assert_eq!(normalize_keyframe_selector_text("from"), Some("0%".into()));
        assert_eq!(
            normalize_keyframe_selector_text("50%, to"),
            Some("50%, 100%".into())
        );
        assert_eq!(normalize_keyframe_selector_text("body"), None);
        assert_eq!(normalize_keyframe_selector_text("-1%"), None);

        assert!(keyframe_selector_texts_match("from", "0%"));
        assert!(keyframe_selector_texts_match("50%, to", "50%, 100%"));
        assert!(!keyframe_selector_texts_match("50%, to", "50%"));
        assert!(!keyframe_selector_texts_match("body", "body"));
    }

    #[test]
    fn page_selector_helper_uses_stylo_page_selectors() {
        assert_eq!(normalize_page_selector_text(""), Some(String::new()));
        assert_eq!(
            normalize_page_selector_text("named:First:left"),
            Some("named:first:left".to_owned())
        );
        assert_eq!(
            normalize_page_selector_text(":RIGHT"),
            Some(":right".to_owned())
        );
        assert_eq!(
            normalize_page_selector_text(":first, named:left"),
            Some(":first, named:left".to_owned())
        );
        assert_eq!(normalize_page_selector_text(":notapagepseudo"), None);
        assert_eq!(normalize_page_selector_text("named @bad"), None);
    }
}
